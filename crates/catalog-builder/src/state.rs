//! The scan cache, saved as `catalog-state.json` next to the catalog. It records
//! every repository ever found and what each release turned out to be, so an
//! unchanged release costs nothing and a cut-off run still makes progress.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use feeling_core::catalog::{Addon, Catalog};
use serde::{Deserialize, Serialize};

use crate::curated::{DirectState, SourceState};
use crate::detect::ReleaseVerdict;
use crate::model::Candidate;
use crate::wowi::WowiState;

/// Bump when old state can't be understood any more. It's only a cache, so the
/// price is one slow run.
const STATE_SCHEMA: u32 = 1;
const DAY_SECS: i64 = 24 * 60 * 60;
/// How long an entry gone from the catalog keeps its age and its claims: the 30 days after the
/// day of the last catalog it was published in. The memory only keeps that day, so counting whole
/// days is what makes it never less than 30 days.
const AWAY_DAYS: u64 = 30;
/// Folders the memory keeps of an entry, the most a real zip brings today. An entry with more
/// keeps none, so padded zips can't make each entry ever published cost kilobytes, and is marked
/// as having more, so a folder check from memory fails closed for it rather than passing.
const MAX_FOLDERS_REMEMBERED: usize = 32;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub schema: u32,
    /// Unix seconds of the last topic and code search.
    #[serde(default)]
    pub searched_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strongbox_etag: Option<String>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoState>,
    #[serde(default)]
    pub wowi: BTreeMap<u64, WowiState>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceState>,
    #[serde(default)]
    pub direct: BTreeMap<String, DirectState>,
    /// What each entry was last published as. An entry a catalog leaves out keeps it, so one
    /// missing build loses neither which addon the entry is nor what the catalog vouched for.
    #[serde(default, skip_serializing_if = "Remembered::is_empty")]
    pub remembered: Remembered,
}

/// The main folder an entry was last published with, the former ones the catalog vouched for
/// then, when that was, how old the entry was and its id as that catalog had it, and for the 30
/// days they count, its name, its folders and the repository it was forked from. Each of the later
/// fields is empty or 0 for an entry noted before it was kept.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPublished {
    pub main: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formerly: Vec<String>,
    /// Unix seconds when the catalog it was last published in was made, rounded down to the day,
    /// so a build with nothing new leaves the scan cache as it was. 0 for an entry noted before
    /// this was kept, which counts as recent.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub at: i64,
    /// Its `created` in that catalog.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub created: i64,
    /// Its id as that catalog spells it. The memory is keyed by the id in lower case.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Its name in that catalog, until it stops counting (see [`LastPublished::is_recent`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Its folders in that catalog, the main one first, or none for an entry with more than 32,
    /// which `more_folders` marks. Kept until they stop counting, as the name is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// It had more folders than the memory keeps, so `folders` can't tell whether one is new to
    /// it, and a folder check from memory fails closed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub more_folders: bool,
    /// For a fork, the repository GitHub says it was forked from, as owner/name. Kept until it
    /// stops counting, as the name is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fork_of: String,
}

impl LastPublished {
    /// What `addon` was published as in a catalog made on the day `at`, forked from `fork_of`.
    fn of(addon: &Addon, at: i64, fork_of: &str) -> Option<Self> {
        let main = addon.folders.first()?;
        let few = addon.folders.len() <= MAX_FOLDERS_REMEMBERED;
        Some(Self {
            main: main.clone(),
            formerly: addon.formerly.clone(),
            at,
            created: addon.created,
            id: addon.id.clone(),
            name: addon.name.clone(),
            folders: if few {
                addon.folders.clone()
            } else {
                Vec::new()
            },
            more_folders: !few,
            fork_of: fork_of.to_string(),
        })
    }

    /// Its id as the catalog spelled it, or `key`, the lower case one, for an entry noted before
    /// the spelling was kept.
    pub fn id_or<'a>(&'a self, key: &'a str) -> &'a str {
        if self.id.is_empty() { key } else { &self.id }
    }

