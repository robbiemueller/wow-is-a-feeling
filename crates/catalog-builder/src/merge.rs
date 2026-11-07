//! Combines the sources into one list and settles who owns what. Anyone with a free
//! GitHub account gets looked at automatically, and everything in their repository is
//! theirs to write: its title, folders, TOC ids and download counts prove nothing. So
//! identity is first come, oldest wins, and whatever a person picked by hand beats all
//! of it. See the Catalog section of `docs/DESIGN.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use feeling_core::catalog::{Catalog, Source};

use crate::curated::{Curated, HandPicked};
use crate::detect::FOREVER_EPOCH;
use crate::github::RepoInfo;
use crate::model::{Candidate, Held};
use crate::text::{fold_folder, fold_name, normalize, tokens};
use crate::tier::popularity;

/// The age of everything that was already listed when ages started being kept. Older
/// than the game itself, so those entries are established from the first build on.
pub const BASELINE_CREATED: i64 = FOREVER_EPOCH;
/// How long an entry must keep its name and main folder before the app may pick it by itself.
const TRUST_AFTER_SECS: i64 = 14 * 24 * 60 * 60;
/// Entries an owner can have without being hand-picked. The most any real owner has today
/// is 21 listed out of 38 known. This is what makes a flood of repositories cost something.
pub const MAX_ENTRIES_PER_OWNER: usize = 40;

/// Why a repository isn't worth looking at, or `None` if it is.
pub fn repo_rejection(info: &RepoInfo, hand_picked: bool) -> Option<&'static str> {
    if info.disabled {
        return Some("disabled");
    }
    if info.empty {
        return Some("empty");
    }
    if info.archived && !hand_picked {
        return Some("archived");
    }
    if hand_picked {
        return None;
    }
    if info.template {
        return Some("template");
    }
    if info.mirror {
        return Some("mirror");
    }
    let repo_name = info.name.split('/').nth(1).unwrap_or(&info.name);
    let words = tokens(repo_name);
    if words.iter().any(|w| {
        matches!(
            w.as_str(),
            "mirror" | "template" | "boilerplate" | "skeleton" | "scaffold"
        )
    }) {
        return Some("template or mirror by name");
    }
    None
}

/// Placeholder titles that mean nobody finished setting up the addon.
pub fn is_placeholder_name(name: &str) -> bool {
    matches!(
        normalize(name).as_str(),
        "" | "myaddon"
            | "addon"
            | "addonname"
            | "newaddon"
            | "youraddon"
            | "addontemplate"
            | "template"
            | "test"
            | "testaddon"
            | "example"
            | "exampleaddon"
            | "sample"
            | "sampleaddon"
            | "helloworld"
            | "untitled"
            | "foo"
            | "bar"
    )
}

/// Forks exist for many reasons, but one that installs the same folders as the repo it
/// came from adds nothing but confusion, and its downloads go to the wrong place. The
/// same goes for a lesser-known sibling. Any other repo with the same folders proves
/// nothing, because anyone can make one, and something picked by hand always stays.
pub fn drop_redundant_forks(
    candidates: Vec<Candidate>,
    picks: &HandPicked,
) -> (Vec<Candidate>, Vec<String>) {
    let sets: BTreeMap<String, BTreeSet<String>> = candidates
        .iter()
        .map(|c| (c.addon.id.to_ascii_lowercase(), c.folder_set()))
        .collect();
    let forks: Vec<(String, String, BTreeSet<String>, u32)> = candidates
        .iter()
        .filter_map(|c| {
            let parent = c.fork_of.as_ref().filter(|parent| !parent.is_empty())?;
            Some((
                c.addon.id.clone(),
                parent.to_ascii_lowercase(),
                c.folder_set(),
                c.addon.stars,
            ))
        })
        .collect();

    let mut kept = Vec::with_capacity(candidates.len());
    let mut dropped = Vec::new();
    for candidate in candidates {
        let redundant = !picks.contains(&candidate.addon.id)
            && candidate.fork_of.as_ref().is_some_and(|parent| {
                let set = candidate.folder_set();
                let parent = parent.to_ascii_lowercase();
                let same_as_parent = sets
                    .get(&format!("gh:{parent}"))
                    .is_some_and(|parent_set| *parent_set == set);
                let same_as_better_sibling =
                    forks.iter().any(|(id, other_parent, other, stars)| {
                        *id != candidate.addon.id
                            && *other_parent == parent
                            && *other == set
                            && *stars > candidate.addon.stars
                    });
                same_as_parent || same_as_better_sibling
            });
        if redundant {
            dropped.push(candidate.addon.id);
        } else {
            kept.push(candidate);
        }
    }
    (kept, dropped)
}

/// Fold WoWInterface entries into the GitHub repositories they're approved for in
/// `catalog/curated.toml`. GitHub wins because its builds are versioned and carry digests,
/// and the WoWI id is kept so an addon installed from there can still be matched. Nothing
/// else merges: a repository can claim any id and copy any folder, so a TOC id or
/// matching folders never remove a listing or move its counts.
pub fn merge_wowi(
    mut github: Vec<Candidate>,
    mut wowi: Vec<Candidate>,
    approved: &BTreeMap<String, String>,
) -> (Vec<Candidate>, usize) {
    // In id order, so two listings approved for one repository settle the same way every run.
    wowi.sort_by_key(|entry| listing_number(entry.addon.ids.wowi.as_deref()));
    let positions: HashMap<String, usize> = github
        .iter()
        .enumerate()
        .map(|(at, g)| (g.addon.id.to_ascii_lowercase(), at))
        .collect();
    let mut merged_count = 0;
    let mut standalone = Vec::new();
    for entry in wowi {
        let id = entry.addon.ids.wowi.clone().unwrap_or_default();
        let repo = approved.get(&id);
        let target = repo.and_then(|repo| {
            positions
                .get(&format!("gh:{}", repo.to_ascii_lowercase()))
                .copied()
        });
        match (target, repo) {
            (Some(at), Some(repo)) => {
                let target = &mut github[at];
                // The id the repository's own TOC gives stays when that pair is approved too.
                let own_id_is_approved = target
                    .addon
                    .ids
                    .wowi
                    .as_ref()
                    .and_then(|own| approved.get(own))
                    .is_some_and(|other| other.eq_ignore_ascii_case(repo));
                if !own_id_is_approved {
                    target.addon.ids.wowi = Some(id);
                }
                target.addon.downloads += entry.addon.downloads;
                target.favorites += entry.favorites;
                if target.addon.summary.is_empty() {
                    target.addon.summary = entry.addon.summary;
                }
                if target.wowi_category.is_none() {
                    target.wowi_category = entry.wowi_category;
                }
                merged_count += 1;
            }
            _ => standalone.push(entry),
        }
    }
    github.append(&mut standalone);
    (github, merged_count)
}

