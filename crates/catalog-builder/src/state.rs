//! The scan cache, saved as `catalog-state.json` next to the catalog. It records
//! every repository ever found and what each release turned out to be, so an
//! unchanged release costs nothing and a cut-off run still makes progress.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use feeling_core::catalog::Catalog;
use serde::{Deserialize, Serialize};

use crate::curated::{DirectState, SourceState};
use crate::detect::ReleaseVerdict;
use crate::model::Candidate;
use crate::wowi::WowiState;

/// Bump when old state can't be understood any more. It's only a cache, so the
/// price is one slow run.
const STATE_SCHEMA: u32 = 1;

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

/// The main folder an entry was last published with, and the former ones the catalog vouched
/// for then.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPublished {
    pub main: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formerly: Vec<String>,
}

/// What each entry was last published as, by lowercased id. Nothing is ever taken out: it's a
/// short line for each entry ever listed, a few hundred kilobytes at most.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Remembered(BTreeMap<String, LastPublished>);

impl Remembered {
    /// Notes what each entry of a catalog that went out was published as. Entries it left out
    /// keep what was noted before.
    pub fn note(&mut self, catalog: &Catalog) {
        for addon in &catalog.addons {
            let Some(main) = addon.folders.first() else {
                continue;
            };
            self.0.insert(
                addon.id.to_ascii_lowercase(),
                LastPublished {
                    main: main.clone(),
                    formerly: addon.formerly.clone(),
                },
            );
        }
    }

    /// What the entry with this id was last published as. Case doesn't matter.
    pub fn get(&self, id: &str) -> Option<&LastPublished> {
        self.0.get(&id.to_ascii_lowercase())
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
                    folders: folders.iter().map(|f| f.to_string()).collect(),
                    formerly: formerly.iter().map(|f| f.to_string()).collect(),
                    ..feeling_core::catalog::Addon::default()
                })
                .collect(),
            ..Catalog::default()
        }
    }

    #[test]
    fn what_each_entry_was_published_as_outlasts_a_catalog_that_leaves_it_out() {
        let mut state = State::empty();
        state.remembered.note(&catalog_of(&[
            ("wowi:7032", &["TomTomForever"], &["TomTom"]),
            ("gh:A/Addon", &["Addon", "Addon_Options"], &[]),
        ]));
        // The next catalog leaves the listing out and moves the repository's main folder.
        state
            .remembered
            .note(&catalog_of(&[("gh:a/addon", &["AddonCore", "Addon"], &[])]));
        let tomtom = state.remembered.get("WOWI:7032").unwrap();
        assert_eq!(tomtom.main, "TomTomForever");
        assert_eq!(tomtom.formerly, ["TomTom"]);
        assert_eq!(
            state.remembered.get("gh:A/Addon").unwrap().main,
            "AddonCore"
        );
        assert_eq!(state.remembered.get("gh:a/other"), None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog-state.json");
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).remembered, state.remembered);

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