    /// Whether it was published in the last 30 days, counted in whole days from the day of that
    /// catalog, so nothing lapses early. One noted before the day was kept counts as recent, and
    /// so does one from a day after `now`.
    pub fn is_recent(&self, now: i64) -> bool {
        let day = |time: i64| u64::try_from(time.div_euclid(DAY_SECS)).unwrap_or(0);
        self.at == 0 || day(now).saturating_sub(day(self.at)) <= AWAY_DAYS
    }
}

/// What a build knew of an entry it published that the catalog doesn't say.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The repository GitHub says it was forked from, as owner/name.
    pub fork_of: Option<String>,
    /// The ids it had before GitHub renamed it.
    pub renamed_from: Vec<String>,
}

impl Origin {
    pub fn of(candidate: &Candidate) -> Self {
        Self {
            fork_of: candidate.fork_of.clone(),
            renamed_from: candidate.renamed_from.clone(),
        }
    }
}

/// Each published entry's [`Origin`], by lowercased id.
pub type Origins = BTreeMap<String, Origin>;

/// What each entry was last published as, by lowercased id. An entry stays a short line once its
/// name and folders stop counting, until a maintainer excludes it or GitHub renames its repository
/// and the entry is published under its new id.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Remembered(BTreeMap<String, LastPublished>);

impl Remembered {
    /// Notes what each entry of a catalog that went out was published as, and on which day, with
    /// where `origins` says it came from. Entries it left out keep what was noted before, except
    /// under an id an entry it lists had before GitHub renamed it: what was published under that id
    /// is the renamed entry's past, so nothing speaks for it any more. Then every entry whose
    /// name, folders and parent have stopped counting forgets them.
    pub fn note(&mut self, catalog: &Catalog, origins: &Origins) {
        let at = catalog.generated.div_euclid(DAY_SECS) * DAY_SECS;
        let origin = |addon: &Addon| origins.get(&addon.id.to_ascii_lowercase());
        for addon in &catalog.addons {
            for old in origin(addon).into_iter().flat_map(|o| &o.renamed_from) {
                self.0.remove(&old.to_ascii_lowercase());
            }
        }
        for addon in &catalog.addons {
            let fork_of = origin(addon).and_then(|o| o.fork_of.as_deref());
            if let Some(last) = LastPublished::of(addon, at, fork_of.unwrap_or_default()) {
                self.0.insert(addon.id.to_ascii_lowercase(), last);
            }
        }
        for last in self.0.values_mut() {
            if !last.is_recent(catalog.generated) {
                last.name.clear();
                last.folders.clear();
                last.more_folders = false;
                last.fork_of.clear();
            }
        }
    }

    /// Notes each entry a catalog lists that the memory has nothing of, or only from before that
    /// catalog's day, as of that day. So the memory starts from the last catalog, such as when the
    /// scan cache had none, and a build that leaves an entry out still keeps what it was. A catalog
    /// with no time on it says nothing about when.
    pub fn note_missing(&mut self, catalog: &Catalog) {
        if catalog.generated <= 0 {
            return;
        }
        let at = catalog.generated.div_euclid(DAY_SECS) * DAY_SECS;
        for addon in &catalog.addons {
            let key = addon.id.to_ascii_lowercase();
            if self.0.get(&key).is_some_and(|last| last.at >= at) {
                continue;
            }
            if let Some(last) = LastPublished::of(addon, at, "") {
                self.0.insert(key, last);
            }
        }
    }

