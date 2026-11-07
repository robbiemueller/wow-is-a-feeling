//! WTF settings backup and restore. WTF holds a player's keybindings, macros and every
//! addon's SavedVariables, so it's the one folder here that can't be downloaded again.
//!
//! A backup goes through a `.part` file and only counts once a restore would accept it, so
//! a bad one can't push out good ones. A restore moves the current folder aside rather than
//! deleting it, and its folder names say how far it got, so [`recover_restore`] can put
//! things right after a crash. Neither one can leave a player worse off.

use std::cmp::Reverse;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::error::{Error, Result};
use crate::extract::{
    Limits, TreeOptions, check_tree, display_path, entry_name_fits, extract_tree, io_error,
    is_safe_folder_name, long_path,
};
use crate::install::{is_present, rename_with_retry};

/// SavedVariables never get this big. Anything that does isn't worth a slow backup.
pub const MAX_BACKUP_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// A backup has no total of its own, so this is only here to stop a zip made elsewhere from
/// filling a disk. No real WTF comes near it.
const MAX_RESTORE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const DAY_SECONDS: i64 = 86_400;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Unpacking goes here. It only outlives a restore that was interrupted.
const STAGING_PREFIX: &str = "WTF.restoring-";
/// The folder a restore replaced, kept for good.
const ASIDE_PREFIX: &str = "WTF.before-restore-";
/// Held while a restore runs, because to [`recover_restore`] a live restore's folders would
/// look like an interrupted one's. Only one copy of the app writes, so this process is enough.
static RESTORE_RUNNING: Mutex<()> = Mutex::new(());

/// What a restore accepts: a player's own files, not a stranger's zip. An addon that saved
/// one long string compresses better than any zip bomb limit, and a heavy player's WTF can
/// outgrow what's sensible for an addon. Neither is a reason to refuse all of someone's
/// settings. Paths, links, programs and the size of any one file are checked as usual.
fn restore_limits() -> Limits {
    Limits {
        max_total_bytes: MAX_RESTORE_BYTES,
        max_ratio: u64::MAX,
        ..Limits::default()
    }
}

fn restore_options() -> TreeOptions {
    TreeOptions {
        settings_backup: true,
        ..TreeOptions::default()
    }
}

