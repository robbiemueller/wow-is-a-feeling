//! Fast search over the catalog, re-run on every keystroke.
//!
//! Names are fuzzy matched with nucleo and blended with popularity, so "det" finds
//! Details before something obscure. Folder names, authors and summaries get cheap
//! substring checks at a lower weight. All per-addon work happens in buffers kept
//! between calls.

use std::sync::{Mutex, PoisonError};

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Matcher, Utf32String, chars};

use crate::catalog::{Addon, Catalog};

/// Points for a perfect name match. A rough scale for everything below.
const NAME_QUALITY: i32 = 1000;
/// Extra points for typing all of a name rather than a little of it.
const NAME_COVERAGE: i32 = 120;
/// Points for being the most popular addon in the catalog.
const POPULARITY: i32 = 300;
/// Substring matches sit below good name matches, so popularity can reorder them
/// among themselves but can't lift them over an addon that's actually named that.
const FOLDER_MATCH: u16 = 600;
const AUTHOR_MATCH: u16 = 450;
const SUMMARY_MATCH: u16 = 300;
/// Shorter words appear inside nearly every folder name.
const MIN_FOLDER_MATCH_BYTES: usize = 2;
/// Shorter words appear inside nearly every author and summary.
const MIN_TEXT_MATCH_BYTES: usize = 3;
/// Nobody types more than this, and a pasted novel must not freeze a keystroke.
const MAX_WORDS: usize = 8;

/// How to order the whole catalog when not searching.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Popular,
    Name,
    /// Most recent release on any channel first.
    Updated,
    /// Most recently added first, falling back to the release date when the catalog
    /// doesn't know when an addon appeared.
    Newest,
}

/// Prepared search data for one catalog. Indices it returns point into that
/// catalog's `addons`, so rebuild it when the catalog changes.
pub struct SearchIndex {
    names: Vec<Utf32String>,
    /// Which characters each name contains, to skip the fuzzy matcher cheaply.
    name_masks: Vec<u64>,
    /// 0 to [`POPULARITY`].
    popularity: Vec<u16>,
    folders: FieldText,
    authors: FieldText,
    summaries: FieldText,
    by_popularity: Vec<u32>,
    by_name: Vec<u32>,
    by_updated: Vec<u32>,
    by_newest: Vec<u32>,
    scratch: Mutex<Scratch>,
}

struct Scratch {
    matcher: Matcher,
    /// Score so far per addon. Negative once a word has failed to match it.
    totals: Vec<i32>,
    /// Best substring score per addon, for the current word.
    text_scores: Vec<u16>,
    /// `(negated score, addon)`, so plain sorting gives best first and stays stable.
    ranked: Vec<(i32, u32)>,
}

impl SearchIndex {
    pub fn build(catalog: &Catalog) -> Self {
        let addons = &catalog.addons;
        let log_max = addons
            .iter()
            .map(|a| log_downloads(a.downloads))
            .fold(0.0, f64::max);

        let mut folders = FieldText::with_capacity(addons.len());
        let mut authors = FieldText::with_capacity(addons.len());
        let mut summaries = FieldText::with_capacity(addons.len());
        for addon in addons {
            folders.push(addon.folders.iter().map(String::as_str));
            authors.push([addon.author.as_str()]);
            summaries.push([addon.summary.as_str()]);
        }

        let popularity: Vec<u16> = addons.iter().map(|a| popularity_of(a, log_max)).collect();
        let lower_names: Vec<String> = addons.iter().map(|a| a.name.to_lowercase()).collect();

        let mut by_name: Vec<u32> = (0..addons.len() as u32).collect();
        by_name.sort_by(|&a, &b| lower_names[a as usize].cmp(&lower_names[b as usize]));

        let mut by_popularity = by_name.clone();
        by_popularity.sort_by_key(|&i| {
            let i = i as usize;
            (
                std::cmp::Reverse(popularity[i]),
                std::cmp::Reverse(addons[i].downloads),
            )
        });

        let mut by_updated = by_popularity.clone();
        by_updated.sort_by_key(|&i| std::cmp::Reverse(release_date(&addons[i as usize])));

        let mut by_newest = by_popularity.clone();
        by_newest.sort_by_key(|&i| {
            let addon = &addons[i as usize];
            let first_seen = if addon.created > 0 {
                addon.created
            } else {
                release_date(addon)
            };
            std::cmp::Reverse(first_seen)
        });

        Self {
            names: addons
                .iter()
                .map(|a| Utf32String::from(a.name.as_str()))
                .collect(),
            name_masks: addons.iter().map(|a| name_mask(&a.name)).collect(),
            popularity,
            folders,
            authors,
            summaries,
            by_popularity,
            by_name,
            by_updated,
            by_newest,
            scratch: Mutex::new(Scratch {
                matcher: Matcher::default(),
                totals: Vec::new(),
                text_scores: Vec::new(),
                ranked: Vec::new(),
            }),
        }
    }

