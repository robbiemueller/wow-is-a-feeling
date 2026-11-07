//! Persistent settings and managed-addon records.
//!
//! `state.json` is shared by whatever versions of the app a player runs, so every
//! field has a default and unknown fields are ignored. Saving never leaves a
//! half-written file behind, and a damaged file is set aside rather than overwritten.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::catalog::Channel;
use crate::error::{Error, Result};
use crate::paths;

/// Bump when an old app could no longer make sense of a new file.
pub const STATE_SCHEMA: u32 = 1;

const STATE_FILE: &str = "state.json";
const UI_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=4.0;
const RETRY_DELAYS_MS: [u64; 4] = [20, 60, 150, 300];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub schema: u32,
    pub settings: Settings,
    /// Records per install, by [`crate::detect::Install::key`].
    pub installs: BTreeMap<String, InstallRecord>,
    pub current_install: Option<String>,
    pub catalog_etag: Option<String>,
    pub skip_app_version: Option<String>,
    pub stats: Stats,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            settings: Settings::default(),
            installs: BTreeMap::new(),
            current_install: None,
            catalog_etag: None,
            skip_app_version: None,
            stats: Stats::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallRecord {
    /// Managed addons by addon id.
    pub managed: BTreeMap<String, ManagedAddon>,
    /// Unix seconds. 0 when never backed up.
    pub last_wtf_backup: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedAddon {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(deserialize_with = "lenient")]
    pub channel: Channel,
    /// Folders this addon owns in `Interface/AddOns`.
    pub folders: Vec<String>,
    /// Unix seconds.
    pub installed_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub pinned: bool,
    #[serde(deserialize_with = "lenient")]
    pub origin: Origin,
    /// The zip of the version this one replaced, for rolling back.
    pub previous_zip: Option<PathBuf>,
    pub previous_version: Option<String>,
    /// Folders the catalog lists for this addon that the player let it replace, but that its
    /// zip didn't bring. They're still the player's, so they aren't in `folders`, and updates
    /// don't ask about them again unless a zip brings one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_in_zip: Vec<String>,
}

/// How an addon came to be managed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    Catalog,
    /// Already on disk, then matched to the catalog so it gets updates.
    Adopted,
    LocalZip,
    GithubUrl,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub check_app_updates: bool,
    pub backup_wtf: bool,
    #[serde(deserialize_with = "lenient")]
    pub default_channel: Channel,
    pub keep_previous_version: bool,
    #[serde(deserialize_with = "lenient")]
    pub ui_scale: f32,
    /// Width and height in logical points. `None` until the window has been sized.
    #[serde(deserialize_with = "lenient")]
    pub window_size: Option<[f32; 2]>,
    /// Left and top in logical points. `None` until the window has been moved.
    #[serde(deserialize_with = "lenient")]
    pub window_pos: Option<[f32; 2]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            check_app_updates: true,
            backup_wtf: true,
            default_channel: Channel::Stable,
            keep_previous_version: true,
            ui_scale: 1.0,
            window_size: None,
            window_pos: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Unix seconds. 0 until the first launch is recorded.
    pub first_launch: i64,
    pub launches: u64,
    pub seconds_open: u64,
    pub addons_installed: u64,
    pub updates_applied: u64,
}

/// A value this version doesn't understand becomes the default instead of costing
/// the whole file, because a newer app may have added a channel or an origin. NaN
/// also lands here: JSON stores it as `null`.
fn lenient<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

pub fn state_path() -> PathBuf {
    paths::config_dir().join(STATE_FILE)
}

