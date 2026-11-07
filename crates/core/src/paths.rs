//! Where the app keeps its files. Asking for a path never touches the disk and
//! never fails, so callers can ask as often as they like.

use std::path::{Path, PathBuf};

const APP_DIR: &str = "wow-is-a-feeling";

/// When set, all four folders live under this one, so a test run can't touch a player's
/// real settings.
const HOME_ENV: &str = "WIAF_HOME";

/// Settings and managed-addon records. Roams with the Windows profile.
pub fn config_dir() -> PathBuf {
    resolve(dirs::config_dir(), "config")
}

/// Things that are safe to lose, such as the downloaded catalog.
pub fn cache_dir() -> PathBuf {
    resolve(dirs::cache_dir(), "cache")
}

/// Larger files we want to keep, such as previous addon zips. Machine-local on
/// Windows, so they never get copied around with a roaming profile.
pub fn data_dir() -> PathBuf {
    resolve(dirs::data_local_dir(), "data")
}

pub fn log_dir() -> PathBuf {
    resolve(dirs::data_local_dir(), "logs")
}

fn resolve(base: Option<PathBuf>, kind: &str) -> PathBuf {
    under_home(std::env::var_os(HOME_ENV), base, kind)
}

fn under_home(home: Option<std::ffi::OsString>, base: Option<PathBuf>, kind: &str) -> PathBuf {
    match home.filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join(kind),
        None => app_dir(base, kind),
    }
}

/// Each kind gets its own subfolder because the OS bases can coincide (on Windows the
/// cache and data bases are the same folder), and clearing a cache must not take the
/// player's data with it.
fn app_dir(base: Option<PathBuf>, kind: &str) -> PathBuf {
    base.unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
        .join(kind)
}

/// Create `path` and its parents if needed, and hand the path back for chaining.
/// A failure is only logged, because the file operation that follows reports it
/// to the player with the exact path that couldn't be written.
pub fn ensure_dir(path: &Path) -> &Path {
    if let Err(e) = std::fs::create_dir_all(path) {
        log::warn!("Couldn't create {}: {e}", path.display());
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dir_sits_under_the_app_folder_and_none_are_shared() {
        let dirs = [config_dir(), cache_dir(), data_dir(), log_dir()];
        for dir in &dirs {
            assert!(
                dir.components().any(|c| c.as_os_str() == APP_DIR),
                "{} is not under {APP_DIR}",
                dir.display()
            );
        }
        for (i, dir) in dirs.iter().enumerate() {
            assert!(!dirs[i + 1..].contains(dir), "{} is shared", dir.display());
        }
    }

    #[test]
    fn a_home_override_holds_every_kind_and_an_empty_one_is_ignored() {
        let base = || Some(PathBuf::from("base"));
        let kinds = ["config", "cache", "data", "logs"];
        for kind in kinds {
            assert_eq!(
                under_home(Some("test-home".into()), base(), kind),
                Path::new("test-home").join(kind)
            );
            assert_eq!(
                under_home(Some("".into()), base(), kind),
                app_dir(base(), kind)
            );
            assert_eq!(under_home(None, base(), kind), app_dir(base(), kind));
        }
    }

    #[test]
    fn a_known_base_is_used() {
        let base = PathBuf::from("base");
        assert_eq!(
            app_dir(Some(base.clone()), "config"),
            base.join(APP_DIR).join("config")
        );
    }

    #[test]
    fn a_missing_base_falls_back_to_temp() {
        let config = app_dir(None, "config");
        assert!(config.starts_with(std::env::temp_dir().join(APP_DIR)));
        assert_ne!(config, app_dir(None, "cache"));
    }

    #[test]
    fn ensure_dir_creates_nested_folders_and_returns_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("a").join("b");
        assert_eq!(ensure_dir(&nested), nested);
        assert!(nested.is_dir());
        // A second call on an existing folder is fine.
        assert_eq!(ensure_dir(&nested), nested);
    }

    #[test]
    fn ensure_dir_survives_a_path_it_cannot_create() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let below_a_file = file.join("child");
        assert_eq!(ensure_dir(&below_a_file), below_a_file);
        assert!(!below_a_file.exists());
    }
}