/// A refusal in words about backups, because the zip checks' own say "addon" and "download".
fn backup_refusal(error: Error, start: &str, end: &str) -> Error {
    match error {
        Error::UnsafeZip(reason) => Error::Other(format!("{start}: {reason} {end}")),
        Error::Corrupt(reason) => Error::Other(format!("{start}: {reason}. {end}")),
        other => other,
    }
}

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
/// are ever deleted. A zip that [`restore_wtf`] would refuse isn't kept and doesn't count:
/// it's an error, and the older backups stay.
pub fn backup_wtf(
    wtf_dir: &Path,
    dest_dir: &Path,
    keep: usize,
    progress: &mut dyn FnMut(BackupProgress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    backup_within(wtf_dir, dest_dir, keep, restore_limits(), progress, cancel)
}

fn backup_within(
    wtf_dir: &Path,
    dest_dir: &Path,
    keep: usize,
    restorable_within: Limits,
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
        .and_then(|()| {
            check_tree(&part, restorable_within, restore_options()).map_err(|error| {
                backup_refusal(
                    error,
                    "This settings backup wasn't saved, because it couldn't be restored",
                    "Your older backups were kept.",
                )
            })
        })
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
    let _only_one = RESTORE_RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // Otherwise this would lose track of the settings an unfinished restore moved aside.
    settle_unfinished_restores(&wtf, parent)?;
    let stamp = free_restore_stamp(parent, unix_now());
    let staging = parent.join(format!("{STAGING_PREFIX}{stamp}"));
    let aside = parent.join(format!("{ASIDE_PREFIX}{stamp}"));
    let unpacked = extract_tree(
        zip,
        &staging,
        restore_limits(),
        restore_options(),
        &mut |_| {},
        &AtomicBool::new(false),
    );
    if let Err(error) = unpacked {
        let _ = fs::remove_dir_all(&staging);
        return Err(backup_refusal(
            error,
            "This backup can't be restored",
            "Your settings weren't changed. Try an older backup.",
        ));
    }

    let had_folder = match fs::symlink_metadata(&wtf) {
        Ok(_) => {
            if let Err(e) = rename_with_retry(&wtf, &aside) {
                let _ = fs::remove_dir_all(&staging);
                return Err(io_error(&wtf, e));
            }
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error(&wtf, e));
        }
    };
    if let Err(e) = rename_with_retry(&staging, &wtf) {
        if had_folder && let Err(back) = rename_with_retry(&aside, &wtf) {
            // Staging stays, because it's what tells the next start this restore never ended.
            log::error!("Couldn't restore WTF ({e}) or put it back ({back})");
            return Err(not_put_back(&aside, &back));
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(io_error(&wtf, e));
    }
    Ok(had_folder.then(|| display_path(&aside)))
}

/// Settle a restore that never finished, because the app was closed or died partway, or
/// because a file stayed locked. Run this at startup. The folder names say how far it got:
/// staging only outlives an interrupted restore, and a moved-aside folder with its stamp
/// means the real settings had already left. Returns one line per interrupted restore.
///
/// Folders moved aside by a restore that did finish are left alone, so a player who deletes
/// WTF on purpose to reset it doesn't get old settings back.
pub fn recover_restore(wtf_dir: &Path) -> Result<Vec<String>> {
    let wtf = long_path(wtf_dir);
    let Some(parent) = wtf.parent() else {
        return Ok(Vec::new());
    };
    let _only_one = RESTORE_RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    settle_unfinished_restores(&wtf, parent)
}

fn settle_unfinished_restores(wtf: &Path, parent: &Path) -> Result<Vec<String>> {
    let Ok(items) = fs::read_dir(parent) else {
        return Ok(Vec::new());
    };
    let mut stamps: Vec<String> = items
        .flatten()
        .filter_map(|item| {
            let name = item.file_name();
            Some(name.to_str()?.strip_prefix(STAGING_PREFIX)?.to_string())
        })
        .collect();
    // Newest first in the odd case of several, because that one moved the folder last.
    stamps.sort_by(|a, b| b.cmp(a));
    let mut notes = Vec::new();
    for stamp in stamps {
        let aside = parent.join(format!("{ASIDE_PREFIX}{stamp}"));
        if is_present(&aside).map_err(|e| io_error(&aside, e))? {
            if is_present(wtf).map_err(|e| io_error(wtf, e))? {
                // The game made a new one in the meantime, which may have settings of its
                // own by now, so that's the player's call.
                notes.push(format!(
                    "A settings restore was interrupted. The settings you had before it are in {}.",
                    display_path(&aside).display()
                ));
            } else {
                rename_with_retry(&aside, wtf).map_err(|e| not_put_back(&aside, &e))?;
                notes.push(
                    "A settings restore was interrupted, so your settings were put back the \
                     way they were."
                        .to_string(),
                );
            }
        }
        let staging = parent.join(format!("{STAGING_PREFIX}{stamp}"));
        if let Err(error) = fs::remove_dir_all(&staging) {
            log::warn!("Couldn't clear what an interrupted restore left: {error}");
        }
    }
    // These may be shown only once, and the old settings aren't going anywhere.
    notes.iter().for_each(|note| log::warn!("{note}"));
    Ok(notes)
}

fn not_put_back(aside: &Path, error: &io::Error) -> Error {
    Error::Other(format!(
        "Couldn't finish restoring your settings or put the old ones back yet ({error}). \
         Nothing was deleted, and your old settings are in {}. Close anything that might be \
         using them and restart this app, or rename that folder to WTF yourself.",
        display_path(aside).display()
    ))
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
            if !entry_name_fits(&name) {
                skip(name, "its path is too long to restore");
                continue;
            }
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

/// A stamp no other restore has used. Staging and the moved-aside folder share it, which is
/// how [`recover_restore`] tells them apart from those of finished restores.
fn free_restore_stamp(parent: &Path, now: i64) -> String {
    let mut stamp = now;
    loop {
        let text = timestamp(stamp);
        let is_free = |prefix: &str| !parent.join(format!("{prefix}{text}")).exists();
        if is_free(ASIDE_PREFIX) && is_free(STAGING_PREFIX) {
            return text;
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
    use crate::install::fault::{self, Plan};
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
                matches!(&result, Err(Error::Other(text)) if text.starts_with("This backup can't")),
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
        let text = restore_wtf(&zip, &wow.wtf, false).unwrap_err().to_string();
        assert!(text.starts_with("This backup can't be restored"), "{text}");
        assert!(
            !text.contains("download") && !text.contains("addon"),
            "{text}"
        );
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
        let staging = wow.dir.path().join("WTF.restoring-20200101-000000");
        write_files(&staging, &[("stale.txt", b"old")]);
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(tree(&wow.wtf), saved);
        assert!(!staging.exists());
    }

    fn names_beside_wtf(wow: &Wow) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(wow.dir.path())
            .unwrap()
            .flatten()
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("WTF"))
            .collect();
        names.sort();
        names
    }

    fn only_aside(wow: &Wow) -> PathBuf {
        let asides: Vec<String> = names_beside_wtf(wow)
            .into_iter()
            .filter(|name| name.starts_with(ASIDE_PREFIX))
            .collect();
        assert_eq!(asides.len(), 1, "{asides:?}");
        wow.dir.path().join(&asides[0])
    }

    /// The second rename fails and the way back fails too, or the process dies between them.
    /// Either way what's left on disk is the same.
    fn restore_stopped_between_the_renames(wow: &Wow, zip: &Path) -> Error {
        fault::set(Plan {
            fail_at: vec![1, 2],
            ..Plan::default()
        });
        let error = restore_wtf(zip, &wow.wtf, false).unwrap_err();
        fault::take();
        error
    }

    #[test]
    fn when_the_swap_and_the_way_back_both_fail_the_error_says_where_the_settings_are() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        let text = restore_stopped_between_the_renames(&wow, &zip).to_string();

        assert!(!wow.wtf.exists());
        let aside = only_aside(&wow);
        assert_eq!(tree(&aside), saved, "the old settings must be intact");
        assert!(text.contains(&*aside.to_string_lossy()), "{text}");
        assert!(text.contains("rename that folder to WTF"), "{text}");
    }

    #[test]
    fn recovery_puts_the_old_settings_back_after_an_interrupted_restore() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        fs::write(wow.wtf.join("Config.wtf"), b"changed since that backup").unwrap();
        let current = tree(&wow.wtf);
        restore_stopped_between_the_renames(&wow, &zip);

        let notes = recover_restore(&wow.wtf).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("put back"), "{notes:?}");
        assert_eq!(tree(&wow.wtf), current);
        assert_ne!(tree(&wow.wtf), saved);
        assert_eq!(names_beside_wtf(&wow), ["WTF"]);

        // And it only has to say so once.
        assert!(recover_restore(&wow.wtf).unwrap().is_empty());
    }

    #[test]
    fn recovery_that_cant_finish_yet_says_where_the_settings_are_and_tries_again() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        restore_stopped_between_the_renames(&wow, &zip);

        fault::set(Plan {
            fail_at: vec![0],
            ..Plan::default()
        });
        let text = recover_restore(&wow.wtf).unwrap_err().to_string();
        fault::take();
        assert!(
            text.contains(&*only_aside(&wow).to_string_lossy()),
            "{text}"
        );
        assert!(!wow.wtf.exists());

        assert_eq!(recover_restore(&wow.wtf).unwrap().len(), 1);
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn pressing_restore_again_after_an_interrupted_one_finishes_and_keeps_the_old_settings() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        fs::write(wow.wtf.join("Config.wtf"), b"changed since that backup").unwrap();
        let current = tree(&wow.wtf);
        restore_stopped_between_the_renames(&wow, &zip);

        let aside = restore_wtf(&zip, &wow.wtf, false).unwrap().unwrap();
        assert_eq!(tree(&wow.wtf), saved);
        assert_eq!(tree(&aside), current);
        assert_eq!(
            names_beside_wtf(&wow).len(),
            2,
            "{:?}",
            names_beside_wtf(&wow)
        );
    }

    #[test]
    fn a_new_wtf_after_an_interrupted_restore_stays_and_the_player_is_told_where_the_old_is() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        restore_stopped_between_the_renames(&wow, &zip);
        // The game ran in between and made itself a default folder.
        write_files(&wow.wtf, &[("Config.wtf", b"defaults")]);
        let defaults = tree(&wow.wtf);

        let notes = recover_restore(&wow.wtf).unwrap();
        let aside = only_aside(&wow);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains(&*aside.to_string_lossy()), "{notes:?}");
        assert_eq!(tree(&wow.wtf), defaults);
        assert_eq!(tree(&aside), saved);
        assert_eq!(
            names_beside_wtf(&wow).len(),
            2,
            "{:?}",
            names_beside_wtf(&wow)
        );
    }

    #[test]
    fn recovery_leaves_folders_from_finished_restores_alone() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let aside = restore_wtf(&zip, &wow.wtf, false).unwrap().unwrap();
        assert!(recover_restore(&wow.wtf).unwrap().is_empty());
        assert!(aside.is_dir() && wow.wtf.is_dir());

        // A player clearing WTF to reset it doesn't get the old one back, not even with a
        // crash during unpacking left beside it.
        fs::remove_dir_all(&wow.wtf).unwrap();
        write_files(
            &wow.dir.path().join("WTF.restoring-20200101-000000"),
            &[("half.lua", b"half")],
        );
        assert!(recover_restore(&wow.wtf).unwrap().is_empty());
        assert!(!wow.wtf.exists());
        assert!(aside.is_dir());
        assert_eq!(names_beside_wtf(&wow).len(), 1);
    }

    /// Picking the same game folder again mid-restore runs recovery, and a live restore's
    /// folders look just like an interrupted one's.
    #[test]
    fn recovery_waits_for_a_restore_that_is_still_running() {
        let wow = Wow::new();
        let zip = wow.backup(5).unwrap();
        let saved = tree(&wow.wtf);
        restore_stopped_between_the_renames(&wow, &zip);

        let running = RESTORE_RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let wtf = wow.wtf.clone();
        let recovery = std::thread::spawn(move || recover_restore(&wtf));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!wow.wtf.exists(), "recovery must wait its turn");
        drop(running);

        assert_eq!(recovery.join().unwrap().unwrap().len(), 1);
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn recovery_with_no_game_folder_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not here").join("WTF");
        assert!(recover_restore(&missing).unwrap().is_empty());
        assert!(!missing.parent().unwrap().exists());
    }

    /// A file like this comes from a power cut, or from an addon that saved one long string.
    #[test]
    fn a_settings_file_of_one_repeated_byte_backs_up_and_restores() {
        let wow = Wow::new();
        let size = 65 * 1024 * 1024;
        let zeros = wow.wtf.join("Account/PLAYER/SavedVariables/Zeros.lua");
        File::create(&zeros).unwrap().set_len(size).unwrap();
        let zip = wow.backup(5).unwrap();
        fs::write(wow.wtf.join("Config.wtf"), b"changed").unwrap();

        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(fs::metadata(&zeros).unwrap().len(), size);
        assert_eq!(
            fs::read(wow.wtf.join("Config.wtf")).unwrap(),
            b"SET gxWindow \"1\""
        );
    }

    /// Going by what's really on this disk: every name stays on Linux, the Kelvin sign and the
    /// `k` stay apart on NTFS though they fold alike, and nothing collides on the rest.
    #[test]
    fn names_that_differ_only_by_case_all_come_back_on_a_disk_that_holds_them() {
        let wow = Wow::new();
        let saved_dir = wow.wtf.join("Account/PLAYER/SavedVariables");
        for (name, body) in [
            ("keep.lua", &b"letter"[..]),
            ("Keep.lua", b"capital"),
            ("\u{212A}eep.lua", b"sign"),
        ] {
            fs::write(saved_dir.join(name), body).unwrap();
        }
        let saved = tree(&wow.wtf);
        let zip = wow.backup(5).unwrap();
        fs::write(wow.wtf.join("Config.wtf"), b"changed").unwrap();

        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(tree(&wow.wtf), saved);
    }

    /// When the disk can't hold both, that's a refusal with nothing changed, and the zip is
    /// still a complete copy for anyone opening it by hand.
    #[test]
    fn names_that_collide_on_this_disk_refuse_the_restore_and_change_nothing() {
        let wow = Wow::new();
        let probe = wow.dir.path().join("case-probe");
        fs::write(&probe, b"").unwrap();
        let folds_case = wow.dir.path().join("CASE-PROBE").exists();

        let zip = wow.dir.path().join("elsewhere.zip");
        fs::write(
            &zip,
            raw_zip(&[
                RawEntry::new("foo.lua", b"one"),
                RawEntry::new("Foo.lua", b"two"),
            ]),
        )
        .unwrap();
        let before = tree(&wow.wtf);
        let result = restore_wtf(&zip, &wow.wtf, false);
        if folds_case {
            let text = result.unwrap_err().to_string();
            assert!(text.starts_with("This backup can't be restored"), "{text}");
            assert_eq!(tree(&wow.wtf), before);
            assert_eq!(names_beside_wtf(&wow), ["WTF"]);
        } else {
            result.unwrap();
            assert_eq!(fs::read(wow.wtf.join("foo.lua")).unwrap(), b"one");
            assert_eq!(fs::read(wow.wtf.join("Foo.lua")).unwrap(), b"two");
        }
    }

    #[test]
    fn a_wtf_over_the_total_limit_for_addons_can_still_be_restored() {
        // Nine files at the most a backup keeps, the case from the review. Only what they
        // declare matters to these checks, which saves the test writing 2 GB.
        let wow = Wow::new();
        let entries: Vec<RawEntry> = (0..9)
            .map(|i| {
                let mut entry = RawEntry::new(format!("big-{i}.lua"), b"x");
                entry.declared_size = Some(MAX_BACKUP_FILE_BYTES as u32);
                entry
            })
            .collect();
        let zip = wow.dir.path().join("big.zip");
        fs::write(&zip, raw_zip(&entries)).unwrap();
        check_tree(&zip, restore_limits(), restore_options()).unwrap();
        assert!(check_tree(&zip, Limits::default(), restore_options()).is_err());
    }

    /// Nothing in a normal WTF reaches a restore's limits now, so this lowers one to get a
    /// zip it would refuse.
    #[test]
    fn a_backup_a_restore_would_refuse_is_not_kept_and_prunes_nothing() {
        let wow = Wow::new();
        let older = [wow.backup(2).unwrap(), wow.backup(2).unwrap()];
        let refused = Limits {
            max_entries: 2,
            ..restore_limits()
        };
        let result = backup_within(
            &wow.wtf,
            &wow.backups,
            2,
            refused,
            &mut |_| {},
            &not_cancelled(),
        );
        let text = result.unwrap_err().to_string();
        assert!(
            text.starts_with("This settings backup wasn't saved"),
            "{text}"
        );
        assert!(text.contains("older backups were kept"), "{text}");
        assert!(!text.contains("addon"), "{text}");

        let left: Vec<PathBuf> = list_backups(&wow.backups)
            .unwrap()
            .into_iter()
            .map(|backup| backup.path)
            .collect();
        assert_eq!(left, [older[1].clone(), older[0].clone()]);
        assert_eq!(
            fs::read_dir(&wow.backups).unwrap().count(),
            2,
            "no .part left"
        );
    }

    #[test]
    fn a_path_too_deep_to_restore_is_left_out_and_reported_and_the_rest_is_saved() {
        let wow = Wow::new();
        let saved = tree(&wow.wtf);
        write_files(
            &wow.wtf.join(format!("Deep/{}", "d/".repeat(70))),
            &[("stuck.lua", b"unreachable")],
        );
        let mut events = Vec::new();
        let zip = backup_wtf(
            &wow.wtf,
            &wow.backups,
            5,
            &mut |e| events.push(e),
            &not_cancelled(),
        )
        .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackupProgress::Skipped { path, reason }
                    if path.starts_with("Deep/") && reason.contains("too long"))),
            "{events:?}"
        );
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn folders_named_like_programs_are_restored_with_what_is_in_them() {
        let wow = Wow::new();
        for folder in [
            "SOMEONE@EXAMPLE.COM",
            "my.so",
            "a.sh",
            "Thing.app",
            "x.jar",
            "z.exe",
        ] {
            write_files(
                &wow.wtf.join("Account").join(folder),
                &[("SavedVariables/Foo.lua", b"settings")],
            );
        }
        let saved = tree(&wow.wtf);
        let zip = wow.backup(5).unwrap();
        fs::remove_dir_all(wow.wtf.join("Account")).unwrap();
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert_eq!(tree(&wow.wtf), saved);
    }

    #[test]
    fn a_program_in_a_backup_still_stays_out_of_a_restore() {
        let wow = Wow::new();
        write_files(
            &wow.wtf,
            &[("sync.bat", b"@echo off"), ("Account/PLAYER/run.vbe", b"x")],
        );
        let zip = wow.backup(5).unwrap();
        assert!(zip_names(&zip).contains(&"sync.bat".to_string()));
        restore_wtf(&zip, &wow.wtf, false).unwrap();
        assert!(!wow.wtf.join("sync.bat").exists());
        assert!(!wow.wtf.join("Account/PLAYER/run.vbe").exists());
        assert!(wow.wtf.join("Config.wtf").is_file());
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