    /// The best `limit` matches, best first. Every word must match somewhere. An empty
    /// query returns everything by popularity.
    pub fn query(&self, text: &str, limit: usize) -> Vec<usize> {
        let mut words = text.split_whitespace().take(MAX_WORDS).peekable();
        if words.peek().is_none() {
            return self
                .by_popularity
                .iter()
                .take(limit)
                .map(|&i| i as usize)
                .collect();
        }
        if limit == 0 {
            return Vec::new();
        }

        let mut guard = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        let Scratch {
            matcher,
            totals,
            text_scores,
            ranked,
        } = &mut *guard;
        totals.clear();
        totals.resize(self.names.len(), 0);
        text_scores.resize(self.names.len(), 0);

        let mut word_count = 0;
        for word in words {
            word_count += 1;
            self.score_word(word, matcher, totals, text_scores);
        }

        ranked.clear();
        ranked.extend(
            totals
                .iter()
                .zip(&self.popularity)
                .enumerate()
                .filter(|(_, (total, _))| **total > 0)
                .map(|(i, (total, popularity))| {
                    (-(total / word_count + i32::from(*popularity)), i as u32)
                }),
        );
        if ranked.len() > limit {
            ranked.select_nth_unstable(limit - 1);
            ranked.truncate(limit);
        }
        ranked.sort_unstable();
        ranked.iter().map(|&(_, addon)| addon as usize).collect()
    }

    /// Add one word's score to every addon still in the running.
    fn score_word(
        &self,
        word: &str,
        matcher: &mut Matcher,
        totals: &mut [i32],
        text_scores: &mut [u16],
    ) {
        let lower = word.to_lowercase();
        text_scores.fill(0);
        if lower.len() >= MIN_FOLDER_MATCH_BYTES {
            self.folders.mark(&lower, FOLDER_MATCH, text_scores);
        }
        if lower.len() >= MIN_TEXT_MATCH_BYTES {
            self.authors.mark(&lower, AUTHOR_MATCH, text_scores);
            self.summaries.mark(&lower, SUMMARY_MATCH, text_scores);
        }

        // `new`, not `parse`: `!Bug` is a folder name here, not fzf's "does not contain".
        let atom = Atom::new(
            word,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );
        let needle_len = atom.needle_text().len() as i32;
        let needle_mask = needle_mask(word);
        // nucleo's score for a whole word at a word start, the best it gives. A match at
        // the very start of the name scores no higher, so "Boss" in "Deadly Boss Mods"
        // and in "Boss Timer" tie, and popularity decides between them.
        let best_raw = (26 * needle_len + 10) as f32;

        for (i, total) in totals.iter_mut().enumerate() {
            if *total < 0 {
                continue;
            }
            let name = &self.names[i];
            let name_score = if self.name_masks[i] & needle_mask == needle_mask {
                atom.score(name.slice(..), matcher).map_or(0, |raw| {
                    // Squared, so a scattered match that scores only a little lower than
                    // the typed word itself can't overtake it on popularity alone.
                    let closeness = (f32::from(raw) / best_raw).min(1.0);
                    let quality = (closeness * closeness * NAME_QUALITY as f32) as i32;
                    quality + NAME_COVERAGE * needle_len / (name.len() as i32).max(needle_len)
                })
            } else {
                0
            };
            let best = name_score.max(i32::from(text_scores[i]));
            if best > 0 {
                *total += best;
            } else {
                *total = -1;
            }
        }
    }