impl State {
    pub fn load() -> State {
        Self::load_from(&state_path())
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&state_path())
    }

    /// Read the state, falling back to the backup when the file is damaged. When
    /// neither can be used, the bad files are renamed aside (nothing is ever wiped)
    /// and the app starts from defaults.
    pub fn load_from(path: &Path) -> State {
        let main_damaged = match read_state(path) {
            Ok(state) => return state,
            Err(ReadProblem::Missing) => false,
            Err(ReadProblem::Damaged(reason)) => {
                log::warn!("{} is damaged: {reason}", path.display());
                true
            }
        };

        let backup = backup_path(path);
        let backup_damaged = match read_state(&backup) {
            Ok(state) => {
                log::warn!("Restored settings from {}", backup.display());
                return state;
            }
            Err(ReadProblem::Missing) => false,
            Err(ReadProblem::Damaged(reason)) => {
                log::warn!("{} is damaged too: {reason}", backup.display());
                true
            }
        };

        if main_damaged {
            set_aside(path);
        }
        if backup_damaged {
            set_aside(&backup);
        }
        State::default()
    }

    /// Write the state so a crash at any moment leaves a readable file: write a
    /// temp file, flush it to disk, keep the old file as `.bak`, then rename into place.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Parse {
            what: "your settings".to_string(),
            detail: e.to_string(),
        })?;

        let temp = sibling(path, &format!(".{}.tmp", std::process::id()));
        if let Err(e) = write_durably(&temp, &json) {
            let _ = fs::remove_file(&temp);
            return Err(Error::io(&temp, e));
        }

        // A damaged file must never replace a good backup.
        if read_state(path).is_ok() {
            let backup = backup_path(path);
            if let Err(e) = rename_with_retry(path, &backup) {
                log::warn!("Couldn't keep a backup at {}: {e}", backup.display());
            }
        }

        if let Err(e) = rename_with_retry(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(Error::io(path, e));
        }
        sync_parent_folder(path);
        Ok(())
    }

    /// The record of the current install, if one is chosen and known.
    pub fn current(&self) -> Option<&InstallRecord> {
        self.installs.get(self.current_install.as_ref()?)
    }

    /// The record of the current install, created on first use. `None` when no install is chosen.
    pub fn current_mut(&mut self) -> Option<&mut InstallRecord> {
        let key = self.current_install.clone()?;
        Some(self.installs.entry(key).or_default())
    }

    /// Which managed addons claim each folder in the current install, by lowercased
    /// folder name. A folder can have several owners, as shared libraries do.
    pub fn folder_owners(&self) -> HashMap<String, Vec<String>> {
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        let Some(record) = self.current() else {
            return owners;
        };
        for (id, addon) in &record.managed {
            for folder in &addon.folders {
                owners
                    .entry(folder.to_lowercase())
                    .or_default()
                    .push(id.clone());
            }
        }
        owners
    }
}

enum ReadProblem {
    Missing,
    Damaged(String),
}

fn read_state(path: &Path) -> std::result::Result<State, ReadProblem> {
    let bytes = with_retry(|| fs::read(path)).map_err(|e| match e.kind() {
        ErrorKind::NotFound => ReadProblem::Missing,
        _ => ReadProblem::Damaged(e.to_string()),
    })?;
    let mut state: State =
        serde_json::from_slice(&bytes).map_err(|e| ReadProblem::Damaged(e.to_string()))?;
    if !UI_SCALE_RANGE.contains(&state.settings.ui_scale) {
        state.settings.ui_scale = Settings::default().ui_scale;
    }
    Ok(state)
}

fn backup_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn set_aside(path: &Path) {
    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = (0..100u32)
        .map(|n| sibling(path, &format!(".damaged-{unix_now}-{n}")))
        .find(|candidate| !candidate.exists());
    let Some(aside) = aside else {
        log::error!(
            "Too many damaged copies of {} to keep another",
            path.display()
        );
        return;
    };
    match rename_with_retry(path, &aside) {
        Ok(()) => log::warn!("Kept the damaged file as {}", aside.display()),
        Err(e) => log::error!("Couldn't set aside {}: {e}", path.display()),
    }
}

fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    with_retry(|| fs::rename(from, to))
}

/// Antivirus and cloud-sync tools can hold a just-written file for a moment.
fn with_retry<T>(mut operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut delays = RETRY_DELAYS_MS.iter();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(e) => match delays.next() {
                Some(&ms) if e.kind() != ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                _ => return Err(e),
            },
        }
    }
}

