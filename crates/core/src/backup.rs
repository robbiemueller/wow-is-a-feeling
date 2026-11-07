//! WTF settings backup and restore. WTF holds a player's keybindings, macros and every
//! addon's SavedVariables, so it's the one folder here that can't be downloaded again.
//!
//! Backups go through a `.part` file, and a restore moves the current folder aside
//! rather than deleting it, so neither one can leave a player worse off.

use std::cmp::Reverse;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::error::{Error, Result};
use crate::extract::{
    Limits, TreeOptions, display_path, extract_tree, io_error, is_safe_folder_name, long_path,
};
use crate::install::rename_with_retry;

/// SavedVariables never get this big. Anything that does isn't worth a slow backup.
pub const MAX_BACKUP_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DAY_SECONDS: i64 = 86_400;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What [`backup_wtf`] reports as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupProgress {
    Writing {
        done: u64,
        total: u64,
    },
    /// Left out of the backup: too big, in use, or not a plain file.
    Skipped {
        path: String,
        reason: String,
    },
}

/// A backup found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    pub path: PathBuf,
    /// Unix seconds, from the file name.
    pub created: i64,
    pub size: u64,
}

/// Zip `wtf_dir` into `dest_dir/WTF-YYYYMMDD-HHMMSS.zip` (UTC), then keep only the newest
/// `keep` backups, always at least the one just made. Only files named like our backups
/// are ever deleted.
pub fn backup_wtf(
    wtf_dir: &Path,
    dest_dir: &Path,
    keep: usize,
    progress: &mut dyn FnMut(BackupProgress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let wtf = long_path(wtf_dir);
    let dest = long_path(dest_dir);
    fs::create_dir_all(&dest).map_err(|e| io_error(&dest, e))?;
    let files = collect_files(&wtf, progress)?;

    let path = dest.join(free_backup_name(&dest, unix_now()));
    let mut part = path.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let written = write_zip(&part, &files, progress, cancel)
        .and_then(|()| rename_with_retry(&part, &path).map_err(|e| io_error(&path, e)));
    if let Err(error) = written {
        let _ = fs::remove_file(&part);
        return Err(error);
    }
    prune_backups(&dest, keep.max(1));
    Ok(display_path(&path))
}

/// Our backups in `dest_dir`, newest first. A missing folder just has none.
pub fn list_backups(dest_dir: &Path) -> Result<Vec<BackupEntry>> {
    let dest = long_path(dest_dir);
    let items = match fs::read_dir(&dest) {
        Ok(items) => items,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&dest, e)),
    };
    let mut backups: Vec<BackupEntry> = items
        .flatten()
        .filter_map(|item| {
            let created = created_from_name(item.file_name().to_str()?)?;
            let size = item.metadata().ok()?.len();
            Some(BackupEntry {
                path: display_path(&item.path()),
                created,
                size,
            })
        })
        .collect();
    backups.sort_by_key(|backup| Reverse(backup.created));
    Ok(backups)
}

/// At most once a day. A clock that has jumped backward counts as due, because waiting
/// for it to catch up could stop backups for a long time.
pub fn should_backup(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| now < last || now - last >= DAY_SECONDS)
}

/// Replace `wtf_dir` with a backup, after running the zip through the same checks as any
/// other. The current folder moves aside, and its new location is returned (`None` when
/// there wasn't one). Refused while the game runs, because it rewrites these files on exit.
pub fn restore_wtf(zip: &Path, wtf_dir: &Path, game_running: bool) -> Result<Option<PathBuf>> {
    if game_running {
        return Err(Error::GameRunning(
            "the game rewrites its settings when it exits, so a restore now would be lost.".into(),
        ));
    }
    let wtf = long_path(wtf_dir);
    let parent = wtf.parent().ok_or_else(|| {
        Error::Other(format!(
            "Couldn't find the folder of {}.",
            wtf_dir.display()
        ))
    })?;
    let staging = parent.join("WTF.restoring");
    remove_leftover(&staging)?;
    let unpacked = extract_tree(
        zip,
        &staging,
        Limits::default(),
        TreeOptions::default(),
        &mut |_| {},
        &AtomicBool::new(false),
    );
    if let Err(error) = unpacked {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let aside = match fs::symlink_metadata(&wtf) {
        Ok(_) => {
            let aside = free_aside_name(parent, unix_now());
            if let Err(e) = rename_with_retry(&wtf, &aside) {
                let _ = fs::remove_dir_all(&staging);
                return Err(io_error(&wtf, e));
            }
            Some(aside)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error(&wtf, e));
        }
    };
    if let Err(e) = rename_with_retry(&staging, &wtf) {
        if let Some(aside) = &aside {
            let _ = rename_with_retry(aside, &wtf);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(io_error(&wtf, e));
    }
    Ok(aside.map(|path| display_path(&path)))
}

fn remove_leftover(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_error(path, e)),
        _ => Ok(()),
    }
}