    /// Every addon in the given order.
    pub fn sorted(&self, sort: SortBy) -> Vec<usize> {
        let order = match sort {
            SortBy::Popular => &self.by_popularity,
            SortBy::Name => &self.by_name,
            SortBy::Updated => &self.by_updated,
            SortBy::Newest => &self.by_newest,
        };
        order.iter().map(|&i| i as usize).collect()
    }
}

/// One field of every addon, lowercased and laid end to end. A single pass over this
/// costs far less than a separate substring search per addon.
struct FieldText {
    text: String,
    /// Where each addon's text ends in `text`.
    ends: Vec<u32>,
}

impl FieldText {
    fn with_capacity(addons: usize) -> Self {
        Self {
            text: String::new(),
            ends: Vec::with_capacity(addons),
        }
    }

    fn push<'a>(&mut self, parts: impl IntoIterator<Item = &'a str>) {
        for part in parts {
            self.text.push_str(&part.to_lowercase());
            // Words never contain a newline, so a match can't span two parts.
            self.text.push('\n');
        }
        self.ends.push(self.text.len() as u32);
    }

    /// Raise the score of every addon whose text contains `needle`.
    fn mark(&self, needle: &str, score: u16, scores: &mut [u16]) {
        let mut addon = 0;
        for (position, _) in self.text.match_indices(needle) {
            // Hits arrive in order, so the owner is at or just after the last one.
            while self.ends[addon] as usize <= position {
                addon += 1;
            }
            scores[addon] = scores[addon].max(score);
        }
    }
}

fn log_downloads(downloads: u64) -> f64 {
    (downloads as f64).ln_1p()
}

/// An even blend of log downloads and tier. Tier matters on its own because curated
/// source installs such as Details report few or no downloads.
fn popularity_of(addon: &Addon, log_max: f64) -> u16 {
    let downloads = if log_max > 0.0 {
        log_downloads(addon.downloads) / log_max
    } else {
        0.0
    };
    let tier = f64::from(addon.tier.min(5)) / 5.0;
    ((downloads + tier) / 2.0 * f64::from(POPULARITY)) as u16
}

fn release_date(addon: &Addon) -> i64 {
    let date = |release: &Option<crate::catalog::Release>| release.as_ref().map_or(0, |r| r.date);
    date(&addon.stable).max(date(&addon.beta))
}

fn mask_bit(byte: u8) -> u64 {
    let byte = byte.to_ascii_lowercase();
    1 << match byte {
        b'a'..=b'z' => byte - b'a',
        b'0'..=b'9' => byte - b'0' + 26,
        _ => byte % 28 + 36,
    }
}

/// Every ASCII character a name can match, including what accented letters reduce to,
/// because nucleo lets `e` find `é`. A superset is fine: it only has to never say no wrongly.
fn name_mask(name: &str) -> u64 {
    name.chars()
        .map(|c| chars::to_lower_case(chars::normalize(c)))
        .filter(char::is_ascii)
        .fold(0, |mask, c| mask | mask_bit(c as u8))
}