fn listing_number(id: Option<&str>) -> u64 {
    id.and_then(|id| id.parse().ok()).unwrap_or(0)
}

/// Whether an entry still is what the last catalog listed: same name to a reader's eye,
/// same main folder.
fn same_identity(old_name: &str, old_folders: &[String], candidate: &Candidate) -> bool {
    let main = |folders: &[String]| folders.first().map(|folder| fold_folder(folder));
    fold_name(old_name) == fold_name(&candidate.addon.name)
        && main(old_folders) == main(&candidate.addon.folders)
}

/// Set when each entry first appeared, from the last published catalog. An entry not
/// in it is new. So is one that changed its name or main folder, unless someone picked it
/// by hand, so a repository can't get established as one thing and then turn into another.
pub fn stamp_ages(
    candidates: &mut [Candidate],
    previous: Option<&Catalog>,
    picks: &HandPicked,
    now: i64,
) {
    let listed: HashMap<String, (&str, &[String], i64)> = previous
        .map(|catalog| catalog.addons.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|addon| {
            (
                addon.id.to_ascii_lowercase(),
                (addon.name.as_str(), addon.folders.as_slice(), addon.created),
            )
        })
        .collect();
    for candidate in candidates {
        let known = listed.get(&candidate.addon.id.to_ascii_lowercase());
        candidate.addon.created = match known {
            Some((name, folders, created))
                if picks.contains(&candidate.addon.id)
                    || same_identity(name, folders, candidate) =>
            {
                if *created == 0 {
                    BASELINE_CREATED
                } else {
                    *created
                }
            }
            _ => now,
        };
    }
}

/// Names and folders only something hand-picked may claim: well-known addons that have no
/// entry yet, whose names a stranger could otherwise take for free.
#[derive(Debug, Default)]
pub struct Reserved {
    names: HashMap<String, String>,
    folders: HashMap<String, String>,
}

impl Reserved {
    pub fn new(curated: &Curated) -> Self {
        let mut reserved = Self::default();
        for gap in &curated.gaps {
            reserved
                .names
                .insert(fold_name(&gap.name), gap.name.clone());
        }
        let folders = curated
            .gaps
            .iter()
            .flat_map(|gap| gap.folders.iter())
            .chain(curated.reserved.iter());
        for folder in folders {
            // A title that reads like the folder passes for the addon just as well.
            reserved
                .names
                .entry(fold_name(folder))
                .or_insert_with(|| folder.clone());
            reserved.folders.insert(fold_folder(folder), folder.clone());
        }
        reserved
    }

    fn problem(&self, candidate: &Candidate) -> Option<String> {
        if let Some(name) = self.names.get(&fold_name(&candidate.addon.name)) {
            return Some(format!(
                "its name is reserved for the real {name}, which needs picking by hand"
            ));
        }
        let folder = candidate
            .addon
            .folders
            .iter()
            .find_map(|folder| self.folders.get(&fold_folder(folder)));
        folder.map(|folder| {
            format!(
                "its folder {folder} is reserved for the real addon, which needs picking by hand"
            )
        })
    }
}

/// What the collision pass goes by.
pub struct ClaimRules<'a> {
    pub picks: &'a HandPicked,
    pub reserved: &'a Reserved,
    pub previous: Option<&'a Catalog>,
}

fn is_baseline(candidate: &Candidate) -> bool {
    candidate.addon.created == BASELINE_CREATED
}

fn is_repo(candidate: &Candidate) -> bool {
    candidate.addon.id.starts_with("gh:")
}

fn is_listing(candidate: &Candidate) -> bool {
    matches!(candidate.addon.source, Source::Wowi { .. })
}

/// The owner part of a GitHub id, in lower case.
fn owner_of(candidate: &Candidate) -> Option<String> {
    let repo = candidate.addon.id.strip_prefix("gh:")?;
    Some(repo.split('/').next()?.to_ascii_lowercase())
}

/// Who has claimed what so far. Candidates go through it best first.
#[derive(Default)]
struct Ledger {
    kept: Vec<Candidate>,
    names: HashMap<String, Vec<usize>>,
    folders: HashMap<String, Vec<usize>>,
    per_owner: HashMap<String, usize>,
}

impl Ledger {
    fn keep(&mut self, candidate: Candidate, picks: &HandPicked) {
        let at = self.kept.len();
        let name = fold_name(&candidate.addon.name);
        if !name.is_empty() {
            self.names.entry(name).or_default().push(at);
        }
        for folder in &candidate.addon.folders {
            self.folders
                .entry(fold_folder(folder))
                .or_default()
                .push(at);
        }
        if let Some(owner) = owner_of(&candidate)
            && !picks.contains(&candidate.addon.id)
        {
            *self.per_owner.entry(owner).or_default() += 1;
        }
        self.kept.push(candidate);
    }

    /// Why this candidate can't have what it claims, given everything ahead of it.
    fn conflict(&self, candidate: &Candidate, picks: &HandPicked) -> Option<String> {
        let picked = picks.contains(&candidate.addon.id);
        // Entries that were already side by side stay that way, and so do two a person chose.
        let tied = |other: &Candidate| {
            (is_baseline(other) && is_baseline(candidate))
                || (picked && picks.contains(&other.addon.id))
        };
        let main = |c: &Candidate| c.addon.folders.first().map(|folder| fold_folder(folder));

        let name = fold_name(&candidate.addon.name);
        for other in self.at(self.names.get(&name)) {
            if !tied(other) {
                return Some(format!("its name is already taken by {}", other.addon.id));
            }
            // Only repositories compete as exact copies, as they always have.
            if is_repo(other) && is_repo(candidate) && main(other) == main(candidate) {
                return Some(format!(
                    "it's a copy of {}, with the same name and main folder",
                    other.addon.id
                ));
            }
        }
        let mut folders = candidate.addon.folders.iter();
        if let Some(main_folder) = folders.next() {
            let claimed = self.at(self.folders.get(&fold_folder(main_folder)));
            if let Some(other) = claimed.into_iter().find(|other| !tied(other)) {
                return Some(format!(
                    "its main folder {main_folder} is already taken by {}",
                    other.addon.id
                ));
            }
        }
        for folder in folders {
            let claimed = self.at(self.folders.get(&fold_folder(folder)));
            let ahead = claimed.into_iter().find(|other| {
                !tied(other)
                    && ((picks.contains(&other.addon.id) && !picked)
                        || other.addon.created < candidate.addon.created)
            });
            if let Some(other) = ahead {
                return Some(format!(
                    "its folder {folder} is already taken by {}",
                    other.addon.id
                ));
            }
        }
        if !picked
            && let Some(owner) = owner_of(candidate)
            && self.per_owner.get(&owner).copied().unwrap_or(0) >= MAX_ENTRIES_PER_OWNER
        {
            return Some(format!(
                "{owner} already has {MAX_ENTRIES_PER_OWNER} addons listed"
            ));
        }
        None
    }