    /// Forgets every entry whose lowercased id `keep` turns down, such as one a maintainer
    /// excluded.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.0.retain(|id, _| keep(id));
    }

    /// How many bytes the memory takes in the scan cache.
    pub fn size(&self) -> usize {
        serde_json::to_vec(self).map_or(0, |bytes| bytes.len())
    }

    /// What the entry with this id was last published as. Case doesn't matter.
    pub fn get(&self, id: &str) -> Option<&LastPublished> {
        self.0.get(&id.to_ascii_lowercase())
    }

    /// Every entry noted, by lowercased id.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &LastPublished)> {
        self.0.iter().map(|(id, last)| (id.as_str(), last))
    }

    /// Each entry's main folder, by lowercased id.
    pub fn mains(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(id, last)| (id.as_str(), last.main.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoState {
    /// Where we heard about this repository.
    #[serde(default)]
    pub found_by: BTreeSet<String>,
    /// Download count from the strongbox catalogue, which sees older releases too.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub known_downloads: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The last good result. Used when this repository can't be checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Candidate>,
    /// Detection results by tag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub releases: BTreeMap<String, ReleaseVerdict>,
    /// The newest tag at the last proper look. While it stays the same, nothing needs reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_tag: Option<String>,
    /// Unix seconds of the last proper look.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub looked_at: i64,
    /// Something failed at the last look, so it needs another.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    /// For a repository in `[[split]]`: each release line's own results, by tag prefix.
    /// Each needs its own verdicts, because pruning one line's would drop the others'.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lines: BTreeMap<String, LineState>,
}

/// What the looks at one release line found.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineState {
    /// The last good result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Candidate>,
    /// Detection results by tag, without the line's prefix.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub releases: BTreeMap<String, ReleaseVerdict>,
    /// Something failed at the last look.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

impl State {
    /// A fresh state with the current schema.
    pub fn empty() -> Self {
        Self {
            schema: STATE_SCHEMA,
            ..Self::default()
        }
    }

    /// Read the state file. A missing, damaged or outdated file means starting
    /// over, never failing: losing a cache only makes a run slower.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read(path) {
            Ok(text) => text,
            Err(_) => return Self::empty(),
        };
        match serde_json::from_slice::<State>(&text) {
            Ok(state) if state.schema == STATE_SCHEMA => state,
            Ok(state) => {
                eprintln!(
                    "{}: schema {} is too old, starting over",
                    path.display(),
                    state.schema
                );
                Self::empty()
            }
            Err(problem) => {
                eprintln!("{}: damaged ({problem}), starting over", path.display());
                Self::empty()
            }
        }
    }

    /// Write atomically, so a run killed mid-save leaves the previous file intact.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes =
            serde_json::to_vec(self).map_err(|e| format!("Couldn't encode the scan cache: {e}"))?;
        write_atomic(path, &bytes)
    }

    /// Records that `found_by` knows about a repository. Sources disagree about
    /// upper and lower case, so a name that differs only by case joins the same record.
    pub fn note_repo(&mut self, name: &str, found_by: &str) -> &mut RepoState {
        let existing = (!self.repos.contains_key(name))
            .then(|| {
                self.repos
                    .keys()
                    .find(|known| known.eq_ignore_ascii_case(name))
            })
            .flatten();
        let key = existing.map_or_else(|| name.to_string(), String::clone);
        let record = self.repos.entry(key).or_default();
        record.found_by.insert(found_by.to_string());
        record
    }

    /// Moves a repository's record after a rename, merging into any record already there.
    pub fn rename_repo(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let Some(moved) = self.repos.remove(old) else {
            return;
        };
        let target = self.repos.entry(new.to_string()).or_default();
        target.found_by.extend(moved.found_by);
        target.known_downloads = target.known_downloads.max(moved.known_downloads);
        if target.entry.is_none() {
            target.entry = moved.entry;
        }
        if target.releases.is_empty() {
            target.releases = moved.releases;
        }
        if target.lines.is_empty() {
            target.lines = moved.lines;
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)
        .map_err(|e| format!("Couldn't write {}: {e}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ZipScan;

    #[test]
    fn saves_and_loads_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog-state.json");
        let mut state = State::empty();
        state.searched_at = 42;
        state.note_repo("A/a", "seed");
        state.note_repo("A/a", "topic");
        let repo = state.repos.get_mut("A/a").unwrap();
        repo.releases.insert(
            "v1".into(),
            ReleaseVerdict {
                sig: "abc".into(),
                asset: Some("a.zip".into()),
                scan: Some(ZipScan {
                    folders: vec!["A".into()],
                    forever: true,
                    ..ZipScan::default()
                }),
                ..ReleaseVerdict::default()
            },
        );
        repo.releases.insert(
            "pack/v2".into(),
            ReleaseVerdict {
                sig: "def".into(),
                skipped_for: Some("A".into()),
                ..ReleaseVerdict::default()
            },
        );
        repo.lines.insert(
            "quests-audio".into(),
            LineState {
                entry: Some(Candidate::default()),
                releases: BTreeMap::from([(
                    "v2.5.0".into(),
                    ReleaseVerdict {
                        sig: "ghi".into(),
                        confirmed_negative: true,
                        ..ReleaseVerdict::default()
                    },
                )]),
                incomplete: true,
            },
        );
        state.wowi.insert(
            7,
            WowiState {
                version: "1".into(),
                entry: None,
            },
        );
        state.save(&path).unwrap();

        let loaded = State::load(&path);
        assert_eq!(loaded.searched_at, 42);
        assert_eq!(loaded.repos["A/a"], state.repos["A/a"]);
        assert_eq!(loaded.repos["A/a"].found_by.len(), 2);
        assert_eq!(loaded.wowi[&7].version, "1");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left behind"
        );
    }

    #[test]
    fn a_missing_or_damaged_file_means_starting_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(State::load(&path).repos.is_empty());

        fs::write(&path, b"{ truncated").unwrap();
        let state = State::load(&path);
        assert!(state.repos.is_empty());
        assert_eq!(state.schema, STATE_SCHEMA);

        fs::write(&path, br#"{"schema":0,"repos":{"A/a":{}}}"#).unwrap();
        assert!(
            State::load(&path).repos.is_empty(),
            "old schemas are thrown away"
        );

        fs::write(&path, b"[1,2,3]").unwrap();
        assert!(State::load(&path).repos.is_empty());
    }

    fn catalog_of(entries: &[(&str, &[&str], &[&str])]) -> Catalog {
        Catalog {
            addons: entries
                .iter()
                .map(|(id, folders, formerly)| feeling_core::catalog::Addon {
                    id: (*id).to_string(),
                    name: "Listed".into(),
                    folders: folders.iter().map(|f| f.to_string()).collect(),
                    formerly: formerly.iter().map(|f| f.to_string()).collect(),
                    created: 1_790_000_000,
                    ..feeling_core::catalog::Addon::default()
                })
                .collect(),
            ..Catalog::default()
        }
    }

    #[test]
    fn what_each_entry_was_published_as_outlasts_a_catalog_that_leaves_it_out() {
        let mut state = State::empty();
        let mut first = catalog_of(&[
            ("wowi:7032", &["TomTomForever"], &["TomTom"]),
            ("gh:A/Addon", &["Addon", "Addon_Options"], &[]),
        ]);
        first.generated = 1_800_000_000;
        state.remembered.note(&first, &Origins::new());
        // The next catalog leaves the listing out and moves the repository's main folder.
        let mut next = catalog_of(&[("gh:a/addon", &["AddonCore", "Addon"], &[])]);
        next.generated = 1_800_086_400;
        state.remembered.note(&next, &Origins::new());
        let tomtom = state.remembered.get("WOWI:7032").unwrap();
        assert_eq!(tomtom.main, "TomTomForever");
        assert_eq!(tomtom.formerly, ["TomTom"]);
        assert_eq!(
            (tomtom.at, tomtom.created),
            (1_799_971_200, 1_790_000_000),
            "the day it was last published, and how old it was then"
        );
        assert_eq!(
            (tomtom.id.as_str(), tomtom.name.as_str()),
            ("wowi:7032", "Listed")
        );
        assert_eq!(tomtom.folders, ["TomTomForever"]);
        let addon = state.remembered.get("gh:A/Addon").unwrap();
        assert_eq!(
            (addon.main.as_str(), addon.at),
            ("AddonCore", 1_800_057_600)
        );
        assert_eq!(
            addon.id, "gh:a/addon",
            "as the catalog it was last in spells it"
        );
        assert_eq!(addon.folders, ["AddonCore", "Addon"]);
        assert_eq!(state.remembered.get("gh:a/other"), None);
        assert_eq!(state.remembered.entries().count(), 2);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog-state.json");
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).remembered, state.remembered);

        // Memory noted before when and how old were kept still loads, with both 0, and so does
        // memory from before its id, name and folders were kept.
        fs::write(
            &path,
            br#"{"schema":1,"remembered":{"wowi:7032":{"main":"TomTomForever","formerly":["TomTom"]},"gh:a/addon":{"main":"Addon","at":1800000000,"created":1790000000}}}"#,
        )
        .unwrap();
        let noted = State::load(&path);
        let tomtom = noted.remembered.get("wowi:7032").unwrap();
        assert_eq!((tomtom.at, tomtom.created), (0, 0));
        assert_eq!(tomtom.formerly, ["TomTom"]);
        let addon = noted.remembered.get("gh:a/addon").unwrap();
        assert_eq!((addon.at, addon.created), (1_800_000_000, 1_790_000_000));
        assert!(addon.id.is_empty() && addon.name.is_empty() && addon.folders.is_empty());
        assert_eq!(addon.id_or("gh:a/addon"), "gh:a/addon");

        // A scan cache from before this was kept still loads, with nothing remembered.
        fs::write(&path, br#"{"schema":1,"repos":{"A/a":{}}}"#).unwrap();
        let old = State::load(&path);
        assert!(old.repos.contains_key("A/a"));
        assert!(old.remembered.is_empty());
        assert!(
            !String::from_utf8(serde_json::to_vec(&old).unwrap())
                .unwrap()
                .contains("remembered"),
            "nothing written while there's nothing to remember"
        );
    }

    #[test]
    fn the_memory_of_entries_that_are_gone_stays_small_whatever_they_listed() {
        // Catalogs whose entries are as big as the gate lets out, each with fresh ids, as a
        // stranger's repositories that come and go would be.
        let folder = |entry: usize, at: usize| format!("{entry:06}{at:03}{}", "x".repeat(111));
        let entry = |n: usize| Addon {
            id: format!("gh:stranger{}/repo{n}", n / 40),
            name: format!("Stranger Addon {n}"),
            folders: (0..crate::zipscan::MAX_FOLDERS)
                .map(|at| folder(n, at))
                .collect(),
            created: 1_800_000_000,
            ..Addon::default()
        };
        let one = Catalog {
            addons: vec![entry(0)],
            generated: 1_800_000_000,
            ..Catalog::default()
        };
        let one_bytes = serde_json::to_vec(&one).unwrap().len();
        let mut remembered = Remembered::default();
        remembered.note(&one, &Origins::new());
        let per_entry = remembered.size();
        // As many such entries as one catalog can carry.
        let per_catalog = 8 * 1024 * 1024 / one_bytes;
        let added_per_catalog = per_entry * per_catalog;
        let catalogs_to_95_mib = 95 * 1024 * 1024 / added_per_catalog.max(1);
        assert!(
            per_entry < 400,
            "one published entry costs {per_entry} bytes of memory; one catalog of {per_catalog} such entries adds {added_per_catalog} bytes, so {catalogs_to_95_mib} catalogs with fresh ids take the scan cache past 95 MiB"
        );
    }

    #[test]
    fn what_a_padded_entry_costs_the_memory_now_and_a_month_on() {
        let padded = |folders: usize| Addon {
            id: "gh:stranger0/repo0".into(),
            name: "Stranger Addon 0".into(),
            folders: (0..folders)
                .map(|at| format!("{at:03}{}", "x".repeat(117)))
                .collect(),
            created: 1_800_000_000,
            ..Addon::default()
        };
        for folders in [32, 33, 256] {
            let mut remembered = Remembered::default();
            let noted = Catalog {
                addons: vec![padded(folders)],
                generated: 1_800_000_000,
                ..Catalog::default()
            };
            remembered.note(&noted, &Origins::new());
            let fresh = remembered.size();
            let last = remembered.get("gh:stranger0/repo0").unwrap();
            assert_eq!(
                last.folders.len(),
                if folders <= 32 { folders } else { 0 },
                "{folders}"
            );
            assert_eq!(last.more_folders, folders > 32, "{folders}");
            // A month on, another catalog goes out without it.
            let later = Catalog {
                generated: 1_800_000_000 + 31 * DAY_SECS,
                ..Catalog::default()
            };
            remembered.note(&later, &Origins::new());
            let last = remembered.get("gh:stranger0/repo0").unwrap();
            assert!(last.name.is_empty() && last.folders.is_empty(), "{folders}");
            assert!(!last.more_folders, "{folders}");
            assert_eq!(last.id, "gh:stranger0/repo0", "still known");
            assert_eq!(last.main, padded(folders).folders[0], "and still pinned");
            let month_on = remembered.size();
            assert!(fresh < 4_500, "{folders} folders: {fresh} bytes when noted");
            assert!(
                month_on < 250,
                "{folders} folders: {month_on} bytes a month on"
            );
        }
    }

    #[test]
    fn an_id_a_published_entry_had_before_github_renamed_it_is_forgotten() {
        let mut remembered = Remembered::default();
        let before = catalog_of(&[("gh:zebra/Helpers", &["Helpers"], &[])]);
        remembered.note(&before, &Origins::new());
        // The rename goes out, beside something unrelated published at the old name.
        let renamed = Origin {
            renamed_from: vec!["gh:zebra/Helpers".into()],
            ..Origin::default()
        };
        let origins = Origins::from([("gh:zebra2/helpers".to_string(), renamed)]);
        let mut after = catalog_of(&[
            ("gh:zebra2/Helpers", &["Helpers"], &[]),
            ("gh:zebra/Helpers", &["Other"], &[]),
        ]);
        after.generated = DAY_SECS;
        remembered.note(&after, &origins);
        assert_eq!(remembered.get("gh:zebra/Helpers").unwrap().main, "Other");
        // Without it, nothing is remembered under the old id at all.
        let alone = catalog_of(&[("gh:zebra2/Helpers", &["Helpers"], &[])]);
        remembered.note(&alone, &origins);
        assert_eq!(remembered.get("gh:zebra/Helpers"), None);
        assert_eq!(remembered.get("gh:zebra2/Helpers").unwrap().main, "Helpers");

        // A fork's parent is noted with it.
        let fork = Origin {
            fork_of: Some("up/Stream".into()),
            ..Origin::default()
        };
        let forked = catalog_of(&[("gh:fork/Stream", &["Stream"], &[])]);
        remembered.note(&forked, &Origins::from([("gh:fork/stream".into(), fork)]));
        assert_eq!(
            remembered.get("gh:fork/Stream").unwrap().fork_of,
            "up/Stream"
        );
    }

    #[test]
    fn a_memory_entry_counts_for_the_30_days_after_the_day_it_was_noted() {
        let day = 20_833 * DAY_SECS;
        let noted = LastPublished {
            at: day,
            ..LastPublished::default()
        };
        assert!(
            noted.is_recent(day + 31 * DAY_SECS - 1),
            "the last second of the 30th day"
        );
        assert!(!noted.is_recent(day + 31 * DAY_SECS));
        assert!(noted.is_recent(day - 5 * DAY_SECS), "from a day after now");
        assert!(LastPublished::default().is_recent(i64::MAX), "undated");
        // Whatever a damaged scan cache says, nothing overflows.
        let early = LastPublished {
            at: i64::MIN,
            ..LastPublished::default()
        };
        assert!(!early.is_recent(i64::MAX));
        let late = LastPublished {
            at: i64::MAX,
            ..LastPublished::default()
        };
        assert!(late.is_recent(i64::MIN));
    }

    #[test]
    fn the_memory_starts_from_the_last_catalog_for_what_it_has_nothing_newer_of() {
        let mut remembered = Remembered::default();
        let mut newer = catalog_of(&[("wowi:1", &["New"], &[])]);
        newer.generated = 10 * DAY_SECS;
        remembered.note(&newer, &Origins::new());
        let mut older = catalog_of(&[("wowi:2", &["Two"], &[])]);
        older.generated = 2 * DAY_SECS;
        remembered.note(&older, &Origins::new());

        let mut last = catalog_of(&[
            ("wowi:1", &["Old"], &[]),
            ("wowi:2", &["TwoNow"], &[]),
            ("gh:a/Addon", &["Addon"], &[]),
        ]);
        last.generated = 5 * DAY_SECS + 600;
        remembered.note_missing(&last);
        let main = |id: &str| remembered.get(id).map(|last| (last.main.clone(), last.at));
        assert_eq!(
            main("wowi:1"),
            Some(("New".into(), 10 * DAY_SECS)),
            "newer stays"
        );
        assert_eq!(main("wowi:2"), Some(("TwoNow".into(), 5 * DAY_SECS)));
        assert_eq!(main("gh:a/addon"), Some(("Addon".into(), 5 * DAY_SECS)));
        let filled = remembered.get("gh:a/Addon").unwrap();
        assert_eq!((filled.name.as_str(), filled.folders.len()), ("Listed", 1));

        // Again, nothing changes, and a catalog with no time on it fills nothing.
        let before = remembered.clone();
        remembered.note_missing(&last);
        assert_eq!(remembered, before);
        let undated = catalog_of(&[("gh:b/Other", &["Other"], &[])]);
        remembered.note_missing(&undated);
        assert_eq!(remembered, before);
    }

    #[test]
    fn saving_into_a_missing_directory_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("state.json");
        assert!(State::empty().save(&path).is_err());
    }

    #[test]
    fn names_differing_by_case_share_one_record() {
        let mut state = State::empty();
        state.note_repo("Foo/Bar", "seed");
        state.note_repo("foo/bar", "topic").known_downloads = 5;
        assert_eq!(state.repos.len(), 1);
        assert_eq!(state.repos["Foo/Bar"].found_by.len(), 2);
        assert_eq!(state.repos["Foo/Bar"].known_downloads, 5);
    }

    #[test]
    fn a_rename_moves_the_record_and_merges() {
        let mut state = State::empty();
        state.note_repo("old/name", "seed");
        state.repos.get_mut("old/name").unwrap().known_downloads = 10;
        state.rename_repo("old/name", "new/name");
        assert!(!state.repos.contains_key("old/name"));
        assert_eq!(state.repos["new/name"].known_downloads, 10);

        state.note_repo("older/name", "topic");
        state.rename_repo("older/name", "new/name");
        assert_eq!(state.repos["new/name"].found_by.len(), 2);

        state.rename_repo("new/name", "new/name");
        assert!(state.repos.contains_key("new/name"));
        state.rename_repo("not/there", "x/y");
        assert!(!state.repos.contains_key("x/y"));
    }

    #[test]
    fn release_lines_move_with_a_renamed_repository() {
        let mut state = State::empty();
        let old = state.note_repo("rusty-key/spoken-wow", "seed");
        old.lines
            .insert("quests-audio".into(), LineState::default());
        state.rename_repo("rusty-key/spoken-wow", "rusty-key/spoken");
        assert!(
            state.repos["rusty-key/spoken"]
                .lines
                .contains_key("quests-audio")
        );

        // A record already at the new name keeps its own lines.
        let other = state.note_repo("rusty-key/other", "seed");
        other
            .lines
            .insert("bosses-audio".into(), LineState::default());
        state.rename_repo("rusty-key/other", "rusty-key/spoken");
        let lines: Vec<&String> = state.repos["rusty-key/spoken"].lines.keys().collect();
        assert_eq!(lines, ["quests-audio"]);
    }
}