/// The ASCII characters a name must contain to match. Other characters are left to the matcher.
fn needle_mask(word: &str) -> u64 {
    word.bytes()
        .filter(u8::is_ascii)
        .fold(0, |mask, byte| mask | mask_bit(byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Release;

    fn addon(name: &str, downloads: u64, tier: u8) -> Addon {
        Addon {
            id: format!("wowi:{name}"),
            name: name.to_string(),
            downloads,
            tier,
            ..Default::default()
        }
    }

    fn released(mut addon: Addon, date: i64) -> Addon {
        addon.stable = Some(Release {
            date,
            ..Default::default()
        });
        addon
    }

    fn index(addons: Vec<Addon>) -> (Catalog, SearchIndex) {
        let catalog = Catalog {
            addons,
            ..Default::default()
        };
        let index = SearchIndex::build(&catalog);
        (catalog, index)
    }

    fn names<'a>(catalog: &'a Catalog, indices: &[usize]) -> Vec<&'a str> {
        indices
            .iter()
            .map(|&i| catalog.addons[i].name.as_str())
            .collect()
    }

    #[test]
    fn det_finds_details_before_something_obscure() {
        let (catalog, index) = index(vec![
            addon("Detox Helper", 12, 1),
            addon("Determined Fishing", 80, 1),
            addon("Details! Damage Meter", 5_000_000, 5),
            addon("Questie", 9_000_000, 5),
        ]);
        let results = index.query("det", 10);
        assert_eq!(names(&catalog, &results)[0], "Details! Damage Meter");
        assert!(!names(&catalog, &results).contains(&"Questie"));
    }

    /// Well-known addons with made-up but realistic numbers, to catch rankings that
    /// pass on paper and feel wrong.
    fn familiar_catalog() -> (Catalog, SearchIndex) {
        let entry = |name: &str, folders: &[&str], author: &str, summary: &str, downloads, tier| {
            let mut addon = addon(name, downloads, tier);
            addon.folders = folders.iter().map(|f| f.to_string()).collect();
            addon.author = author.to_string();
            addon.summary = summary.to_string();
            addon
        };
        index(vec![
            entry(
                "Details! Damage Meter",
                &["Details"],
                "Terciob",
                "Damage and healing meter.",
                90_000,
                5,
            ),
            entry(
                "Deadly Boss Mods",
                &["DBM-Core", "DBM-GUI"],
                "Tandanu",
                "Timers and warnings for boss fights.",
                8_000_000,
                5,
            ),
            entry(
                "BigWigs Bossmods",
                &["BigWigs", "BigWigs_Core"],
                "Funkeh",
                "Modular, lightweight boss mod.",
                2_000_000,
                5,
            ),
            entry(
                "Questie",
                &["Questie"],
                "Aero",
                "The quest helper for classic servers.",
                7_000_000,
                5,
            ),
            entry(
                "WeakAuras",
                &["WeakAuras", "WeakAurasOptions"],
                "Stanzilla",
                "Powerful framework to display auras.",
                6_000_000,
                5,
            ),
            entry(
                "ElvUI",
                &["ElvUI", "ElvUI_Options"],
                "Elv",
                "A complete user interface replacement.",
                0,
                5,
            ),
            entry(
                "Bagnon",
                &["Bagnon", "Bagnon_Config"],
                "Tuller",
                "Single-window bags.",
                3_000_000,
                4,
            ),
            entry(
                "Auctionator",
                &["Auctionator"],
                "Borjamacare",
                "Simple auction house interface.",
                4_000_000,
                4,
            ),
            entry(
                "Skada Damage Meter",
                &["Skada"],
                "Zarnivoop",
                "Modular damage meter.",
                500_000,
                3,
            ),
            entry(
                "Leatrix Plus",
                &["Leatrix_Plus"],
                "Leatrix",
                "Interface enhancements.",
                1_000_000,
                4,
            ),
            entry(
                "Detox Reminder",
                &["DetoxReminder"],
                "Nobody",
                "Reminds you to cleanse.",
                40,
                1,
            ),
            entry(
                "Better Quest Text",
                &["BetterQuestText"],
                "Nobody",
                "Larger quest log text.",
                300,
                1,
            ),
            entry(
                "Bag Space Counter",
                &["BagSpace"],
                "Nobody",
                "Counts bag slots.",
                90,
                1,
            ),
            entry(
                "Boss Timer Lite",
                &["BossTimerLite"],
                "Nobody",
                "A tiny pull timer.",
                25,
                1,
            ),
            entry(
                "Interface Sounds",
                &["InterfaceSounds"],
                "Nobody",
                "Mutes some sounds.",
                15,
                1,
            ),
        ])
    }

    #[test]
    fn familiar_searches_put_the_expected_addon_first() {
        let (catalog, index) = familiar_catalog();
        for (query, expected) in [
            ("det", "Details! Damage Meter"),
            ("details", "Details! Damage Meter"),
            ("dbm", "Deadly Boss Mods"),
            ("deadly", "Deadly Boss Mods"),
            ("boss", "Deadly Boss Mods"),
            ("bigwigs", "BigWigs Bossmods"),
            ("quest", "Questie"),
            ("questie", "Questie"),
            ("weak", "WeakAuras"),
            ("wa", "WeakAuras"),
            ("elv", "ElvUI"),
            ("bag", "Bagnon"),
            ("auc", "Auctionator"),
            ("leatrix", "Leatrix Plus"),
            ("damage meter", "Details! Damage Meter"),
            ("skada", "Skada Damage Meter"),
            // By folder, author and summary.
            ("dbm-core", "Deadly Boss Mods"),
            ("tuller", "Bagnon"),
            ("funkeh", "BigWigs Bossmods"),
            ("cleanse", "Detox Reminder"),
        ] {
            let results = index.query(query, 5);
            let results = names(&catalog, &results);
            assert_eq!(
                results.first().copied(),
                Some(expected),
                "{query:?}: {results:?}"
            );
        }
    }

    #[test]
    fn tier_lifts_popular_addons_with_no_reported_downloads() {
        let (catalog, index) = index(vec![
            addon("Detox Helper", 500, 1),
            addon("Details", 0, 5),
            addon("Questie", 1_000_000, 5),
        ]);
        assert_eq!(names(&catalog, &index.query("det", 1)), ["Details"]);
    }

    #[test]
    fn a_full_name_beats_a_more_popular_near_miss() {
        let (catalog, index) = index(vec![
            addon("Quest Items Everywhere", 9_000_000, 5),
            addon("Questie", 50_000, 3),
            addon("Questie Extras", 90_000, 3),
        ]);
        let results = index.query("questie", 10);
        assert_eq!(names(&catalog, &results)[0], "Questie");
        assert_eq!(names(&catalog, &results)[1], "Questie Extras");
    }

    #[test]
    fn matching_ignores_case_and_accents() {
        let (catalog, index) = index(vec![addon("Détails Écran", 1, 1), addon("Plain", 1, 1)]);
        assert_eq!(
            names(&catalog, &index.query("DETAILS", 5)),
            ["Détails Écran"]
        );
        assert_eq!(names(&catalog, &index.query("ecran", 5)), ["Détails Écran"]);
    }

    #[test]
    fn non_ascii_queries_work() {
        let (catalog, index) = index(vec![
            addon("Квестовый помощник", 1, 1),
            addon("Plain", 1, 1),
        ]);
        assert_eq!(
            names(&catalog, &index.query("квест", 5)),
            ["Квестовый помощник"]
        );
        assert!(index.query("日本語", 5).is_empty());
    }

    #[test]
    fn leading_bangs_and_other_fzf_syntax_are_plain_text() {
        let mut bug = addon("BugGrabber", 10, 2);
        bug.folders = vec!["!BugGrabber".to_string()];
        let (catalog, index) = index(vec![bug, addon("Bangers", 10, 2)]);
        assert_eq!(names(&catalog, &index.query("!bug", 5)), ["BugGrabber"]);
        assert!(index.query("^bug$", 5).is_empty());
        assert!(index.query("'bug", 5).is_empty());
    }

    #[test]
    fn folder_author_and_summary_match_at_decreasing_weight() {
        let mut in_folder = addon("One", 100, 2);
        in_folder.folders = vec!["Zarquon-Core".to_string()];
        let mut in_author = addon("Two", 100, 2);
        in_author.author = "ZARQUON".to_string();
        let mut in_summary = addon("Three", 100, 2);
        in_summary.summary = "Inspired by Zarquon himself.".to_string();
        let in_name = addon("Zarquon", 100, 2);
        let (catalog, index) = index(vec![
            in_summary,
            in_author,
            in_folder,
            in_name,
            addon("Four", 100, 2),
        ]);
        assert_eq!(
            names(&catalog, &index.query("zarquon", 10)),
            ["Zarquon", "One", "Two", "Three"]
        );
    }

    #[test]
    fn popularity_cannot_lift_a_summary_mention_over_a_name_match() {
        let mut mention = addon("Everything Meter", 9_000_000, 5);
        mention.summary = "Works great with zarquon".to_string();
        let (catalog, index) = index(vec![mention, addon("Zarquon", 3, 1)]);
        assert_eq!(names(&catalog, &index.query("zarquon", 2))[0], "Zarquon");
    }

    #[test]
    fn every_word_must_match_but_can_match_different_fields() {
        let mut dbm = addon("Deadly Boss Mods", 100, 5);
        dbm.folders = vec!["DBM-Core".to_string()];
        dbm.author = "Tandanu".to_string();
        let mut other = addon("Deadly Poison Timer", 100, 1);
        other.folders = vec!["DPT".to_string()];
        let (catalog, index) = index(vec![other, dbm]);

        assert_eq!(
            names(&catalog, &index.query("deadly core", 5)),
            ["Deadly Boss Mods"]
        );
        assert_eq!(
            names(&catalog, &index.query("boss tandanu", 5)),
            ["Deadly Boss Mods"]
        );
        assert_eq!(
            names(&catalog, &index.query("  deadly   timer ", 5)),
            ["Deadly Poison Timer"]
        );
        assert!(index.query("deadly zzzz", 5).is_empty());
    }

    #[test]
    fn short_words_do_not_match_summaries_or_authors() {
        let mut noisy = addon("Noisy", 100, 1);
        noisy.summary = "ab ab ab".to_string();
        noisy.author = "ab".to_string();
        let (_, index) = index(vec![noisy]);
        assert!(index.query("ab", 5).is_empty());
    }

    #[test]
    fn a_single_letter_does_not_match_folder_names() {
        let mut addon = addon("Noisy", 100, 1);
        addon.folders = vec!["Xylophone".to_string()];
        let (_, index) = index(vec![addon]);
        assert!(index.query("x", 5).is_empty());
        assert_eq!(index.query("xy", 5), [0]);
    }

    #[test]
    fn matches_do_not_span_two_folders() {
        let mut split = addon("Split", 1, 1);
        split.folders = vec!["foo".to_string(), "bar".to_string()];
        let (_, index) = index(vec![split]);
        assert!(index.query("foobar", 5).is_empty());
        assert_eq!(index.query("foo", 5), [0]);
        assert_eq!(index.query("bar", 5), [0]);
    }

    #[test]
    fn hits_land_on_the_right_addon_when_fields_are_empty_or_repeat() {
        let mut first = addon("A", 1, 1);
        first.summary = "needle needle needle".to_string();
        let mut third = addon("C", 1, 1);
        third.summary = "a needle".to_string();
        let mut last = addon("E", 1, 1);
        last.summary = "needle".to_string();
        let (_, index) = index(vec![first, addon("B", 1, 1), third, addon("D", 1, 1), last]);
        let mut hits = index.query("needle", 10);
        hits.sort_unstable();
        assert_eq!(hits, [0, 2, 4]);
    }

    #[test]
    fn empty_and_blank_queries_return_everything_by_popularity() {
        let (catalog, index) = index(vec![
            addon("Small", 10, 1),
            addon("Big", 1_000_000, 5),
            addon("Middle", 5_000, 3),
        ]);
        for text in ["", "   ", "\t\n"] {
            assert_eq!(
                names(&catalog, &index.query(text, 10)),
                ["Big", "Middle", "Small"]
            );
        }
        assert_eq!(names(&catalog, &index.query("", 2)), ["Big", "Middle"]);
    }

    #[test]
    fn limit_is_respected_and_keeps_the_best() {
        let addons = (0..100)
            .map(|i| addon(&format!("Quest Thing {i}"), i * 1000, 2))
            .collect();
        let (catalog, index) = index(addons);
        let top = index.query("quest", 3);
        assert_eq!(top.len(), 3);
        assert_eq!(names(&catalog, &top)[0], "Quest Thing 99");
        assert!(index.query("quest", 0).is_empty());
        assert!(index.query("", 0).is_empty());
        assert_eq!(index.query("quest", 1000).len(), 100);
    }

    #[test]
    fn repeated_queries_give_the_same_answer() {
        let (_, index) = index(vec![addon("Details", 1, 1), addon("Questie", 1, 1)]);
        let first = index.query("det", 5);
        assert_eq!(index.query("questie", 5), [1]);
        assert_eq!(index.query("det", 5), first);
        assert_eq!(index.query("det questie", 5), Vec::<usize>::new());
        assert_eq!(index.query("det", 5), first);
    }

    #[test]
    fn an_empty_catalog_is_fine() {
        let (_, index) = index(Vec::new());
        assert!(index.query("anything", 10).is_empty());
        assert!(index.query("", 10).is_empty());
        assert!(index.sorted(SortBy::Name).is_empty());
    }

    #[test]
    fn nameless_addons_and_odd_input_do_not_panic() {
        let (_, index) = index(vec![addon("", 0, 0), addon("x", 0, 200)]);
        assert_eq!(index.query("x", 5), [1]);
        for text in [
            "|||",
            "\u{0}",
            "🙂",
            "e\u{301}",
            "\\ \\",
            &"long".repeat(5000),
            &"a ".repeat(5000),
        ] {
            let _ = index.query(text, 5);
        }
    }

    #[test]
    fn sorted_orders_the_whole_catalog() {
        let (catalog, index) = index(vec![
            released(addon("banana", 100, 2), 300),
            released(addon("Apple", 1_000_000, 5), 100),
            released(addon("cherry", 5_000, 3), 200),
            addon("Date", 0, 0),
        ]);
        let order = |sort| names(&catalog, &index.sorted(sort));
        assert_eq!(
            order(SortBy::Popular),
            ["Apple", "cherry", "banana", "Date"]
        );
        assert_eq!(order(SortBy::Name), ["Apple", "banana", "cherry", "Date"]);
        assert_eq!(
            order(SortBy::Updated),
            ["banana", "cherry", "Apple", "Date"]
        );
        assert_eq!(order(SortBy::Newest), ["banana", "cherry", "Apple", "Date"]);
    }

    #[test]
    fn updated_counts_beta_releases() {
        let mut with_beta = released(addon("Beta Fan", 1, 1), 100);
        with_beta.beta = Some(Release {
            date: 900,
            ..Default::default()
        });
        let (catalog, index) = index(vec![released(addon("Steady", 1, 1), 500), with_beta]);
        assert_eq!(
            names(&catalog, &index.sorted(SortBy::Updated)),
            ["Beta Fan", "Steady"]
        );
    }

    #[test]
    fn newest_prefers_when_the_addon_appeared() {
        let mut veteran = released(addon("Veteran", 1, 1), 900);
        veteran.created = 100;
        let mut newcomer = released(addon("Newcomer", 1, 1), 500);
        newcomer.created = 800;
        let unknown = released(addon("Unknown", 1, 1), 600);
        let (catalog, index) = index(vec![veteran, newcomer, unknown]);
        assert_eq!(
            names(&catalog, &index.sorted(SortBy::Newest)),
            ["Newcomer", "Unknown", "Veteran"]
        );
        assert_eq!(
            names(&catalog, &index.sorted(SortBy::Updated)),
            ["Veteran", "Unknown", "Newcomer"]
        );
    }

    #[test]
    fn ties_in_a_date_sort_fall_back_to_popularity() {
        let (catalog, index) = index(vec![
            released(addon("Small", 1, 1), 100),
            released(addon("Large", 1_000, 5), 100),
        ]);
        assert_eq!(
            names(&catalog, &index.sorted(SortBy::Updated)),
            ["Large", "Small"]
        );
    }

    #[test]
    fn the_character_masks_never_reject_a_real_match() {
        let (catalog, index) = index(vec![
            addon("Äpfel Ünï-Code_99", 1, 1),
            addon("With (Punct) & More!", 1, 1),
        ]);
        for (query, name) in [
            ("apfel", "Äpfel Ünï-Code_99"),
            ("uni", "Äpfel Ünï-Code_99"),
            ("code_99", "Äpfel Ünï-Code_99"),
            ("(punct)", "With (Punct) & More!"),
            ("&", "With (Punct) & More!"),
            ("MORE!", "With (Punct) & More!"),
        ] {
            assert_eq!(
                names(&catalog, &index.query(query, 5)),
                [name],
                "query {query:?}"
            );
        }
    }

    #[test]
    fn the_index_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SearchIndex>();
    }

    /// Words a real catalog is made of, so match counts look like real life.
    const WORDS: [&str; 48] = [
        "Quest",
        "Boss",
        "Mods",
        "Deadly",
        "Details",
        "Damage",
        "Meter",
        "Bag",
        "Bank",
        "Auction",
        "House",
        "Map",
        "Mini",
        "Tracker",
        "Helper",
        "Raid",
        "Frame",
        "Unit",
        "Nameplate",
        "Aura",
        "Weak",
        "Tooltip",
        "Chat",
        "Social",
        "Guild",
        "Loot",
        "Master",
        "Profession",
        "Gather",
        "Herb",
        "Mining",
        "Fishing",
        "Combat",
        "Log",
        "Threat",
        "Heal",
        "Timer",
        "Cooldown",
        "Action",
        "Bar",
        "Interface",
        "Skin",
        "Dungeon",
        "Finder",
        "Group",
        "Pet",
        "Mount",
        "Lite",
    ];

    /// A deterministic catalog of `count` addons with realistic field lengths.
    fn synthetic_catalog(count: usize) -> Catalog {
        let mut seed: u64 = 0x2026_1107;
        let mut next = move |bound: usize| {
            // Knuth's multiplicative step: plenty for test data, and it needs no crate.
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % bound
        };
        let addons = (0..count)
            .map(|i| {
                let name_words = 2 + next(3);
                let name = (0..name_words)
                    .map(|_| WORDS[next(WORDS.len())])
                    .collect::<Vec<_>>()
                    .join(" ");
                let folder = format!("{}{i}", name.replace(' ', ""));
                let summary = (0..18)
                    .map(|_| WORDS[next(WORDS.len())].to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" ");
                Addon {
                    id: format!("wowi:{i}"),
                    name,
                    author: format!("{}{}", WORDS[next(WORDS.len())], next(1000)),
                    summary,
                    folders: vec![folder.clone(), format!("{folder}_Options")],
                    downloads: next(5_000_000) as u64,
                    tier: next(6) as u8,
                    stable: Some(Release {
                        date: next(1_000_000) as i64,
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            })
            .collect();
        Catalog {
            addons,
            ..Default::default()
        }
    }

    /// Run with `cargo test --release -p feeling-core -- --nocapture search_20k`
    /// to see real numbers. Each query's best of several runs is what counts, because
    /// other builds share this machine.
    #[test]
    fn search_20k_addons_within_budget() {
        let catalog = synthetic_catalog(20_000);
        let build_start = std::time::Instant::now();
        let index = SearchIndex::build(&catalog);
        println!("build: {:?}", build_start.elapsed());

        let budget_ms = if cfg!(debug_assertions) { 150 } else { 10 };
        let mut worst = std::time::Duration::ZERO;
        for query in [
            "d",
            "det",
            "quest",
            "boss mods",
            "deadly boss mods",
            "tracker helper raid",
            "interfaceskin",
            "dungeonfinderlite",
            "zzzzqq",
            "e",
            "the",
            "ae",
        ] {
            let best = (0..9)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let results = index.query(query, 200);
                    let elapsed = start.elapsed();
                    std::hint::black_box(results);
                    elapsed
                })
                .min()
                .unwrap();
            println!(
                "{query:>22}: {best:?}, {} results",
                index.query(query, usize::MAX).len()
            );
            worst = worst.max(best);
        }
        println!("worst query: {worst:?} (budget {budget_ms} ms)");
        assert!(
            worst.as_millis() < budget_ms,
            "worst query took {worst:?}, budget is {budget_ms} ms"
        );
    }
}