/// On Unix a rename only survives a power cut once its folder is flushed too.
#[cfg(unix)]
fn sync_parent_folder(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(folder) = File::open(parent)
    {
        let _ = folder.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_folder(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(id: &str, folders: &[&str]) -> ManagedAddon {
        ManagedAddon {
            id: id.to_string(),
            name: id.to_string(),
            version: "1.0".to_string(),
            folders: folders.iter().map(|f| f.to_string()).collect(),
            ..Default::default()
        }
    }

    fn sample_state() -> State {
        let mut state = State {
            current_install: Some("wow".to_string()),
            catalog_etag: Some("abc".to_string()),
            skip_app_version: Some("0.2.0".to_string()),
            ..Default::default()
        };
        state.settings.ui_scale = 1.5;
        state.settings.window_size = Some([960.0, 640.0]);
        state.settings.default_channel = Channel::Beta;
        state.stats.launches = 7;
        let mut addon = managed("gh:Questie/Questie", &["Questie"]);
        addon.channel = Channel::Beta;
        addon.origin = Origin::Adopted;
        addon.pinned = true;
        addon.previous_zip = Some(PathBuf::from("old.zip"));
        addon.previous_version = Some("0.9".to_string());
        state
            .current_mut()
            .unwrap()
            .managed
            .insert(addon.id.clone(), addon);
        state
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let state = State::default();
        assert_eq!(state.schema, STATE_SCHEMA);
        assert!(state.settings.check_app_updates);
        assert!(state.settings.backup_wtf);
        assert!(state.settings.keep_previous_version);
        assert_eq!(state.settings.ui_scale, 1.0);
        assert_eq!(state.settings.default_channel, Channel::Stable);
        assert_eq!(state.settings.window_size, None);
        assert_eq!(state.stats, Stats::default());
    }

    #[test]
    fn missing_fields_take_their_defaults_at_every_level() {
        let state: State = serde_json::from_str("{}").unwrap();
        assert_eq!(state, State::default());

        let state: State = serde_json::from_str(
            r#"{"settings":{"backup_wtf":false},"installs":{"a":{"managed":{"x":{"name":"X"}}}}}"#,
        )
        .unwrap();
        assert!(!state.settings.backup_wtf);
        assert!(state.settings.check_app_updates);
        assert_eq!(state.settings.ui_scale, 1.0);
        let addon = &state.installs["a"].managed["x"];
        assert_eq!(addon.name, "X");
        assert_eq!(addon.channel, Channel::Stable);
        assert_eq!(addon.origin, Origin::Catalog);
    }

    #[test]
    fn unknown_fields_are_ignored_at_every_level() {
        let state: State = serde_json::from_str(
            r#"{"new_top":1,"settings":{"new_setting":[1,2]},"stats":{"new_stat":3},
               "installs":{"a":{"new_record":true,"managed":{"x":{"new_addon":{"y":1},"name":"X"}}}}}"#,
        )
        .unwrap();
        assert_eq!(state.installs["a"].managed["x"].name, "X");
    }

    #[test]
    fn enum_values_from_a_newer_app_do_not_cost_the_whole_file() {
        let state: State = serde_json::from_str(
            r#"{"settings":{"default_channel":"nightly"},
                "installs":{"a":{"managed":{"x":{"name":"X","channel":"nightly","origin":"from_the_future"}}}}}"#,
        )
        .unwrap();
        assert_eq!(state.settings.default_channel, Channel::Stable);
        let addon = &state.installs["a"].managed["x"];
        assert_eq!(addon.name, "X");
        assert_eq!(addon.channel, Channel::Stable);
        assert_eq!(addon.origin, Origin::Catalog);
    }

    #[test]
    fn round_trips_through_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join(STATE_FILE);
        let state = sample_state();
        state.save_to(&path).unwrap();
        assert_eq!(State::load_from(&path), state);
    }

    #[test]
    fn saving_leaves_no_temp_file_and_keeps_the_previous_file_as_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);

        let mut state = State::default();
        state.stats.launches = 1;
        state.save_to(&path).unwrap();
        assert_eq!(files_in(temp.path()), ["state.json"]);

        state.stats.launches = 2;
        state.save_to(&path).unwrap();
        assert_eq!(files_in(temp.path()), ["state.json", "state.json.bak"]);

        assert_eq!(State::load_from(&path).stats.launches, 2);
        assert_eq!(State::load_from(&backup_path(&path)).stats.launches, 1);
    }

    #[test]
    fn a_missing_file_is_a_first_launch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        assert_eq!(State::load_from(&path), State::default());
        assert!(files_in(temp.path()).is_empty());
    }

    #[test]
    fn a_corrupt_file_falls_back_to_the_backup() {
        for damage in [
            b"{\"settings\": tru".as_slice(),
            b"",
            b"   \n",
            b"[1,2,3]",
            b"\xff\xfe\x00",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(STATE_FILE);
            let state = sample_state();
            state.save_to(&backup_path(&path)).unwrap();
            fs::write(&path, damage).unwrap();
            assert_eq!(State::load_from(&path), state, "damage: {damage:?}");
        }
    }

    #[test]
    fn a_crash_between_the_two_renames_loses_nothing() {
        // The old file has become the backup, and the new one never reached its name.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        let state = sample_state();
        state.save_to(&backup_path(&path)).unwrap();
        fs::write(sibling(&path, ".123.tmp"), b"{\"schema\"").unwrap();
        assert_eq!(State::load_from(&path), state);
    }

    #[test]
    fn when_nothing_is_readable_the_bad_files_are_kept_and_defaults_returned() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        fs::write(&path, b"broken main").unwrap();
        fs::write(backup_path(&path), b"broken backup").unwrap();

        assert_eq!(State::load_from(&path), State::default());
        assert!(!path.exists());
        assert!(!backup_path(&path).exists());

        let kept: Vec<Vec<u8>> = files_in(temp.path())
            .iter()
            .map(|name| fs::read(temp.path().join(name)).unwrap())
            .collect();
        assert!(kept.contains(&b"broken main".to_vec()), "{kept:?}");
        assert!(kept.contains(&b"broken backup".to_vec()), "{kept:?}");

        // The next save starts clean, and doesn't touch the kept copies.
        State::default().save_to(&path).unwrap();
        assert_eq!(State::load_from(&path), State::default());
        assert_eq!(files_in(temp.path()).len(), 3);
    }

    #[test]
    fn set_aside_never_overwrites_an_earlier_damaged_copy() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        for text in ["first", "second", "third"] {
            fs::write(&path, text).unwrap();
            assert_eq!(State::load_from(&path), State::default());
        }
        assert_eq!(files_in(temp.path()).len(), 3);
    }

    #[test]
    fn a_damaged_file_never_replaces_a_good_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        let good = sample_state();
        good.save_to(&backup_path(&path)).unwrap();
        fs::write(&path, b"not json").unwrap();

        State::default().save_to(&path).unwrap();
        assert_eq!(State::load_from(&path), State::default());
        assert_eq!(State::load_from(&backup_path(&path)), good);
    }

    #[test]
    fn a_failed_save_reports_the_path_and_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let path = blocker.join(STATE_FILE);
        let error = State::default().save_to(&path).unwrap_err().to_string();
        assert!(error.contains("blocker"), "{error}");

        // A folder sitting at the target name can't be replaced.
        let path = temp.path().join("taken");
        fs::create_dir(&path).unwrap();
        let error = State::default().save_to(&path).unwrap_err().to_string();
        assert!(error.contains("taken"), "{error}");
        assert_eq!(files_in(temp.path()), ["blocker", "taken"]);
    }

    #[test]
    fn nonsense_numbers_cannot_break_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILE);
        let mut state = State::default();
        state.settings.ui_scale = f32::NAN;
        state.settings.window_size = Some([f32::INFINITY, 640.0]);
        state.stats.launches = 3;
        state.save_to(&path).unwrap();

        let loaded = State::load_from(&path);
        assert_eq!(loaded.stats.launches, 3);
        assert_eq!(loaded.settings.ui_scale, 1.0);
        assert_eq!(loaded.settings.window_size, None);

        fs::write(&path, r#"{"settings":{"ui_scale":1000.0}}"#).unwrap();
        assert_eq!(State::load_from(&path).settings.ui_scale, 1.0);
    }

    #[test]
    fn folder_owners_lowercases_and_lists_every_claim() {
        let mut state = State {
            current_install: Some("wow".to_string()),
            ..Default::default()
        };
        let record = state.current_mut().unwrap();
        record.managed.insert(
            "gh:a/dbm".to_string(),
            managed("gh:a/dbm", &["DBM-Core", "DBM-GUI", "LibStub"]),
        );
        record.managed.insert(
            "wowi:7".to_string(),
            managed("wowi:7", &["libstub", "Other"]),
        );

        let owners = state.folder_owners();
        assert_eq!(owners["dbm-core"], ["gh:a/dbm"]);
        assert_eq!(owners["libstub"], ["gh:a/dbm", "wowi:7"]);
        assert_eq!(owners["other"], ["wowi:7"]);
        assert_eq!(owners.len(), 4);
    }

    #[test]
    fn folder_owners_only_covers_the_current_install() {
        let mut state = State::default();
        state
            .installs
            .entry("other".to_string())
            .or_default()
            .managed
            .insert("wowi:1".to_string(), managed("wowi:1", &["Elsewhere"]));
        assert!(state.folder_owners().is_empty());

        state.current_install = Some("missing".to_string());
        assert!(state.current().is_none());
        assert!(state.folder_owners().is_empty());

        state.current_install = Some("other".to_string());
        assert_eq!(state.folder_owners()["elsewhere"], ["wowi:1"]);
    }

    #[test]
    fn current_mut_needs_a_chosen_install() {
        let mut state = State::default();
        assert!(state.current_mut().is_none());
        state.current_install = Some("wow".to_string());
        assert!(state.current_mut().is_some());
        assert!(state.installs.contains_key("wow"));
    }

    #[test]
    fn origins_have_stable_names_on_disk() {
        for (origin, name) in [
            (Origin::Catalog, "\"catalog\""),
            (Origin::Adopted, "\"adopted\""),
            (Origin::LocalZip, "\"local_zip\""),
            (Origin::GithubUrl, "\"github_url\""),
        ] {
            assert_eq!(serde_json::to_string(&origin).unwrap(), name);
        }
    }
}