struct FileToSave {
    path: PathBuf,
    /// Relative to WTF, with `/` separators.
    name: String,
    size: u64,
    modified: Option<SystemTime>,
}

/// Everything worth saving, in a stable order. Skips what can't be saved or restored,
/// rather than giving up the whole backup over one odd file.
fn collect_files(wtf: &Path, progress: &mut dyn FnMut(BackupProgress)) -> Result<Vec<FileToSave>> {
    let mut files = Vec::new();
    let mut folders = vec![(wtf.to_path_buf(), String::new())];
    let mut skip = |name: String, reason: &str| {
        log::warn!("Left {name} out of a WTF backup: {reason}");
        progress(BackupProgress::Skipped {
            path: name,
            reason: reason.to_string(),
        });
    };
    while let Some((folder, prefix)) = folders.pop() {
        let items = match fs::read_dir(&folder) {
            Ok(items) => items,
            Err(e) if prefix.is_empty() => return Err(io_error(&folder, e)),
            Err(_) => {
                skip(prefix, "the folder couldn't be read");
                continue;
            }
        };
        for item in items.flatten() {
            let raw_name = item.file_name();
            let Some(file_name) = raw_name.to_str().filter(|name| is_safe_folder_name(name)) else {
                skip(
                    format!("{prefix}{}", raw_name.to_string_lossy()),
                    "its name can't be restored everywhere",
                );
                continue;
            };
            let name = format!("{prefix}{file_name}");
            // `DirEntry::metadata` doesn't follow links, which is the point: a link
            // could lead anywhere, including in a circle.
            let Ok(meta) = item.metadata() else {
                skip(name, "it couldn't be read");
                continue;
            };
            if meta.is_dir() {
                folders.push((item.path(), format!("{name}/")));
            } else if !meta.is_file() {
                skip(name, "it's a link, not a file");
            } else if meta.len() > MAX_BACKUP_FILE_BYTES {
                skip(name, "it's over 256 MB");
            } else {
                files.push(FileToSave {
                    path: item.path(),
                    name,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

fn write_zip(
    part: &Path,
    files: &[FileToSave],
    progress: &mut dyn FnMut(BackupProgress),
    cancel: &AtomicBool,
) -> Result<()> {
    let output = File::create(part).map_err(|e| io_error(part, e))?;
    let mut zip = ZipWriter::new(BufWriter::new(output));
    let zip_error = |e: zip::result::ZipError| io_error(part, io::Error::other(e));
    let total: u64 = files.iter().map(|file| file.size).sum();
    let mut done = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut last_report = Instant::now();
    progress(BackupProgress::Writing { done, total });
    for file in files {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut source = match File::open(&file.path) {
            Ok(source) => source,
            Err(error) => {
                log::warn!("Left {} out of a WTF backup: {error}", file.name);
                progress(BackupProgress::Skipped {
                    path: file.name.clone(),
                    reason: "another program has it open".to_string(),
                });
                continue;
            }
        };
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(dos_time(file.modified));
        zip.start_file(file.name.as_str(), options)
            .map_err(zip_error)?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let count = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // Half a settings file is worse than none, so this one comes back out.
                    log::warn!("Left {} out of a WTF backup: {error}", file.name);
                    zip.abort_file().map_err(zip_error)?;
                    progress(BackupProgress::Skipped {
                        path: file.name.clone(),
                        reason: "it couldn't be read".to_string(),
                    });
                    break;
                }
            };
            zip.write_all(&buffer[..count])
                .map_err(|e| io_error(part, e))?;
            done += count as u64;
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                progress(BackupProgress::Writing { done, total });
                last_report = Instant::now();
            }
        }
    }
    let writer = zip.finish().map_err(zip_error)?;
    let output = writer
        .into_inner()
        .map_err(|e| io_error(part, e.into_error()))?;
    output.sync_all().map_err(|e| io_error(part, e))?;
    progress(BackupProgress::Writing { done, total });
    Ok(())
}

fn prune_backups(dest: &Path, keep: usize) {
    let Ok(backups) = list_backups(dest) else {
        return;
    };
    for old in backups.iter().skip(keep) {
        if let Err(error) = fs::remove_file(long_path(&old.path)) {
            log::warn!("Couldn't delete an old WTF backup: {error}");
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Always sorts after every existing backup, even two in one second or after the clock
/// jumps back. Pruning goes by name, so a newer backup with an older name would prune itself.
fn free_backup_name(dest: &Path, now: i64) -> String {
    let newest = list_backups(dest)
        .ok()
        .and_then(|backups| backups.first().map(|b| b.created));
    let stamp = newest.map_or(now, |newest| now.max(newest + 1));
    format!("WTF-{}.zip", timestamp(stamp))
}

fn free_aside_name(parent: &Path, now: i64) -> PathBuf {
    let mut stamp = now;
    loop {
        let path = parent.join(format!("WTF.before-restore-{}", timestamp(stamp)));
        if !path.exists() {
            return path;
        }
        stamp += 1;
    }
}

fn timestamp(unix: i64) -> String {
    let (year, month, day, hour, minute, second) = civil_from_unix(unix);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Parse `WTF-YYYYMMDD-HHMMSS.zip`. Anything else isn't ours.
fn created_from_name(name: &str) -> Option<i64> {
    let stamp = name.strip_prefix("WTF-")?.strip_suffix(".zip")?;
    let (date, time) = stamp.split_once('-')?;
    if date.len() != 8
        || time.len() != 6
        || !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let field = |text: &str, range: std::ops::Range<usize>| -> Option<i64> {
        text.get(range)?.parse().ok()
    };
    let (year, month, day) = (field(date, 0..4)?, field(date, 4..6)?, field(date, 6..8)?);
    let (hour, minute, second) = (field(time, 0..2)?, field(time, 2..4)?, field(time, 4..6)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * DAY_SECONDS + hour * 3600 + minute * 60 + second)
}

/// Zip stores local-looking times with no zone. UTC keeps this free of a time dependency,
/// and these are only ever read by people opening the zip by hand.
fn dos_time(modified: Option<SystemTime>) -> DateTime {
    let unix = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (year, month, day, hour, minute, second) = civil_from_unix(unix);
    u16::try_from(year)
        .ok()
        .and_then(|year| {
            DateTime::from_date_and_time(
                year,
                month as u8,
                day as u8,
                hour as u8,
                minute as u8,
                second as u8,
            )
            .ok()
        })
        .unwrap_or_default()
}

/// Calendar date and time for a Unix timestamp. Howard Hinnant's `civil_from_days`,
/// which every date library is built on, in place of pulling one in for a file name.
fn civil_from_unix(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(DAY_SECONDS);
    let seconds = unix.rem_euclid(DAY_SECONDS);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        (seconds / 3600) as u32,
        (seconds % 3600 / 60) as u32,
        (seconds % 60) as u32,
    )
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::test_zips::{RawEntry, raw_zip, zip_bytes};
    use std::collections::BTreeMap;
    use zip::ZipArchive;

    fn not_cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    struct Wow {
        dir: tempfile::TempDir,
        wtf: PathBuf,
        backups: PathBuf,
    }

    impl Wow {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let wtf = dir.path().join("WTF");
            write_files(
                &wtf,
                &[
                    ("Config.wtf", b"SET gxWindow \"1\""),
                    ("Account/PLAYER/bindings-cache.wtf", b"bind W MOVEFORWARD"),
                    (
                        "Account/PLAYER/SavedVariables/Questie.lua",
                        b"QuestieConfig = {}",
                    ),
                    ("Account/PLAYER/Realm/Character/layout-local.txt", b"layout"),
                ],
            );
            let backups = dir.path().join("backups");
            Wow { dir, wtf, backups }
        }

        fn backup(&self, keep: usize) -> Result<PathBuf> {
            backup_wtf(
                &self.wtf,
                &self.backups,
                keep,
                &mut |_| {},
                &not_cancelled(),
            )
        }
    }

    fn write_files(root: &Path, files: &[(&str, &[u8])]) {
        for (name, body) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for item in fs::read_dir(dir).unwrap().flatten() {
                let name = format!("{prefix}{}", item.file_name().to_string_lossy());
                if item.file_type().unwrap().is_dir() {
                    walk(&item.path(), &format!("{name}/"), out);
                } else {
                    out.insert(name, fs::read(item.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, "", &mut out);
        out
    }

    fn zip_names(path: &Path) -> Vec<String> {
        let archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_backup_restores_to_exactly_what_was_saved() {
        let wow = Wow::new();
        let saved = tree(&wow.wtf);
        let zip = wow.backup(5).unwrap();
        let name = zip.file_name().unwrap().to_str().unwrap();
        assert!(created_from_name(name).is_some(), "{name}");
        assert_eq!(
            zip_names(&zip),
            [
                "Account/PLAYER/Realm/Character/layout-local.txt",
                "Account/PLAYER/SavedVariables/Questie.lua",
                "Account/PLAYER/bindings-cache.wtf",
                "Config.wtf",
            ]
        );

        fs::write(wow.wtf.join("Config.wtf"), b"ruined").unwrap();
        fs::remove_dir_all(wow.wtf.join("Account")).unwrap();
        let ruined = tree(&wow.wtf);

        let aside = restore_wtf(&zip, &wow.wtf, false).unwrap().unwrap();
        assert_eq!(tree(&wow.wtf), saved);
        assert_eq!(
            tree(&aside),
            ruined,
            "the current folder must move aside intact"
        );
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("WTF.before-restore-")
        );
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let wow = Wow::new();
        let mut made = Vec::new();
        for _ in 0..5 {
            made.push(wow.backup(3).unwrap());
        }
        let left = list_backups(&wow.backups).unwrap();
        assert_eq!(left.len(), 3);
        assert_eq!(left[0].path, made[4]);
        assert_eq!(left[2].path, made[2]);
        assert!(left[0].created > left[1].created);
        assert!(left[0].size > 0);
    }

    #[test]
    fn a_new_backup_sorts_newest_even_if_the_clock_went_back() {
        let wow = Wow::new();
        fs::create_dir_all(&wow.backups).unwrap();
        let future = wow.backups.join("WTF-20991231-235959.zip");
        fs::write(&future, b"from a wrong clock").unwrap();
        let zip = wow.backup(1).unwrap();
        assert!(zip.exists(), "the backup just made must survive pruning");
        assert!(!future.exists());
        assert_eq!(list_backups(&wow.backups).unwrap()[0].path, zip);
    }

    #[test]
    fn keep_zero_still_keeps_the_new_backup() {
        let wow = Wow::new();
        let zip = wow.backup(0).unwrap();
        assert!(zip.exists());
    }

    #[test]
    fn pruning_only_touches_files_named_like_our_backups() {
        let wow = Wow::new();
        fs::create_dir_all(&wow.backups).unwrap();
        for foreign in [
            "notes.txt",
            "WTF-old.zip",
            "WTF-20201301-000000.zip",
            "my-WTF-20200101-000000.zip",
        ] {
            fs::write(wow.backups.join(foreign), b"mine").unwrap();
        }
        wow.backup(1).unwrap();
        wow.backup(1).unwrap();
        for foreign in [
            "notes.txt",
            "WTF-old.zip",
            "WTF-20201301-000000.zip",
            "my-WTF-20200101-000000.zip",
        ] {
            assert!(wow.backups.join(foreign).exists(), "{foreign} was deleted");
        }
        assert_eq!(list_backups(&wow.backups).unwrap().len(), 1);
    }

    #[test]
    fn files_over_the_limit_are_skipped_and_reported() {
        let wow = Wow::new();
        let big = File::create(wow.wtf.join("huge.lua")).unwrap();
        big.set_len(MAX_BACKUP_FILE_BYTES + 1).unwrap();
        drop(big);
        let exact = File::create(wow.wtf.join("fits.lua")).unwrap();
        exact.set_len(10).unwrap();
        drop(exact);

        let mut events = Vec::new();
        let zip = backup_wtf(
            &wow.wtf,
            &wow.backups,
            5,
            &mut |e| events.push(e),
            &not_cancelled(),
        )
        .unwrap();
        let names = zip_names(&zip);
        assert!(!names.contains(&"huge.lua".to_string()), "{names:?}");
        assert!(names.contains(&"fits.lua".to_string()), "{names:?}");
        assert!(events.iter().any(|event| matches!(
            event,
            BackupProgress::Skipped { path, reason } if path == "huge.lua" && reason.contains("256")
        )));
    }

    #[test]
    fn progress_adds_up() {
        let wow = Wow::new();
        let total: u64 = tree(&wow.wtf).values().map(|body| body.len() as u64).sum();
        let mut events = Vec::new();
        backup_wtf(
            &wow.wtf,
            &wow.backups,
            5,
            &mut |e| events.push(e),
            &not_cancelled(),
        )
        .unwrap();
        assert_eq!(
            events.first(),
            Some(&BackupProgress::Writing { done: 0, total })
        );
        assert_eq!(
            events.last(),
            Some(&BackupProgress::Writing { done: total, total })
        );
    }

    #[test]
    fn cancelling_leaves_no_part_or_zip() {
        let wow = Wow::new();
        let cancel = AtomicBool::new(true);
        let result = backup_wtf(&wow.wtf, &wow.backups, 5, &mut |_| {}, &cancel);
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(fs::read_dir(&wow.backups).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_wtf_folder_is_an_error_and_leaves_nothing() {
        let wow = Wow::new();
        fs::remove_dir_all(&wow.wtf).unwrap();
        assert!(matches!(wow.backup(5), Err(Error::Io { .. })));
        assert_eq!(fs::read_dir(&wow.backups).unwrap().count(), 0);
    }

    #[test]
    fn an_empty_wtf_folder_makes_a_valid_empty_backup() {
        let wow = Wow::new();
        fs::remove_dir_all(&wow.wtf).unwrap();
        fs::create_dir_all(&wow.wtf).unwrap();
        let zip = wow.backup(5).unwrap();
        assert!(zip_names(&zip).is_empty());
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert!(tree(&wow.wtf).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_another_program_holds_is_skipped_not_fatal() {
        use std::os::windows::fs::OpenOptionsExt;

        let wow = Wow::new();
        let held = wow.wtf.join("Config.wtf");
        let mut attempts = 0;
        let _handle = loop {
            match File::options().read(true).share_mode(0).open(&held) {
                Ok(handle) => break handle,
                Err(_) if attempts < 100 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => panic!("{error}"),
            }
        };
        let mut events = Vec::new();
        let zip = backup_wtf(
            &wow.wtf,
            &wow.backups,
            5,
            &mut |e| events.push(e),
            &not_cancelled(),
        )
        .unwrap();
        assert!(!zip_names(&zip).contains(&"Config.wtf".to_string()));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackupProgress::Skipped { path, .. } if path == "Config.wtf"))
        );
    }

    fn link_folder(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(output.status.success(), "mklink failed");
        }
    }

    #[test]
    fn linked_folders_are_never_followed() {
        let wow = Wow::new();
        let elsewhere = wow.dir.path().join("elsewhere");
        write_files(&elsewhere, &[("secret.txt", b"not settings")]);
        link_folder(&elsewhere, &wow.wtf.join("Linked"));
        // A link to its own parent would otherwise recurse forever.
        link_folder(&wow.wtf, &wow.wtf.join("Loop"));

        let mut events = Vec::new();
        let zip = backup_wtf(
            &wow.wtf,
            &wow.backups,
            5,
            &mut |e| events.push(e),
            &not_cancelled(),
        )
        .unwrap();
        let names = zip_names(&zip);
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with("Linked") || name.starts_with("Loop")),
            "{names:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackupProgress::Skipped { path, .. } if path == "Linked"))
        );
    }

    #[test]
    fn restore_refuses_while_the_game_runs() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let before = tree(&wow.wtf);
        fs::write(wow.wtf.join("Config.wtf"), b"changed").unwrap();
        let changed = tree(&wow.wtf);
        let result = restore_wtf(&zip, &wow.wtf, true);
        assert!(matches!(result, Err(Error::GameRunning(_))));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("Close WoW first")
        );
        assert_eq!(tree(&wow.wtf), changed);
        assert_ne!(before, changed);
    }

    #[test]
    fn a_malicious_zip_changes_nothing() {
        for evil in [
            "../evil.lua",
            "Account/../../evil.lua",
            "C:/evil.lua",
            "a/CON",
            "x:y",
            "/abs",
        ] {
            let wow = Wow::new();
            let before = tree(&wow.wtf);
            let zip = wow.dir.path().join("evil.zip");
            fs::write(
                &zip,
                raw_zip(&[
                    RawEntry::new("Config.wtf", b"ok"),
                    RawEntry::new(evil, b"x"),
                ]),
            )
            .unwrap();
            let result = restore_wtf(&zip, &wow.wtf, false);
            assert!(
                matches!(result, Err(Error::UnsafeZip(_))),
                "{evil}: {result:?}"
            );
            assert_eq!(tree(&wow.wtf), before, "{evil}");
            assert!(!wow.dir.path().join("evil.lua").exists());
            let names: Vec<_> = fs::read_dir(wow.dir.path())
                .unwrap()
                .flatten()
                .map(|i| i.file_name())
                .collect();
            assert!(
                !names
                    .iter()
                    .any(|name| name.to_string_lossy().contains("restor")),
                "{names:?}"
            );
        }
    }

    #[test]
    fn a_damaged_zip_changes_nothing() {
        let wow = Wow::new();
        let before = tree(&wow.wtf);
        let zip = wow.dir.path().join("bad.zip");
        fs::write(&zip, b"not a zip").unwrap();
        assert!(matches!(
            restore_wtf(&zip, &wow.wtf, false),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(tree(&wow.wtf), before);
    }

    #[test]
    fn restore_works_without_a_current_folder() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        fs::remove_dir_all(&wow.wtf).unwrap();
        assert_eq!(restore_wtf(&zip, &wow.wtf, false).unwrap(), None);
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn repeated_restores_keep_every_moved_aside_copy() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let first = restore_wtf(&zip, &wow.wtf, false).unwrap().unwrap();
        let second = restore_wtf(&zip, &wow.wtf, false).unwrap().unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
    }

    #[test]
    fn a_leftover_from_a_crashed_restore_is_cleared() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        write_files(
            &wow.dir.path().join("WTF.restoring"),
            &[("stale.txt", b"old")],
        );
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn restoring_a_zip_made_elsewhere_works() {
        let wow = Wow::new();
        let zip = wow.dir.path().join("other.zip");
        fs::write(&zip, zip_bytes(&[("Config.wtf", b"from elsewhere")])).unwrap();
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(
            fs::read(wow.wtf.join("Config.wtf")).unwrap(),
            b"from elsewhere"
        );
    }

    #[test]
    fn listing_a_missing_folder_is_empty() {
        let wow = Wow::new();
        assert!(list_backups(&wow.backups).unwrap().is_empty());
    }

    #[test]
    fn should_backup_at_most_once_a_day() {
        let t = 1_794_000_000;
        assert!(should_backup(None, t));
        assert!(!should_backup(Some(t), t));
        assert!(!should_backup(Some(t), t + 3600));
        assert!(!should_backup(Some(t), t + DAY_SECONDS - 1));
        assert!(should_backup(Some(t), t + DAY_SECONDS));
        assert!(should_backup(Some(t), t + 30 * DAY_SECONDS));
        assert!(should_backup(Some(t), t - 1));
    }

    #[test]
    fn dates_convert_both_ways() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(1_794_000_000), (2026, 11, 6, 21, 20, 0));
        assert_eq!(civil_from_unix(1_735_689_599), (2024, 12, 31, 23, 59, 59));
        assert_eq!(civil_from_unix(-1), (1969, 12, 31, 23, 59, 59));
        for unix in [0, 86_399, 951_782_400, 1_794_000_000, 4_102_444_800] {
            let name = format!("WTF-{}.zip", timestamp(unix));
            assert_eq!(created_from_name(&name), Some(unix), "{name}");
        }
    }

    #[test]
    fn backup_names_are_recognized_strictly() {
        assert_eq!(created_from_name("WTF-19700101-000000.zip"), Some(0));
        for name in [
            "WTF-1970010-000000.zip",
            "WTF-19700101-00000.zip",
            "WTF-19701301-000000.zip",
            "WTF-19700132-000000.zip",
            "WTF-19700101-240000.zip",
            "WTF-19700101-000000.zip.part",
            "wtf-19700101-000000.zip",
            "WTF-abcdefgh-000000.zip",
            "WTF-19700101.zip",
        ] {
            assert_eq!(created_from_name(name), None, "{name}");
        }
    }

    #[test]
    fn zip_times_handle_the_dos_range() {
        let in_range = UNIX_EPOCH + Duration::from_secs(1_794_000_000);
        assert_eq!(dos_time(Some(in_range)).year(), 2026);
        // Before 1980 can't be stored, and that's no reason to fail a backup.
        assert_eq!(dos_time(Some(UNIX_EPOCH)), DateTime::default());
        assert_eq!(dos_time(None), DateTime::default());
    }
}