    fn at(&self, places: Option<&Vec<usize>>) -> Vec<&Candidate> {
        places
            .into_iter()
            .flatten()
            .map(|at| &self.kept[*at])
            .collect()
    }
}

/// Which folders the last catalog listed, and for whom.
#[derive(Default)]
struct LastCatalog<'a> {
    folders_of: HashMap<String, HashSet<String>>,
    owners_of: HashMap<String, Vec<&'a str>>,
}

impl<'a> LastCatalog<'a> {
    fn new(previous: Option<&'a Catalog>) -> Self {
        let mut last = Self::default();
        for addon in previous.map_or(&[][..], |catalog| catalog.addons.as_slice()) {
            let folders: HashSet<String> = addon.folders.iter().map(|f| fold_folder(f)).collect();
            for folder in &folders {
                last.owners_of
                    .entry(folder.clone())
                    .or_default()
                    .push(&addon.id);
            }
            last.folders_of
                .insert(addon.id.to_ascii_lowercase(), folders);
        }
        last
    }

    /// For an entry that was already listed: a folder it has only now added, which another
    /// listed entry already had. That claim dates from today, however old the entry is, or
    /// a long-listed repository could push out anything younger by shipping its folder.
    fn added_a_taken_folder(
        &self,
        candidate: &Candidate,
        present: &HashSet<String>,
    ) -> Option<String> {
        let id = candidate.addon.id.to_ascii_lowercase();
        let had = self.folders_of.get(&id)?;
        for folder in &candidate.addon.folders {
            let folded = fold_folder(folder);
            if had.contains(&folded) {
                continue;
            }
            let owner = self.owners_of.get(&folded).and_then(|owners| {
                owners.iter().find(|owner| {
                    let owner = owner.to_ascii_lowercase();
                    owner != id && present.contains(&owner)
                })
            });
            if let Some(owner) = owner {
                return Some(format!(
                    "it just added the folder {folder}, which {owner} already lists"
                ));
            }
        }
        None
    }
}

/// Settle who gets what. Candidates go in order, hand-picked first and then oldest, each
/// claiming its name and folders. One whose name or main folder is already taken is held
/// out of the catalog, and so is one with a side folder taken by something strictly ahead
/// of it, or a reserved name or folder, or too many siblings from one owner. What was already
/// listed together stays that way, so this only ever affects newcomers and changes.
pub fn hold_collisions(
    candidates: Vec<Candidate>,
    rules: &ClaimRules<'_>,
) -> (Vec<Candidate>, Vec<Held>) {
    let picks = rules.picks;
    let present: HashSet<String> = candidates
        .iter()
        .map(|c| c.addon.id.to_ascii_lowercase())
        .collect();
    let mut ordered = candidates;
    ordered.sort_by(|a, b| {
        // Among two entries new in the same build, a moderated listing goes before a repository.
        let key = |c: &Candidate| {
            (
                !picks.contains(&c.addon.id),
                c.addon.created,
                !is_baseline(c) && !is_listing(c),
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| popularity(b, 0.0).total_cmp(&popularity(a, 0.0)))
            .then_with(|| a.addon.id.cmp(&b.addon.id))
    });

    let last = LastCatalog::new(rules.previous);
    let mut ledger = Ledger::default();
    let mut held = Vec::new();
    for candidate in ordered {
        let problem = (!picks.contains(&candidate.addon.id))
            .then(|| {
                let reserved = rules.reserved.problem(&candidate);
                reserved.or_else(|| last.added_a_taken_folder(&candidate, &present))
            })
            .flatten()
            .or_else(|| ledger.conflict(&candidate, picks));
        match problem {
            Some(reason) => held.push(Held::new(&candidate.addon, reason)),
            None => ledger.keep(candidate, picks),
        }
    }
    held.sort_by(|a, b| a.id.cmp(&b.id));
    (ledger.kept, held)
}

/// Decide what the app may pick by itself: anything hand-picked, a WoWInterface listing, or
/// anything that has kept its name for two weeks. Everything else is a stranger's new repo
/// and only gets installed when a player asks for it, and its ids on other sites are only
/// claims, so they go. Among what's left, an id belongs to whoever got there first.
/// `candidates` must be in the collision pass's order.
pub fn settle_trust(
    candidates: &mut [Candidate],
    picks: &HandPicked,
    approved: &BTreeMap<String, String>,
    now: i64,
) {
    let present: HashSet<String> = candidates
        .iter()
        .map(|c| c.addon.id.to_ascii_lowercase())
        .collect();
    let mut wowi_owner: HashMap<String, String> = HashMap::new();
    for candidate in candidates.iter() {
        if let Source::Wowi { id } = &candidate.addon.source {
            wowi_owner.insert(id.to_string(), candidate.addon.id.to_ascii_lowercase());
        }
    }
    for (id, repo) in approved {
        let repo = format!("gh:{}", repo.to_ascii_lowercase());
        if present.contains(&repo) {
            wowi_owner.insert(id.clone(), repo);
        }
    }
    let mut curse_owner: HashMap<String, String> = HashMap::new();
    let mut wago_owner: HashMap<String, String> = HashMap::new();

    for candidate in candidates.iter_mut() {
        let addon = &mut candidate.addon;
        addon.trusted = picks.contains(&addon.id)
            || matches!(addon.source, Source::Wowi { .. })
            || now - addon.created >= TRUST_AFTER_SECS;
        if !addon.trusted {
            addon.ids = Default::default();
            continue;
        }
        let me = addon.id.to_ascii_lowercase();
        let keep = |owners: &mut HashMap<String, String>, id: &mut Option<String>| {
            let first = id
                .as_ref()
                .is_some_and(|id| *owners.entry(id.clone()).or_insert_with(|| me.clone()) == me);
            if !first {
                *id = None;
            }
        };
        keep(&mut wowi_owner, &mut addon.ids.wowi);
        keep(&mut curse_owner, &mut addon.ids.curse);
        keep(&mut wago_owner, &mut addon.ids.wago);
    }
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::{Addon, ExternalIds};

    use super::*;
    use crate::curated::{GapSpec, Seeds};

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 24 * 60 * 60;

    fn gh(repo: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("gh:{repo}");
        c.addon.name = repo.split('/').nth(1).unwrap().into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Github { repo: repo.into() };
        c
    }

    fn named(mut candidate: Candidate, name: &str) -> Candidate {
        candidate.addon.name = name.into();
        candidate
    }

    fn wowi(id: u64, name: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("wowi:{id}");
        c.addon.name = name.into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Wowi { id };
        c.addon.ids = ExternalIds {
            wowi: Some(id.to_string()),
            ..ExternalIds::default()
        };
        c
    }

    fn approved(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, repo)| ((*id).to_string(), (*repo).to_string()))
            .collect()
    }

    /// `seeds` is what's hand-picked: `owner/repo`, or a number for a WoWInterface listing.
    fn picks(seeds: &[&str]) -> HandPicked {
        let seeds = Seeds {
            repos: seeds
                .iter()
                .filter(|s| s.contains('/'))
                .map(|s| (*s).to_string())
                .collect(),
            wowi: seeds.iter().filter_map(|s| s.parse().ok()).collect(),
        };
        HandPicked::new(&seeds, &Curated::default())
    }

    fn ids(candidates: &[Candidate]) -> Vec<&str> {
        let mut ids: Vec<&str> = candidates.iter().map(|c| c.addon.id.as_str()).collect();
        ids.sort_unstable();
        ids
    }

    fn held_ids(held: &[Held]) -> Vec<&str> {
        held.iter().map(|h| h.id.as_str()).collect()
    }

    /// The catalog as it was published some builds ago: `created` 0 is one from before ages.
    fn published(entries: &[(&Candidate, i64)]) -> Catalog {
        Catalog {
            addons: entries
                .iter()
                .map(|(c, created)| Addon {
                    created: *created,
                    ..c.addon.clone()
                })
                .collect(),
            ..Catalog::default()
        }
    }

    /// Ages, then the collision pass, the same way a build does it.
    fn pass(
        mut list: Vec<Candidate>,
        chosen: &HandPicked,
        previous: Option<&Catalog>,
    ) -> (Vec<Candidate>, Vec<Held>) {
        pass_with(&mut list, chosen, previous, &Reserved::default())
    }

    fn pass_with(
        list: &mut Vec<Candidate>,
        chosen: &HandPicked,
        previous: Option<&Catalog>,
        reserved: &Reserved,
    ) -> (Vec<Candidate>, Vec<Held>) {
        stamp_ages(list, previous, chosen, NOW);
        let rules = ClaimRules {
            picks: chosen,
            reserved,
            previous,
        };
        hold_collisions(std::mem::take(list), &rules)
    }

    #[test]
    fn an_approved_pair_merges_and_the_counts_add_up() {
        let mut github = gh("funkydude/BugSack", &["BugSack", "!BugGrabber"]);
        github.addon.ids.wowi = Some("5995".into());
        github.addon.downloads = 100;
        let mut listing = wowi(5995, "BugSack (WoWI)", &["BugSack"]);
        listing.addon.downloads = 900;
        listing.favorites = 3;
        let pairs = approved(&[("5995", "FUNKYDUDE/bugsack")]);
        let (out, merged) = merge_wowi(vec![github], vec![listing], &pairs);
        assert_eq!(merged, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].addon.id, "gh:funkydude/BugSack", "GitHub wins");
        assert_eq!(
            out[0].addon.downloads, 1000,
            "counts from both sources add up"
        );
        assert_eq!(out[0].favorites, 3);
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("5995"));
    }

    #[test]
    fn a_pair_gives_the_repository_the_id_even_when_its_toc_doesnt() {
        let github = gh("Nevcairiel/Mapster", &["Mapster"]);
        let listing = wowi(1234, "Something Else Entirely", &["Mapster"]);
        let pairs = approved(&[("1234", "Nevcairiel/Mapster")]);
        let (out, merged) = merge_wowi(vec![github], vec![listing], &pairs);
        assert_eq!(merged, 1);
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("1234"));
    }

    #[test]
    fn claiming_a_listings_id_merges_nothing_and_moves_no_counts() {
        let mut stranger = gh("zebra/CoolBags", &["CoolBags"]);
        stranger.addon.ids.wowi = Some("7032".into());
        stranger.addon.downloads = 1;
        let mut listing = wowi(7032, "TomTom", &["TomTom"]);
        listing.addon.downloads = 3_550_000;
        let (out, merged) = merge_wowi(vec![stranger], vec![listing], &BTreeMap::new());
        assert_eq!(merged, 0);
        assert_eq!(ids(&out), ["gh:zebra/CoolBags", "wowi:7032"]);
        assert_eq!(out[0].addon.downloads, 1, "the stranger gets none of it");
        assert_eq!(out[1].addon.downloads, 3_550_000);
    }

    #[test]
    fn matching_folders_merge_nothing_either() {
        let stranger = gh("zebra/TomTom", &["TomTom"]);
        let listing = wowi(7032, "TomTom", &["TomTom"]);
        let (out, merged) = merge_wowi(vec![stranger], vec![listing], &BTreeMap::new());
        assert_eq!(merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_pair_only_applies_to_the_repository_it_names() {
        let other = gh("zebra/TomTom", &["TomTom"]);
        let listing = wowi(7032, "TomTom", &["TomTom"]);
        let pairs = approved(&[("7032", "cladhaire/TomTom")]);
        let (out, merged) = merge_wowi(vec![other], vec![listing], &pairs);
        assert_eq!(merged, 0, "its repository isn't here, so the listing stays");
        assert_eq!(ids(&out), ["gh:zebra/TomTom", "wowi:7032"]);
    }

    #[test]
    fn two_listings_of_one_addon_settle_on_the_same_id_either_way() {
        let build = |listings: Vec<Candidate>| {
            let mut repo = gh("AssisrMatheus/TodoChecklister", &["TodoChecklister"]);
            repo.addon.ids.wowi = Some("25348".into());
            let pairs = approved(&[
                ("25348", "AssisrMatheus/TodoChecklister"),
                ("25349", "AssisrMatheus/TodoChecklister"),
            ]);
            merge_wowi(vec![repo], listings, &pairs)
        };
        let one = wowi(25348, "TodoChecklister", &["TodoChecklister"]);
        let two = wowi(25349, "TodoChecklister", &["TodoChecklister"]);
        let (forward, merged) = build(vec![one.clone(), two.clone()]);
        let (backward, _) = build(vec![two, one]);
        assert_eq!(merged, 2);
        assert_eq!(forward[0].addon.ids.wowi.as_deref(), Some("25348"));
        assert_eq!(forward, backward);
    }

    #[test]
    fn forks_with_the_same_folders_as_the_parent_go_away() {
        let parent = gh("Up/Stream", &["Foo"]);
        let mut copy = gh("me/Stream", &["Foo"]);
        copy.fork_of = Some("Up/Stream".into());
        let mut own_path = gh("you/Stream", &["FooReborn"]);
        own_path.fork_of = Some("Up/Stream".into());
        let (kept, dropped) =
            drop_redundant_forks(vec![parent, copy, own_path], &HandPicked::default());
        assert_eq!(dropped, ["gh:me/Stream"]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_fork_of_something_missing_can_stay_when_it_is_the_only_one() {
        let mut revived = gh("new/Keeper", &["Foo"]);
        revived.fork_of = Some("Gone/Abandoned".into());
        let (kept, dropped) = drop_redundant_forks(vec![revived], &HandPicked::default());
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn among_forks_of_a_missing_parent_the_best_known_survives() {
        let mut a = gh("a/Fork", &["Foo"]);
        a.fork_of = Some("Gone/Foo".into());
        a.addon.stars = 40;
        let mut b = gh("b/Fork", &["Foo"]);
        b.fork_of = Some("Gone/Foo".into());
        b.addon.stars = 2;
        let (kept, dropped) = drop_redundant_forks(vec![a, b], &HandPicked::default());
        assert_eq!(dropped, ["gh:b/Fork"]);
        assert_eq!(kept[0].addon.id, "gh:a/Fork");
    }

    #[test]
    fn a_fork_of_a_listed_fork_goes_away_too() {
        let mut middle = gh("middle/Stream", &["Foo"]);
        middle.fork_of = Some("Gone/Stream".into());
        let mut last = gh("last/Stream", &["Foo"]);
        last.fork_of = Some("middle/Stream".into());
        let (_, dropped) = drop_redundant_forks(vec![middle, last], &HandPicked::default());
        assert_eq!(dropped, ["gh:last/Stream"]);
    }

    #[test]
    fn a_new_repository_with_the_same_folders_doesnt_evict_a_fork() {
        for chosen in [picks(&[]), picks(&["Legacy-of-Sylvanaar/prat-3-0"])] {
            let mut real = gh("Legacy-of-Sylvanaar/prat-3-0", &["Prat-3.0"]);
            real.fork_of = Some("sylvanaar/prat-3-0".into());
            real.addon.stars = 38;
            let stranger = gh("free-account/Prat", &["Prat-3.0"]);
            let (kept, dropped) = drop_redundant_forks(vec![real, stranger], &chosen);
            assert!(dropped.is_empty(), "{dropped:?}");
            assert_eq!(kept.len(), 2, "the collision pass settles the rest");
        }
    }

    #[test]
    fn a_hand_picked_fork_is_never_dropped_even_beside_its_parent() {
        let parent = gh("sylvanaar/wim", &["WIM"]);
        let mut fork = gh("Legacy-of-Sylvanaar/wim", &["WIM"]);
        fork.fork_of = Some("sylvanaar/wim".into());
        let chosen = picks(&["Legacy-of-Sylvanaar/wim"]);
        let (kept, dropped) = drop_redundant_forks(vec![parent, fork], &chosen);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn the_first_build_with_ages_makes_everything_listed_the_baseline() {
        let old = gh("a/Old", &["Old"]);
        let chosen = gh("a/Chosen", &["Chosen"]);
        let previous = published(&[(&old, 0), (&chosen, 0)]);
        let fresh = gh("a/Fresh", &["Fresh"]);
        let mut list = vec![old.clone(), chosen.clone(), fresh];
        stamp_ages(&mut list, Some(&previous), &picks(&["a/Chosen"]), NOW);
        assert_eq!(list[0].addon.created, BASELINE_CREATED);
        assert_eq!(list[1].addon.created, BASELINE_CREATED);
        assert_eq!(list[2].addon.created, NOW, "it wasn't listed");
    }

    #[test]
    fn an_age_stays_from_build_to_build() {
        let addon = gh("a/Thing", &["Thing"]);
        let previous = published(&[(&addon, NOW - 3 * DAY)]);
        let mut list = vec![addon];
        stamp_ages(&mut list, Some(&previous), &picks(&[]), NOW);
        assert_eq!(list[0].addon.created, NOW - 3 * DAY);
    }

    #[test]
    fn with_nothing_published_yet_everything_is_new() {
        let mut list = vec![gh("a/Thing", &["Thing"])];
        stamp_ages(&mut list, None, &picks(&[]), NOW);
        assert_eq!(list[0].addon.created, NOW);
    }

    #[test]
    fn changing_a_name_or_main_folder_counts_as_new_unless_hand_picked() {
        let listed = named(gh("a/Thing", &["Thing", "Thing_Options"]), "Thing");
        let previous = published(&[(&listed, NOW - 90 * DAY)]);
        let age_after = |candidate: Candidate, chosen: &[&str]| {
            let mut list = vec![candidate];
            stamp_ages(&mut list, Some(&previous), &picks(chosen), NOW);
            list[0].addon.created
        };
        let retitled = named(listed.clone(), "Questie");
        assert_eq!(age_after(retitled.clone(), &[]), NOW);
        assert_eq!(age_after(retitled, &["a/Thing"]), NOW - 90 * DAY);

        let moved = gh("a/Thing", &["Questie", "Thing"]);
        assert_eq!(age_after(named(moved, "Thing"), &[]), NOW);

        // None of these is a different name to a reader, or a different folder.
        for same in ["THING", "Thing!", "Th\u{0456}ng"] {
            assert_eq!(
                age_after(named(listed.clone(), same), &[]),
                NOW - 90 * DAY,
                "{same}"
            );
        }
        let more_folders = gh("a/Thing", &["thing", "Thing_Options", "Thing_Extra"]);
        assert_eq!(
            age_after(named(more_folders, "Thing"), &[]),
            NOW - 90 * DAY,
            "a side folder isn't the addon's identity"
        );
    }

    #[test]
    fn a_new_repo_cant_take_a_listings_name_with_or_without_claiming_its_id() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let previous = published(&[(&tomtom, 0)]);
        for chosen in [picks(&[]), picks(&["7032"])] {
            let mut with_id = gh("zebra/TomTom", &["TomTom"]);
            with_id.addon.ids.wowi = Some("7032".into());
            with_id.addon.downloads = 4_000_000;
            for stranger in [with_id, gh("zebra/TomTom", &["TomTom"])] {
                let (kept, held) = pass(vec![stranger, tomtom.clone()], &chosen, Some(&previous));
                assert_eq!(ids(&kept), ["wowi:7032"]);
                assert_eq!(held_ids(&held), ["gh:zebra/TomTom"]);
                assert!(held[0].reason.contains("wowi:7032"), "{}", held[0].reason);
            }
        }
    }

    #[test]
    fn download_counts_dont_decide_who_owns_a_name() {
        let mut real = named(gh("Peavers/PeaversCommons", &["PeaversCommons"]), "Commons");
        real.addon.downloads = 10;
        let previous = published(&[(&real, NOW - 30 * DAY)]);
        let mut copy = named(gh("zebra/Commons", &["PeaversCommons"]), "Commons");
        copy.addon.downloads = 9_000_000;
        copy.addon.stars = 5000;
        let (kept, held) = pass(vec![copy, real], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Peavers/PeaversCommons"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Commons"]);
    }

    #[test]
    fn look_alike_letters_dont_get_a_name_past_the_pass() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, 0)]);
        let fake = named(gh("zebra/Totally", &["QuestieX"]), "Qu\u{0435}stie");
        let (kept, held) = pass(vec![fake, questie], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Questie/Questie"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Totally"]);
    }

    #[test]
    fn a_folder_that_only_looks_the_same_counts_as_taken() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, 0)]);
        let fake = named(gh("zebra/Totally", &["Qu\u{0435}stie"]), "Something Else");
        let (_, held) = pass(vec![fake, questie], &picks(&[]), Some(&previous));
        assert_eq!(held_ids(&held), ["gh:zebra/Totally"]);
    }

    #[test]
    fn a_main_folder_taken_by_anyone_older_holds_a_newcomer_under_another_name() {
        let masque = named(gh("SFX-WoW/Masque", &["Masque"]), "Masque");
        let dominos = named(gh("t/Dominos", &["Dominos", "Masque_Dominos"]), "Dominos");
        let previous = published(&[(&masque, 0), (&dominos, 0)]);
        for (repo, main) in [("zebra/One", "Masque"), ("zebra/Two", "Masque_Dominos")] {
            let new = named(gh(repo, &[main]), "A New Name");
            let (_, held) = pass(
                vec![new, masque.clone(), dominos.clone()],
                &picks(&[]),
                Some(&previous),
            );
            assert_eq!(held_ids(&held), [repo.replace("zebra", "gh:zebra")]);
            assert!(held[0].reason.contains("main folder"), "{}", held[0].reason);
        }
    }

    #[test]
    fn a_side_folder_taken_by_something_older_holds_a_newcomer() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, NOW - 90 * DAY)]);
        let pins = gh("zebra/CoolPins", &["CoolPins", "Questie"]);
        let (kept, held) = pass(vec![pins, questie], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Questie/Questie"]);
        assert_eq!(held_ids(&held), ["gh:zebra/CoolPins"]);
        assert!(held[0].reason.contains("Questie"), "{}", held[0].reason);
    }

    #[test]
    fn a_side_folder_shared_with_something_just_as_new_is_allowed() {
        let one = gh("a/One", &["One", "SharedLib"]);
        let two = gh("b/Two", &["Two", "SharedLib"]);
        let (kept, held) = pass(vec![one, two], &picks(&[]), None);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn what_was_already_listed_together_stays_together() {
        // Each of these pairs is in the real catalog today.
        let pairs = [
            (
                named(gh("delaysoftware/fightclub-forever", &["FightClub"]), "One"),
                named(gh("lordjumper/FightClub", &["FightClub"]), "Two"),
            ),
            (
                named(gh("SFX-WoW/Masque_Dominos", &["Masque_Dominos"]), "Skin"),
                named(gh("tullamods/Dominos", &["Dominos", "Masque_Dominos"]), "D"),
            ),
            (
                named(gh("LenweSaralonde/Musician", &["Musician"]), "Musician"),
                wowi(24915, "Musician", &["Musician", "MusicianList"]),
            ),
            (
                named(gh("stpain/Forever", &["Forever"]), "Forever"),
                named(gh("xIG/ForeverPlusPlus", &["ForeverPlusPlus"]), "Forever++"),
            ),
        ];
        for (a, b) in pairs {
            let previous = published(&[(&a, 0), (&b, 0)]);
            let chosen = [
                picks(&[]),
                picks(&["SFX-WoW/Masque_Dominos", "xIG/ForeverPlusPlus"]),
            ];
            for chosen in chosen {
                let (kept, held) = pass(vec![a.clone(), b.clone()], &chosen, Some(&previous));
                assert!(held.is_empty(), "{held:?}");
                assert_eq!(kept.len(), 2);
            }
        }
    }

    #[test]
    fn a_new_seed_beats_even_an_older_stranger_for_its_name() {
        let stranger = gh("zebra/WeakAuras", &["WeakAuras"]);
        let previous = published(&[(&stranger, 0)]);
        let real = named(gh("WeakAuras/WeakAuras2", &["WeakAuras"]), "WeakAuras");
        let chosen = picks(&["WeakAuras/WeakAuras2"]);
        let (kept, held) = pass(vec![stranger, real], &chosen, Some(&previous));
        assert_eq!(ids(&kept), ["gh:WeakAuras/WeakAuras2"]);
        assert_eq!(held_ids(&held), ["gh:zebra/WeakAuras"]);
    }

    #[test]
    fn two_hand_picked_repos_settle_like_they_always_have() {
        let mut big = named(gh("real/Details", &["Details"]), "Details!");
        big.addon.downloads = 5000;
        let small = named(gh("other/Details", &["Details"]), "Details!");
        let chosen = picks(&["real/Details", "other/Details"]);
        let (kept, held) = pass(vec![small, big.clone()], &chosen, None);
        assert_eq!(ids(&kept), ["gh:real/Details"], "an exact copy goes");
        assert!(held[0].reason.contains("copy"), "{}", held[0].reason);

        let different = named(gh("other/Details", &["Details"]), "Another Project");
        let (kept, held) = pass(vec![different, big], &chosen, None);
        assert!(held.is_empty(), "both were chosen: {held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_hand_given_name_counts_when_the_pass_runs_after_overrides() {
        // `settle` applies names first. This pins what that buys.
        let dbm = named(gh("Deadly/DBM", &["DBM-Core"]), "Deadly Boss Mods");
        let previous = published(&[(&dbm, 0)]);
        let fake = named(gh("zebra/Boss", &["BossThing"]), "Deadly Boss Mods");
        let (_, held) = pass(vec![fake, dbm], &picks(&["Deadly/DBM"]), Some(&previous));
        assert_eq!(held_ids(&held), ["gh:zebra/Boss"]);
    }

    #[test]
    fn a_retitled_sleeper_is_new_and_loses_to_what_was_there_first() {
        let real = gh("Peavers/PeaversCommons", &["PeaversCommons"]);
        let sleeper = gh("zebra/Helpers", &["Helpers"]);
        // The sleeper has been listed longer than the real addon.
        let previous = published(&[(&sleeper, NOW - 90 * DAY), (&real, NOW - 30 * DAY)]);
        let retitled = named(gh("zebra/Helpers", &["PeaversCommons"]), "PeaversCommons");
        let (kept, held) = pass(vec![retitled, real], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Peavers/PeaversCommons"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
    }

    #[test]
    fn a_sleeper_cant_evict_a_younger_addon_by_adding_its_folder() {
        let real = gh("Peavers/PeaversCommons", &["PeaversCommons"]);
        let sleeper = named(gh("zebra/Helpers", &["Helpers"]), "Helpers");
        for created in [0, NOW - 90 * DAY] {
            let previous = published(&[(&sleeper, created), (&real, NOW - 30 * DAY)]);
            let grown = named(
                gh("zebra/Helpers", &["Helpers", "PeaversCommons"]),
                "Helpers",
            );
            let (kept, held) = pass(vec![grown, real.clone()], &picks(&[]), Some(&previous));
            assert_eq!(
                ids(&kept),
                ["gh:Peavers/PeaversCommons"],
                "the older claim on the folder wins"
            );
            assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
            assert!(held[0].reason.contains("just added"), "{}", held[0].reason);
        }
    }

    #[test]
    fn a_baseline_repo_cant_add_a_popular_side_folder_either() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let stranger = named(gh("zebra/Helpers", &["Helpers"]), "Helpers");
        let previous = published(&[(&tomtom, 0), (&stranger, 0)]);
        let grown = named(gh("zebra/Helpers", &["Helpers", "TomTom"]), "Helpers");
        let (kept, held) = pass(vec![grown, tomtom], &picks(&["7032"]), Some(&previous));
        assert_eq!(ids(&kept), ["wowi:7032"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
    }

    #[test]
    fn an_addon_adding_a_folder_nobody_else_had_is_fine() {
        let listed = gh("a/Thing", &["Thing"]);
        let previous = published(&[(&listed, NOW - 90 * DAY)]);
        let grown = named(gh("a/Thing", &["Thing", "Thing_Options"]), "Thing");
        let (kept, held) = pass(vec![grown], &picks(&[]), Some(&previous));
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept[0].addon.created, NOW - 90 * DAY);
    }

    #[test]
    fn a_held_entry_claims_nothing() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let previous = published(&[(&tomtom, 0)]);
        let first = named(gh("a/Copy", &["Elsewhere", "TomTom"]), "Elsewhere");
        let second = named(gh("b/Other", &["Elsewhere"]), "Other");
        let (kept, held) = pass(
            vec![first, second, tomtom],
            &picks(&["7032"]),
            Some(&previous),
        );
        assert_eq!(held_ids(&held), ["gh:a/Copy"]);
        assert_eq!(ids(&kept), ["gh:b/Other", "wowi:7032"]);
    }

    #[test]
    fn in_one_build_a_moderated_listing_goes_before_a_repository_and_the_outcome_is_stable() {
        let mut repo = named(gh("zebra/Foo", &["Foo"]), "Foo");
        repo.addon.downloads = 9_000_000;
        let listing = wowi(77, "Foo", &["Foo"]);
        let (kept, held) = pass(vec![repo.clone(), listing.clone()], &picks(&[]), None);
        assert_eq!(ids(&kept), ["wowi:77"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Foo"]);

        let a = named(gh("a/One", &["Foo"]), "Foo");
        let b = named(gh("b/Two", &["Foo"]), "Foo");
        let (first, _) = pass(vec![a.clone(), b.clone()], &picks(&[]), None);
        let (second, _) = pass(vec![b, a], &picks(&[]), None);
        assert_eq!(ids(&first), ids(&second));
    }

    #[test]
    fn reserved_names_and_folders_are_only_for_something_hand_picked() {
        let curated = Curated {
            gaps: vec![
                GapSpec {
                    name: "Leatrix Plus".into(),
                    folders: vec!["Leatrix_Plus".into()],
                    ..GapSpec::default()
                },
                GapSpec {
                    name: "ElvUI".into(),
                    folders: vec!["ElvUI".into(), "ElvUI_Options".into()],
                    ..GapSpec::default()
                },
                GapSpec {
                    name: "BugGrabber".into(),
                    folders: vec!["!BugGrabber".into()],
                    ..GapSpec::default()
                },
            ],
            reserved: vec!["WeakAuras".into(), "pfQuest".into()],
            ..Curated::default()
        };
        let reserved = Reserved::new(&curated);
        let mut list = vec![
            named(gh("z/One", &["One"]), "Leatrix Plus"),
            named(gh("z/Two", &["Leatrix_Plus"]), "Something"),
            named(gh("z/Three", &["Three", "Leatrix_Plus"]), "Something 3"),
            named(gh("z/Four", &["Four"]), "ELV-UI"),
            named(gh("z/Five", &["ElvUI"]), "Something 5"),
            named(gh("z/Six", &["Six", "ElvUI_Options"]), "Something 6"),
            named(gh("z/Seven", &["Seven", "!BugGrabber"]), "Something 7"),
            named(gh("z/Eight", &["BugGrabber"]), "Bug Grabber"),
            named(gh("z/Nine", &["Nine", "WeakAuras"]), "Something 9"),
            named(gh("z/Ten", &["Ten"]), "Weak Auras"),
            named(gh("z/Eleven", &["pfQuest"]), "Something 11"),
            named(gh("z/Twelve", &["Twelve"]), "Something 12"),
            wowi(5, "ElvUI", &["ElvUI"]),
        ];
        let (kept, held) = pass_with(&mut list, &picks(&[]), None, &reserved);
        assert_eq!(
            ids(&kept),
            ["gh:z/Twelve"],
            "the only one not reserved: {held:?}"
        );
        assert_eq!(held.len(), 12);
        assert!(
            held.iter().all(|h| h.reason.contains("reserved")),
            "{held:?}"
        );

        let mut real = vec![
            named(gh("tukui-org/ElvUI", &["ElvUI", "ElvUI_Options"]), "ElvUI"),
            named(gh("WeakAuras/WeakAuras2", &["WeakAuras"]), "WeakAuras"),
        ];
        let chosen = picks(&["tukui-org/ElvUI", "WeakAuras/WeakAuras2"]);
        let (kept, held) = pass_with(&mut real, &chosen, None, &reserved);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn an_owner_gets_a_cap_oldest_first_but_hand_picked_repos_dont_count() {
        let repo = |at: usize| {
            named(
                gh(&format!("flood/R{at}"), &[&format!("F{at}")]),
                &format!("N{at}"),
            )
        };
        let flood: Vec<Candidate> = (0..MAX_ENTRIES_PER_OWNER + 5).map(repo).collect();
        // The oldest ones have the highest numbers, so order in the list can't decide this.
        let listed: Vec<(&Candidate, i64)> = flood
            .iter()
            .rev()
            .take(MAX_ENTRIES_PER_OWNER)
            .enumerate()
            .map(|(at, c)| (c, NOW - 100 * DAY + i64::try_from(at).unwrap()))
            .collect();
        let previous = published(&listed);
        let (kept, held) = pass(flood.clone(), &picks(&[]), Some(&previous));
        assert_eq!(kept.len(), MAX_ENTRIES_PER_OWNER);
        assert_eq!(held.len(), 5);
        assert!(held.iter().all(|h| h.reason.contains("flood")), "{held:?}");
        assert!(
            held.iter().any(|h| h.id == "gh:flood/R0"),
            "the new ones go"
        );

        let seed_names: Vec<String> = (0..MAX_ENTRIES_PER_OWNER + 5)
            .map(|at| format!("flood/R{at}"))
            .collect();
        let seeds: Vec<&str> = seed_names.iter().map(String::as_str).collect();
        let (kept, held) = pass(flood, &picks(&seeds), None);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), MAX_ENTRIES_PER_OWNER + 5);

        let others = vec![gh("other/One", &["One"]), gh("other/Two", &["Two"])];
        let (kept, _) = pass(others, &picks(&[]), None);
        assert_eq!(kept.len(), 2);
    }

    fn trust(
        mut list: Vec<Candidate>,
        chosen: &HandPicked,
        pairs: &[(&str, &str)],
    ) -> Vec<Candidate> {
        settle_trust(&mut list, chosen, &approved(pairs), NOW);
        list
    }

    fn aged(mut candidate: Candidate, days: i64) -> Candidate {
        candidate.addon.created = NOW - days * DAY;
        candidate
    }

    fn with_ids(mut candidate: Candidate, wowi: &str, curse: &str, wago: &str) -> Candidate {
        candidate.addon.ids = ExternalIds {
            wowi: Some(wowi.into()),
            curse: Some(curse.into()),
            wago: Some(wago.into()),
        };
        candidate
    }

    #[test]
    fn only_hand_picked_listed_and_established_entries_are_trusted() {
        let out = trust(
            vec![
                aged(gh("a/Chosen", &["A"]), 0),
                aged(wowi(9, "Listing", &["B"]), 0),
                aged(gh("a/Old", &["C"]), 14),
                aged(gh("a/Older", &["D"]), 400),
                aged(gh("a/Young", &["E"]), 13),
                aged(gh("a/New", &["F"]), 0),
            ],
            &picks(&["a/Chosen"]),
            &[],
        );
        let trusted: Vec<&str> = out
            .iter()
            .filter(|c| c.addon.trusted)
            .map(|c| c.addon.id.as_str())
            .collect();
        assert_eq!(trusted, ["gh:a/Chosen", "wowi:9", "gh:a/Old", "gh:a/Older"]);
    }

    #[test]
    fn the_ids_of_an_untrusted_entry_are_only_claims_and_go() {
        let out = trust(
            vec![
                with_ids(aged(gh("zebra/New", &["A"]), 1), "7032", "2382", "abc"),
                with_ids(aged(gh("a/Old", &["B"]), 30), "1", "2", "3"),
            ],
            &picks(&[]),
            &[],
        );
        assert!(out[0].addon.ids.is_empty());
        assert_eq!(out[1].addon.ids.wowi.as_deref(), Some("1"));
        assert_eq!(out[1].addon.ids.curse.as_deref(), Some("2"));
        assert_eq!(out[1].addon.ids.wago.as_deref(), Some("3"));
    }

    #[test]
    fn an_id_stays_with_whoever_owns_it() {
        // In the collision pass's order: hand-picked first.
        let out = trust(
            vec![
                with_ids(
                    aged(gh("mooreatv/AuctionDB", &["AuctionDB"]), 90),
                    "25159",
                    "336518",
                    "R4N",
                ),
                with_ids(
                    aged(gh("zebra/Copy", &["AuctionDB"]), 90),
                    "25159",
                    "336518",
                    "R4N",
                ),
                with_ids(aged(gh("zebra/Sleeper", &["S"]), 90), "7032", "2382", "own"),
                aged(wowi(7032, "TomTom", &["TomTom"]), 90),
            ],
            &picks(&["mooreatv/AuctionDB"]),
            &[("25159", "mooreatv/AuctionDB")],
        );
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("25159"));
        assert_eq!(out[0].addon.ids.curse.as_deref(), Some("336518"));
        assert!(out[1].addon.ids.is_empty(), "a copy of the real one's ids");
        assert_eq!(out[2].addon.ids.wowi, None, "the listing owns its own id");
        assert_eq!(out[2].addon.ids.curse.as_deref(), Some("2382"));
        assert_eq!(out[2].addon.ids.wago.as_deref(), Some("own"));
        assert_eq!(out[3].addon.ids.wowi.as_deref(), Some("7032"));
    }

    fn info(name: &str) -> RepoInfo {
        RepoInfo {
            name: name.into(),
            ..RepoInfo::default()
        }
    }

    #[test]
    fn junk_repositories_are_rejected() {
        assert_eq!(repo_rejection(&info("a/Good"), false), None);
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    archived: true,
                    ..info("a/b")
                },
                false
            ),
            Some("archived")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    template: true,
                    ..info("a/b")
                },
                false
            ),
            Some("template")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    empty: true,
                    ..info("a/b")
                },
                false
            ),
            Some("empty")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    mirror: true,
                    ..info("a/b")
                },
                false
            ),
            Some("mirror")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    disabled: true,
                    ..info("a/b")
                },
                false
            ),
            Some("disabled")
        );
        assert!(repo_rejection(&info("andi-74/foreverdungeonjournal-mirror"), false).is_some());
        assert!(repo_rejection(&info("someone/wow-addon-template"), false).is_some());
        assert_eq!(
            repo_rejection(&info("a/Templates4Life"), false),
            None,
            "whole words only"
        );
    }

    #[test]
    fn hand_picked_repositories_survive_being_archived_but_not_being_empty() {
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    archived: true,
                    ..info("a/b")
                },
                true
            ),
            None
        );
        assert_eq!(
            repo_rejection(&info("a/some-template-looking-name"), true),
            None
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    empty: true,
                    ..info("a/b")
                },
                true
            ),
            Some("empty")
        );
    }

    #[test]
    fn placeholder_names_are_recognized() {
        for name in [
            "MyAddon",
            "My Addon",
            "Addon Template",
            "Hello World",
            "",
            "@",
        ] {
            assert!(is_placeholder_name(name), "{name}");
        }
        for name in ["Questie", "Testament", "Foobar Deluxe"] {
            assert!(!is_placeholder_name(name), "{name}");
        }
    }
}
