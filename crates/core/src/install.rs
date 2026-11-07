//! The install transaction: staging, journaled swap, rollback and crash recovery.
//!
//! Everything lives in `<Interface>/.wiaf/`, on the same volume as `AddOns` and
//! outside it, so swaps are plain renames and the game never scans our leftovers:
//!
//! ```text
//! j/<txn>.lock   held open while the transaction runs, so `recover` leaves it alone
//! j/<txn>.json   the journal
//! d/<txn>.zip    the download
//! s/<txn>/       new folders, waiting to move in
//! b/<txn>/       old folders, moved aside
//! swap.lock      taken for each swap, so two never interleave
//! ```
//!
//! The rule that makes this crash-safe: **nothing is deleted until the journal says,
//! durably, which way the transaction went.** While it says `pending`, staging and
//! backup are intact, so the on-disk state alone tells rollback what happened to each
//! folder. `committed` and `rolledback` only ever mean "delete the leftovers".

use std::cmp::Reverse;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::catalog::{Move, Release};
use crate::error::{Error, Result};
use crate::extract::{self, Limits, display_path, fold, io_error, is_safe_folder_name, long_path};
use crate::net::Http;
use crate::toc;

const WORK_DIR_NAME: &str = ".wiaf";
/// Used only when the catalog doesn't know a zip's size. Real addons top out around 90 MB.
const MAX_UNKNOWN_ZIP_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TOC_BYTES: u64 = 1024 * 1024;
/// About five seconds in total, for antivirus and cloud-sync tools to let go of a file.
const RETRY_DELAYS_MS: [u64; 9] = [50, 100, 200, 400, 800, 800, 800, 800, 800];
const JOURNAL_VERSION: u32 = 1;
/// The zip just installed, plus the one before it for rollback.
const KEPT_ZIPS_PER_ADDON: usize = 2;
const MAX_KEPT_ZIPS_TOTAL: u64 = 500 * 1024 * 1024;

/// Where one game install keeps its addons and our work files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub interface_dir: PathBuf,
    pub addons_dir: PathBuf,
    pub work_dir: PathBuf,
}

impl Layout {
    /// Build from the path of `.../Interface/AddOns`. Anything not named `AddOns` is
    /// refused, because a wrong path here would scatter work files around someone's disk.
    pub fn from_addons_dir(addons_dir: impl AsRef<Path>) -> Result<Layout> {
        let addons_dir = addons_dir.as_ref();
        let absolute = std::path::absolute(addons_dir).map_err(|e| Error::io(addons_dir, e))?;
        let named_addons = absolute
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("AddOns"));
        let Some(interface_dir) = absolute.parent().filter(|_| named_addons) else {
            return Err(Error::Other(format!(
                "{} isn't a game's AddOns folder. Pick the folder ending in Interface\\AddOns.",
                addons_dir.display()
            )));
        };
        Ok(Layout {
            interface_dir: interface_dir.to_path_buf(),
            work_dir: interface_dir.join(WORK_DIR_NAME),
            addons_dir: absolute,
        })
    }
}

/// How far an install has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Downloading { done: u64, total: Option<u64> },
    Verifying,
    Extracting { done: u64, total: u64 },
    Swapping,
    Done,
}

/// What to install and what it replaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallRequest {
    /// Catalog id such as `gh:owner/repo`. Empty for a loose zip, which keeps no copy.
    pub addon_id: String,
    pub version: String,
    /// Ignored by [`install_from_zip`].
    pub url: String,
    /// Bytes. 0 when unknown.
    pub size: u64,
    pub sha256: Option<String>,
    pub md5: Option<String>,
    pub moves: Vec<Move>,
    /// Folders the previous version owned. Any that aren't in the new zip are removed
    /// in the same transaction. Leave out folders another addon still claims.
    pub old_folders: Vec<String>,
    /// `paths::cache_dir()`, to keep a copy in `zips/<safe id>/` for rollback.
    /// `None` keeps nothing.
    pub cache_dir: Option<PathBuf>,
}

impl InstallRequest {
    /// A request for a catalog release, keeping nothing and replacing nothing.
    pub fn for_release(addon_id: &str, release: &Release) -> Self {
        InstallRequest {
            addon_id: addon_id.to_string(),
            version: release.version.clone(),
            url: release.url.clone(),
            size: release.size,
            sha256: release.sha256.clone(),
            md5: release.md5.clone(),
            moves: release.moves.clone(),
            ..InstallRequest::default()
        }
    }
}

/// What an install did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Folders now in AddOns from this zip, sorted.
    pub folders: Vec<String>,
    /// Executables and scripts that were left out.
    pub skipped_files: Vec<String>,
    /// The folders whose TOC targets Forever.
    pub forever_folders: Vec<String>,
    /// Things worth telling the player. The install itself succeeded.
    pub warnings: Vec<String>,
    pub kept_zip: Option<PathBuf>,
}

/// Download and verify `request.url`, then install it.
pub fn install_from_url(
    http: &Http,
    layout: &Layout,
    request: &InstallRequest,
    progress: &mut dyn FnMut(Stage),
    cancel: &AtomicBool,
) -> Result<InstallOutcome> {
    run_install(layout, request, ZipSource::Download(http), progress, cancel)
}

/// Install a zip already on disk: "install from a zip file", and rollback to a kept
/// zip. The file itself is left where it is.
pub fn install_from_zip(
    layout: &Layout,
    zip_path: &Path,
    request: &InstallRequest,
    progress: &mut dyn FnMut(Stage),
    cancel: &AtomicBool,
) -> Result<InstallOutcome> {
    run_install(layout, request, ZipSource::File(zip_path), progress, cancel)
}

/// Remove folders from AddOns, journaled like an install, and return the ones that
/// were there. Deciding that no other addon still claims a folder is the caller's job.
pub fn remove(layout: &Layout, folders: &[String]) -> Result<Vec<String>> {
    check_folder_names(folders)?;
    let txn = Txn::begin(layout)?;
    let (_swap_lock, entries) = match settle_and_plan(&txn, &[], folders) {
        Ok((lock, entries, earlier)) => {
            earlier.iter().for_each(|note| log::info!("{note}"));
            (lock, entries)
        }
        Err(error) => {
            txn.abandon();
            return Err(error);
        }
    };
    if entries.is_empty() {
        txn.abandon();
        return Ok(Vec::new());
    }
    let removed: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.old.clone())
        .collect();
    txn.commit(&entries)?;
    if let Some(warning) = txn.finish() {
        log::warn!("{warning}");
    }
    log::info!("Removed {removed:?}");
    Ok(removed)
}

/// Run at startup. Rolls back transactions that never committed, finishes cleaning up
/// after ones that did, and deletes stale downloads and staging. Returns one line per
/// interrupted transaction, for the player. Safe to run again at any point, including
/// after a crash inside this function; on error, whatever couldn't be finished yet
/// stays journaled for next time.
pub fn recover(layout: &Layout) -> Result<Vec<String>> {
    let dirs = Dirs::new(layout);
    if !dirs.root.exists() {
        return Ok(Vec::new());
    }
    let _swap_lock = SwapLock::acquire(&dirs)?;
    recover_dead_transactions(&dirs, None)
}

/// Deal with every transaction whose process is gone. The caller holds the swap lock.
fn recover_dead_transactions(dirs: &Dirs, own_id: Option<&str>) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let mut first_error = None;
    for id in dirs.transaction_ids() {
        if own_id == Some(id.as_str()) {
            continue;
        }
        match recover_one(dirs, &id) {
            Ok(Some(note)) => notes.push(note),
            Ok(None) => {}
            Err(error) => {
                log::error!("Couldn't recover transaction {id}: {error}");
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(notes),
    }
}

/// Everything a swap needs before it can start: the lock, a clean slate, and the steps
/// planned from what's in AddOns right now. An unfinished transaction from a dead process
/// has to be settled first, or rolling it back later would undo this swap too.
fn settle_and_plan(
    txn: &Txn,
    new_folders: &[String],
    old_folders: &[String],
) -> Result<(SwapLock, Vec<SwapEntry>, Vec<String>)> {
    let lock = SwapLock::acquire(&txn.dirs)?;
    let earlier = recover_dead_transactions(&txn.dirs, Some(&txn.id))?;
    let existing = list_addons(&txn.dirs.addons)?;
    Ok((
        lock,
        plan_swap(&existing, new_folders, old_folders),
        earlier,
    ))
}

/// The kept zips for an addon, newest first: the installed version, then the previous one.
pub fn kept_zips(cache_dir: &Path, addon_id: &str) -> Vec<PathBuf> {
    let mut zips = list_zips(&zip_dir(&long_path(cache_dir), addon_id));
    zips.sort_by_key(|zip| Reverse(zip.modified));
    zips.into_iter()
        .map(|zip| display_path(&zip.path))
        .collect()
}

/// A rename that waits out transient locks. Renames are all this module ever uses to
/// move folders, because copies aren't atomic.
pub(crate) fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    retry_transient(|| {
        fault::hit("rename")?;
        fs::rename(from, to)
    })
}

/// SHA-256 of a file as lowercase hex.
pub(crate) fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String> {
    hash_file::<Sha256>(path, cancel)
}

fn retry_transient(mut op: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut delays = RETRY_DELAYS_MS.iter();
    loop {
        match op() {
            Err(error) if is_transient(&error) => match delays.next() {
                Some(ms) => thread::sleep(Duration::from_millis(*ms)),
                None => return Err(error),
            },
            other => return other,
        }
    }
}

#[cfg(windows)]
fn is_transient(error: &io::Error) -> bool {
    // Access denied, sharing violation, lock violation.
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(not(windows))]
fn is_transient(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ResourceBusy
}

/// Failure injection for tests. Every change a transaction makes to the disk reports
/// here first, so tests can fail or "crash" at each step through the public API.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::path::Path;

    #[derive(Debug, Default)]
    pub struct Plan {
        /// This step and all later ones fail, as if the process died here.
        pub crash_at: Option<usize>,
        /// These steps fail once each, and the transaction carries on handling it.
        pub fail_at: Vec<usize>,
        /// The next this-many renames report that a file is in use.
        pub lock_errors: usize,
        /// Every step reached so far.
        pub log: Vec<&'static str>,
    }

    thread_local! {
        static PLAN: RefCell<Option<Plan>> = const { RefCell::new(None) };
    }

    pub fn set(plan: Plan) {
        PLAN.with(|cell| *cell.borrow_mut() = Some(plan));
    }

    pub fn take() -> Plan {
        PLAN.with(|cell| cell.borrow_mut().take())
            .unwrap_or_default()
    }

    pub fn lock_error() -> io::Error {
        io::Error::from_raw_os_error(if cfg!(windows) { 32 } else { 16 })
    }

    pub fn hit(op: &'static str) -> io::Result<()> {
        PLAN.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(plan) = cell.as_mut() else {
                return Ok(());
            };
            let index = plan.log.len();
            plan.log.push(op);
            if plan.crash_at.is_some_and(|at| index >= at) {
                return Err(io::Error::other("simulated crash"));
            }
            if plan.fail_at.contains(&index) {
                return Err(io::Error::other("injected failure"));
            }
            if op == "rename" && plan.lock_errors > 0 {
                plan.lock_errors -= 1;
                return Err(lock_error());
            }
            Ok(())
        })
    }

    /// Dying halfway through deleting a tree leaves part of it behind.
    pub fn hit_tree(path: &Path) -> io::Result<()> {
        let result = hit("remove tree");
        if result.is_err()
            && let Some(Ok(child)) = fs::read_dir(path).ok().and_then(|mut dir| dir.next())
        {
            let child = child.path();
            if fs::remove_dir_all(&child).is_err() {
                let _ = fs::remove_file(&child);
            }
        }
        result
    }
}

#[cfg(not(test))]
mod fault {
    use std::io;
    use std::path::Path;

    #[inline(always)]
    pub fn hit(_op: &'static str) -> io::Result<()> {
        Ok(())
    }

    #[inline(always)]
    pub fn hit_tree(_path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// The long-path form of every directory a transaction touches.
struct Dirs {
    addons: PathBuf,
    root: PathBuf,
    journals: PathBuf,
    staging: PathBuf,
    backups: PathBuf,
    downloads: PathBuf,
}

impl Dirs {
    fn new(layout: &Layout) -> Dirs {
        let root = long_path(&layout.work_dir);
        Dirs {
            addons: long_path(&layout.addons_dir),
            journals: root.join("j"),
            staging: root.join("s"),
            backups: root.join("b"),
            downloads: root.join("d"),
            root,
        }
    }

    fn create(&self) -> Result<()> {
        for dir in [
            &self.addons,
            &self.journals,
            &self.staging,
            &self.backups,
            &self.downloads,
        ] {
            fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
        }
        Ok(())
    }

    fn lock(&self, id: &str) -> PathBuf {
        self.journals.join(format!("{id}.lock"))
    }

    fn journal(&self, id: &str) -> PathBuf {
        self.journals.join(format!("{id}.json"))
    }

    fn journal_tmp(&self, id: &str) -> PathBuf {
        self.journals.join(format!("{id}.json.tmp"))
    }

    fn staged(&self, id: &str) -> PathBuf {
        self.staging.join(id)
    }

    fn backup(&self, id: &str) -> PathBuf {
        self.backups.join(id)
    }

    fn zip(&self, id: &str) -> PathBuf {
        self.downloads.join(format!("{id}.zip"))
    }

    fn zip_part(&self, id: &str) -> PathBuf {
        self.downloads.join(format!("{id}.zip.part"))
    }

    /// Every transaction with something on disk. Names that aren't ours are ignored.
    fn transaction_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for dir in [
            &self.journals,
            &self.staging,
            &self.backups,
            &self.downloads,
        ] {
            let Ok(items) = fs::read_dir(dir) else {
                continue;
            };
            for item in items.flatten() {
                let name = item.file_name();
                let Some(name) = name.to_str() else { continue };
                let id = name.split('.').next().unwrap_or(name);
                if id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    ids.insert(id.to_string());
                }
            }
        }
        ids
    }
}

/// Which folder leaves AddOns and which one takes its place, for one step of a swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwapEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    new: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JournalState {
    Pending,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    version: u32,
    state: JournalState,
    entries: Vec<SwapEntry>,
}

impl Journal {
    fn folders(&self) -> String {
        let mut names: Vec<&str> = self
            .entries
            .iter()
            .filter_map(|entry| entry.new.as_deref().or(entry.old.as_deref()))
            .collect();
        names.sort_unstable();
        names.join(", ")
    }

    /// A journal is data on disk like any other. Never trust its names with a rename.
    fn is_sound(&self) -> bool {
        self.version == JOURNAL_VERSION
            && self.entries.iter().all(|entry| {
                [&entry.old, &entry.new]
                    .into_iter()
                    .flatten()
                    .all(|name| is_safe_folder_name(name))
            })
    }
}

enum StoredJournal {
    Missing,
    Damaged,
    Found(Journal),
}

fn read_journal(path: &Path) -> Result<StoredJournal> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StoredJournal::Missing),
        Err(e) => return Err(io_error(path, e)),
    };
    Ok(match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) if journal.is_sound() => StoredJournal::Found(journal),
        _ => StoredJournal::Damaged,
    })
}

/// Atomic and durable: readers see the old journal or the whole new one, and it's on
/// disk before this returns.
fn write_journal(
    dirs: &Dirs,
    id: &str,
    state: JournalState,
    entries: &[SwapEntry],
) -> io::Result<()> {
    let journal = Journal {
        version: JOURNAL_VERSION,
        state,
        entries: entries.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&journal).map_err(io::Error::other)?;
    fault::hit(match state {
        JournalState::Pending => "journal pending",
        JournalState::Committed => "journal committed",
        JournalState::RolledBack => "journal rolledback",
    })?;
    let tmp = dirs.journal_tmp(id);
    let mut file = File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    rename_with_retry(&tmp, &dirs.journal(id))?;
    sync_dir(&dirs.journals);
    Ok(())
}

/// Windows has no way to sync a directory, and NTFS logs renames anyway. Elsewhere it
/// makes a rename durable, but not every filesystem supports it, so failure is fine.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// A file lock the OS drops if the process dies, so a crash can't leave anything stuck.
/// A filesystem without locks just means going without this protection.
struct SwapLock {
    _file: Option<File>,
}

impl SwapLock {
    fn acquire(dirs: &Dirs) -> Result<SwapLock> {
        let path = dirs.root.join("swap.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        match file.lock() {
            Ok(()) => Ok(SwapLock { _file: Some(file) }),
            Err(error) => {
                log::warn!("Couldn't lock {}: {error}", display_path(&path).display());
                Ok(SwapLock { _file: None })
            }
        }
    }
}

/// One running transaction, holding its lock file open.
struct Txn {
    dirs: Dirs,
    id: String,
    lock: File,
}

impl Txn {
    fn begin(layout: &Layout) -> Result<Txn> {
        let dirs = Dirs::new(layout);
        dirs.create()?;
        for _ in 0..32 {
            let id = format!("{:08x}", RandomState::new().hash_one(0u8) as u32);
            let path = dirs.lock(&id);
            let lock = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(lock) => lock,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_error(&path, e)),
            };
            match lock.try_lock() {
                Ok(()) | Err(TryLockError::Error(_)) => return Ok(Txn { dirs, id, lock }),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        Err(Error::Other(
            "Couldn't start the install. Try again.".into(),
        ))
    }

    /// Before the swap started: nothing to undo, so just drop everything.
    fn abandon(self) {
        let _ = cleanup(&self.dirs, &self.id);
        release(self);
    }

    /// The swap itself, through to the durable commit. On failure it undoes what it did,
    /// and the transaction is over either way unless it returns `Ok`.
    fn commit(&self, entries: &[SwapEntry]) -> Result<()> {
        let (dirs, id) = (&self.dirs, self.id.as_str());
        let backup = dirs.backup(id);
        let prepared = fs::create_dir_all(&backup)
            .and_then(|()| fs::create_dir_all(dirs.staged(id)))
            .and_then(|()| write_journal(dirs, id, JournalState::Pending, entries));
        if let Err(error) = prepared {
            let _ = cleanup(dirs, id);
            let _ = remove_any(&dirs.lock(id));
            return Err(io_error(&dirs.journal(id), error));
        }
        let applied = apply_entries(dirs, id, entries).and_then(|()| {
            write_journal(dirs, id, JournalState::Committed, entries).map_err(|error| SwapFailure {
                folder: "the install record".into(),
                error,
            })
        });
        match applied {
            Ok(()) => Ok(()),
            Err(failure) => Err(undo_failed_swap(dirs, id, entries, failure)),
        }
    }

    /// After the commit. A leftover that won't delete yet is no reason to fail: the
    /// addon is installed, and `recover` finishes the job from the journal next start.
    fn finish(self) -> Option<String> {
        let result = cleanup(&self.dirs, &self.id);
        match result {
            Ok(()) => {
                release(self);
                None
            }
            Err(error) => {
                log::warn!("Couldn't clean up after an install yet: {error}");
                Some(
                    "Installed, but a few temporary files are still in use. They'll be \
                     cleaned up next time the app starts."
                        .to_string(),
                )
            }
        }
    }
}

/// The lock file goes last, because a transaction's other leftovers are only safe to
/// touch once no process holds it.
fn release(txn: Txn) {
    let path = txn.dirs.lock(&txn.id);
    drop(txn.lock);
    let _ = remove_any(&path);
}

struct SwapFailure {
    folder: String,
    error: io::Error,
}

fn apply_entries(
    dirs: &Dirs,
    id: &str,
    entries: &[SwapEntry],
) -> std::result::Result<(), SwapFailure> {
    let failure = |folder: &str, error| SwapFailure {
        folder: folder.to_string(),
        error,
    };
    let (backup, staged) = (dirs.backup(id), dirs.staged(id));
    for entry in entries {
        if let Some(old) = &entry.old {
            rename_with_retry(&dirs.addons.join(old), &backup.join(old))
                .map_err(|e| failure(old, e))?;
        }
        if let Some(new) = &entry.new {
            rename_with_retry(&staged.join(new), &dirs.addons.join(new))
                .map_err(|e| failure(new, e))?;
        }
    }
    Ok(())
}

/// Put AddOns back and report why we had to. Every piece of this can also run from
/// `recover`, so an undo that can't finish now simply stays journaled for next start.
fn undo_failed_swap(dirs: &Dirs, id: &str, entries: &[SwapEntry], failure: SwapFailure) -> Error {
    match rollback(dirs, id, entries) {
        Ok(()) => {
            if write_journal(dirs, id, JournalState::RolledBack, entries).is_ok()
                && cleanup(dirs, id).is_ok()
            {
                let _ = remove_any(&dirs.lock(id));
            }
            swap_error(failure)
        }
        Err(undo_error) => {
            log::error!("Couldn't undo a failed swap: {undo_error}");
            Error::Other(format!(
                "Couldn't finish changing {}, and couldn't put everything back yet ({undo_error}). \
                 Nothing was deleted. Close WoW and restart this app to finish restoring your addons.",
                failure.folder
            ))
        }
    }
}

fn swap_error(failure: SwapFailure) -> Error {
    let SwapFailure { folder, error } = failure;
    if is_transient(&error) {
        Error::Other(format!(
            "Couldn't replace {folder} because another program is using it. \
             Close WoW and anything else that might have it open, then try again. \
             Your addons weren't changed."
        ))
    } else if error.kind() == io::ErrorKind::CrossesDevices {
        Error::Other(
            "Your AddOns folder is a link to another drive, so addons can't be swapped safely. \
             Your addons weren't changed."
                .to_string(),
        )
    } else {
        Error::Other(format!(
            "Couldn't replace {folder} ({error}). Your addons weren't changed. Try again."
        ))
    }
}

/// Undo a pending swap, working only from what's on disk. Safe to repeat from any
/// point, because each step checks what actually happened to its folder.
fn rollback(dirs: &Dirs, id: &str, entries: &[SwapEntry]) -> io::Result<()> {
    let (backup, staged) = (dirs.backup(id), dirs.staged(id));
    fs::create_dir_all(&staged)?;
    for entry in entries.iter().rev() {
        let old_is_aside = match &entry.old {
            Some(old) => is_present(&backup.join(old))?,
            None => false,
        };
        if let Some(new) = &entry.new {
            // Old folders always leave first. So if an old folder isn't aside yet, what
            // sits in AddOns under this name is still that old folder, staging or not.
            let may_have_moved = entry.old.is_none() || old_is_aside;
            let target = dirs.addons.join(new);
            if may_have_moved && !is_present(&staged.join(new))? && is_present(&target)? {
                rename_with_retry(&target, &staged.join(new))?;
            }
        }
        if let Some(old) = &entry.old
            && old_is_aside
        {
            rename_with_retry(&backup.join(old), &dirs.addons.join(old))?;
        }
    }
    Ok(())
}

fn is_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Delete a transaction's leftovers. The journal goes after everything else, so that
/// anything left behind is still accounted for at next start.
fn cleanup(dirs: &Dirs, id: &str) -> io::Result<()> {
    let mut first_error = None;
    for path in [
        dirs.backup(id),
        dirs.staged(id),
        dirs.zip(id),
        dirs.zip_part(id),
        dirs.journal_tmp(id),
    ] {
        if let Err(error) = remove_any(&path) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => remove_any(&dirs.journal(id)),
    }
}

/// Remove a file, folder or link. Never follows a link: a developer's symlinked working
/// copy in AddOns loses the link, not the code. Already gone is fine.
fn remove_any(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let is_dir = meta.is_dir();
    if is_dir {
        fault::hit_tree(path)?;
    } else {
        fault::hit("remove file")?;
    }
    retry_transient(|| {
        let result = if is_dir {
            fs::remove_dir_all(path)
        } else {
            // A link to a folder is a directory to Windows but a file to `metadata`.
            fs::remove_file(path).or_else(|_| fs::remove_dir(path))
        };
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    })
}

fn recover_one(dirs: &Dirs, id: &str) -> Result<Option<String>> {
    let lock_path = dirs.lock(id);
    let lock = OpenOptions::new().write(true).open(&lock_path).ok();
    if let Some(lock) = &lock
        && matches!(lock.try_lock(), Err(TryLockError::WouldBlock))
    {
        return Ok(None);
    }
    let note = match read_journal(&dirs.journal(id))? {
        StoredJournal::Missing => {
            // Backups are only ever filled under a journal, and it's deleted last. Files
            // here without one means something outside this app interfered, so hands off.
            if !is_empty_or_missing(&dirs.backup(id)) {
                return Ok(Some(leftover_note(dirs, id)));
            }
            None
        }
        StoredJournal::Damaged => return Ok(Some(leftover_note(dirs, id))),
        StoredJournal::Found(journal) => {
            let folders = journal.folders();
            match journal.state {
                JournalState::Pending => {
                    rollback(dirs, id, &journal.entries).map_err(|error| {
                        log::error!("Couldn't roll back {id}: {error}");
                        Error::Other(format!(
                            "Couldn't finish restoring {folders} after an interrupted install \
                             ({error}). Nothing was deleted. Close WoW and restart this app to try again."
                        ))
                    })?;
                    write_journal(dirs, id, JournalState::RolledBack, &journal.entries)
                        .map_err(|e| io_error(&dirs.journal(id), e))?;
                    Some(format!(
                        "An install was interrupted, so these were put back the way they were: {folders}."
                    ))
                }
                JournalState::RolledBack => None,
                JournalState::Committed => Some(format!(
                    "An install was interrupted just before it finished cleaning up. These are installed: {folders}."
                )),
            }
        }
    };
    cleanup(dirs, id).map_err(|e| io_error(&dirs.root, e))?;
    drop(lock);
    remove_any(&lock_path).map_err(|e| io_error(&lock_path, e))?;
    Ok(note)
}

fn is_empty_or_missing(dir: &Path) -> bool {
    fs::read_dir(dir).map_or(true, |mut items| items.next().is_none())
}

fn leftover_note(dirs: &Dirs, id: &str) -> String {
    format!(
        "Found old addon files from an interrupted install but no record of what they were. \
         They were left in {} in case you want them.",
        display_path(&dirs.backup(id)).display()
    )
}

fn check_folder_names(folders: &[String]) -> Result<()> {
    match folders.iter().find(|name| !is_safe_folder_name(name)) {
        Some(bad) => Err(Error::Other(format!(
            "\"{bad}\" isn't the name of a folder inside AddOns, so nothing was changed."
        ))),
        None => Ok(()),
    }
}

/// Names in AddOns right now, exactly as the filesystem spells them.
fn list_addons(addons: &Path) -> Result<Vec<String>> {
    let items = fs::read_dir(addons).map_err(|e| io_error(addons, e))?;
    let mut names = Vec::new();
    for item in items {
        let item = item.map_err(|e| io_error(addons, e))?;
        if let Some(name) = item.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Turn "these go in, those go away" into swap steps, in the spelling on disk.
fn plan_swap(
    existing: &[String],
    new_folders: &[String],
    old_folders: &[String],
) -> Vec<SwapEntry> {
    let mut entries: Vec<SwapEntry> = Vec::new();
    let mut leaving: HashSet<&str> = HashSet::new();
    for new in new_folders {
        let folded = fold(new);
        let same: Vec<&String> = existing
            .iter()
            .filter(|name| fold(name) == folded)
            .collect();
        // More than one only happens on a case-sensitive disk. They all go, so Wine
        // never sees two spellings of one addon.
        for extra in same.iter().skip(1) {
            leaving.insert(extra);
            entries.push(SwapEntry {
                old: Some((*extra).clone()),
                new: None,
            });
        }
        if let Some(first) = same.first() {
            leaving.insert(first);
        }
        entries.push(SwapEntry {
            old: same.first().map(|name| (*name).clone()),
            new: Some(new.clone()),
        });
    }
    for old in old_folders {
        if new_folders.iter().any(|new| fold(new) == fold(old)) {
            continue;
        }
        let actual = existing
            .iter()
            .find(|name| *name == old)
            .or_else(|| existing.iter().find(|name| fold(name) == fold(old)));
        if let Some(actual) = actual
            && leaving.insert(actual)
        {
            entries.push(SwapEntry {
                old: Some(actual.clone()),
                new: None,
            });
        }
    }
    entries
}

enum ZipSource<'a> {
    Download(&'a Http),
    File(&'a Path),
}

struct Prepared {
    zip: PathBuf,
    folders: Vec<String>,
    skipped_files: Vec<String>,
    forever_folders: Vec<String>,
}

fn run_install(
    layout: &Layout,
    request: &InstallRequest,
    source: ZipSource<'_>,
    progress: &mut dyn FnMut(Stage),
    cancel: &AtomicBool,
) -> Result<InstallOutcome> {
    check_folder_names(&request.old_folders)?;
    let txn = Txn::begin(layout)?;
    let prepared = match prepare(&txn, request, source, progress, cancel) {
        Ok(prepared) => prepared,
        Err(error) => {
            txn.abandon();
            return Err(error);
        }
    };

    // The last moment a cancel counts. Past here the swap runs to its end.
    if cancel.load(Ordering::Relaxed) {
        txn.abandon();
        return Err(Error::Cancelled);
    }
    progress(Stage::Swapping);
    let (swap_lock, entries, mut warnings) =
        match settle_and_plan(&txn, &prepared.folders, &request.old_folders) {
            Ok(planned) => planned,
            Err(error) => {
                txn.abandon();
                return Err(error);
            }
        };
    for old in entries.iter().filter_map(|entry| entry.old.as_ref()) {
        if !request
            .old_folders
            .iter()
            .any(|name| fold(name) == fold(old))
        {
            log::info!("Replacing a folder that was already in AddOns: {old}");
        }
    }
    txn.commit(&entries)?;

    if prepared.forever_folders.is_empty() {
        warnings.push(
            "None of this addon's folders say they support WoW Forever. It was installed \
             anyway, but the game may list it as out of date."
                .to_string(),
        );
    }
    let kept_zip = request.cache_dir.as_ref().and_then(|cache_dir| {
        keep_zip(
            cache_dir,
            &request.addon_id,
            &request.version,
            &prepared.zip,
            MAX_KEPT_ZIPS_TOTAL,
        )
        .unwrap_or_else(|error| {
            log::warn!("Couldn't keep a copy of the zip for rollback: {error}");
            warnings.push(
                "Couldn't keep a copy of this version, so it can't be rolled back to later."
                    .to_string(),
            );
            None
        })
    });
    warnings.extend(txn.finish());
    drop(swap_lock);
    progress(Stage::Done);
    log::info!("Installed {:?}", prepared.folders);
    Ok(InstallOutcome {
        folders: prepared.folders,
        skipped_files: prepared.skipped_files,
        forever_folders: prepared.forever_folders,
        warnings,
        kept_zip,
    })
}

/// Everything before the swap. AddOns isn't touched by any of it.
fn prepare(
    txn: &Txn,
    request: &InstallRequest,
    source: ZipSource<'_>,
    progress: &mut dyn FnMut(Stage),
    cancel: &AtomicBool,
) -> Result<Prepared> {
    let zip = match source {
        ZipSource::Download(http) => {
            let zip = txn.dirs.zip(&txn.id);
            let max_bytes = if request.size > 0 {
                request.size
            } else {
                MAX_UNKNOWN_ZIP_BYTES
            };
            http.download(
                &request.url,
                &zip,
                &mut |p| {
                    progress(Stage::Downloading {
                        done: p.done,
                        total: p.total,
                    })
                },
                cancel,
                max_bytes,
            )?;
            zip
        }
        ZipSource::File(path) => long_path(path),
    };
    progress(Stage::Verifying);
    verify_zip(&zip, request, cancel)?;
    let staging = txn.dirs.staged(&txn.id);
    let extracted = extract::extract(
        &zip,
        &staging,
        &request.moves,
        Limits::default(),
        &mut |p| {
            progress(Stage::Extracting {
                done: p.done,
                total: p.total.unwrap_or(p.done),
            })
        },
        cancel,
    )?;
    let forever_folders = extracted
        .folders
        .iter()
        .filter(|folder| targets_forever(&staging.join(folder), folder))
        .cloned()
        .collect();
    Ok(Prepared {
        zip,
        folders: extracted.folders,
        skipped_files: extracted.skipped_files,
        forever_folders,
    })
}

/// Digest first by SHA-256, else MD5. Empty strings count as not given.
fn verify_zip(zip: &Path, request: &InstallRequest, cancel: &AtomicBool) -> Result<()> {
    let actual_size = fs::metadata(zip).map_err(|e| io_error(zip, e))?.len();
    if request.size > 0 && actual_size != request.size {
        return Err(Error::Corrupt(format!(
            "it's {actual_size} bytes, but it should be {}",
            request.size
        )));
    }
    let given = |digest: &Option<String>| {
        digest
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(str::to_string)
    };
    let (actual, expected) = if let Some(expected) = given(&request.sha256) {
        (hash_file::<Sha256>(zip, cancel)?, expected)
    } else if let Some(expected) = given(&request.md5) {
        (hash_file::<Md5>(zip, cancel)?, expected)
    } else {
        return Ok(());
    };
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(Error::Corrupt(
            "its checksum doesn't match what its author published".into(),
        ))
    }
}

fn hash_file<D: Digest>(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut hasher = D::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(path, e)),
        };
        hasher.update(&buffer[..count]);
    }
    Ok(to_hex(&hasher.finalize()))
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn targets_forever(folder_dir: &Path, folder: &str) -> bool {
    let Ok(items) = fs::read_dir(folder_dir) else {
        return false;
    };
    let names: Vec<String> = items
        .flatten()
        .filter_map(|item| item.file_name().to_str().map(str::to_string))
        .collect();
    let Some(toc_name) = toc::pick_toc(folder, &names) else {
        return false;
    };
    let Ok(file) = File::open(folder_dir.join(toc_name)) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(MAX_TOC_BYTES).read_to_end(&mut bytes).is_err() {
        return false;
    }
    toc::parse(&String::from_utf8_lossy(&bytes)).targets_forever()
}

/// `gh:owner/repo` as a folder name. The hash keeps `a/b_c` and `a_b/c` apart.
fn safe_id(addon_id: &str) -> String {
    let readable: String = addon_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let digest = Sha256::digest(addon_id.as_bytes());
    format!(
        "{}-{}",
        readable.trim_end_matches('.'),
        to_hex(&digest[..4])
    )
}

fn zip_dir(cache_dir: &Path, addon_id: &str) -> PathBuf {
    cache_dir.join("zips").join(safe_id(addon_id))
}

fn kept_zip_name(version: &str) -> String {
    let readable: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let name = format!("{readable}.zip");
    if readable.is_empty() || !is_safe_folder_name(&name) {
        format!("_{name}")
    } else {
        name
    }
}

struct KeptZip {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn list_zips(dir: &Path) -> Vec<KeptZip> {
    let Ok(items) = fs::read_dir(dir) else {
        return Vec::new();
    };
    items
        .flatten()
        .filter(|item| item.path().extension().is_some_and(|ext| ext == "zip"))
        .filter_map(|item| {
            let meta = item.metadata().ok()?;
            Some(KeptZip {
                path: item.path(),
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

/// Copy the installed zip into the cache, where the next install's "previous version"
/// will find it, then trim the cache. The cache may be on another drive, so it's a copy.
fn keep_zip(
    cache_dir: &Path,
    addon_id: &str,
    version: &str,
    zip: &Path,
    max_total: u64,
) -> io::Result<Option<PathBuf>> {
    if addon_id.is_empty() || fs::metadata(zip)?.len() > max_total {
        return Ok(None);
    }
    let cache_dir = long_path(cache_dir);
    let dir = zip_dir(&cache_dir, addon_id);
    fs::create_dir_all(&dir)?;
    let dest = dir.join(kept_zip_name(version));
    let already_there = match (fs::canonicalize(zip), fs::canonicalize(&dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !already_there {
        // Not `fs::copy`, which would carry over a read-only flag on the source.
        let part = dir.join(format!("{}.part", kept_zip_name(version)));
        io::copy(&mut File::open(zip)?, &mut File::create(&part)?)?;
        rename_with_retry(&part, &dest)?;
    }
    // "Newest" has to mean most recently installed, which a rollback's old file isn't.
    File::options()
        .write(true)
        .open(&dest)?
        .set_modified(SystemTime::now())?;
    trim_kept_zips(&cache_dir, &dir, &dest, max_total);
    Ok(Some(display_path(&dest)))
}

/// One previous version per addon, then the oldest go until the whole cache fits. The
/// zip just kept is safe from both.
fn trim_kept_zips(cache_dir: &Path, addon_dir: &Path, just_kept: &Path, max_total: u64) {
    let mut own = list_zips(addon_dir);
    own.sort_by_key(|zip| Reverse(zip.modified));
    let others = own.iter().filter(|zip| zip.path != just_kept);
    for stale in others.skip(KEPT_ZIPS_PER_ADDON - 1) {
        let _ = fs::remove_file(&stale.path);
    }
    // A copy interrupted by a crash, which nothing else would ever find.
    if let Ok(items) = fs::read_dir(addon_dir) {
        for item in items.flatten() {
            if item.path().extension().is_some_and(|ext| ext == "part") {
                let _ = fs::remove_file(item.path());
            }
        }
    }

    let root = cache_dir.join("zips");
    let Ok(addon_dirs) = fs::read_dir(&root) else {
        return;
    };
    let mut all: Vec<KeptZip> = addon_dirs
        .flatten()
        .flat_map(|dir| list_zips(&dir.path()))
        .collect();
    all.sort_by_key(|zip| zip.modified);
    let mut total: u64 = all.iter().map(|zip| zip.size).sum();
    for zip in all.iter().filter(|zip| zip.path != just_kept) {
        if total <= max_total {
            break;
        }
        if fs::remove_file(&zip.path).is_ok() {
            total -= zip.size;
            if let Some(parent) = zip.path.parent() {
                // Only succeeds once the folder is empty, which is exactly when it should.
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fault::{self, Plan};
    use super::*;
    use crate::extract::test_zips::zip_bytes;
    use crate::net::test_server::{respond, serve};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    const FOREVER_TOC: &[u8] = b"## Interface: 16001\n## Title: Test\n";
    const RETAIL_TOC: &[u8] = b"## Interface: 120105\n## Title: Test\n";

    /// A game folder with some addons in it and a zip ready to install.
    struct Game {
        dir: tempfile::TempDir,
        layout: Layout,
    }

    /// File paths to contents, plus empty folders as `path/` with no contents.
    type Tree = BTreeMap<String, Vec<u8>>;

    fn tree_of(root: &Path) -> Tree {
        fn walk(dir: &Path, prefix: &str, out: &mut Tree) {
            let mut empty = true;
            for item in fs::read_dir(dir).unwrap() {
                empty = false;
                let item = item.unwrap();
                let name = item.file_name().to_string_lossy().into_owned();
                let path = format!("{prefix}{name}");
                if item.file_type().unwrap().is_dir() {
                    walk(&item.path(), &format!("{path}/"), out);
                } else {
                    out.insert(path, fs::read(item.path()).unwrap());
                }
            }
            if empty && !prefix.is_empty() {
                out.insert(prefix.to_string(), Vec::new());
            }
        }
        let mut out = Tree::new();
        walk(root, "", &mut out);
        out
    }

    fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
        for (path, body) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    fn tree_from(files: &[(&str, &[u8])]) -> Tree {
        files
            .iter()
            .map(|(path, body)| (path.to_string(), body.to_vec()))
            .collect()
    }

    impl Game {
        fn with_addons(files: &[(&str, &[u8])]) -> Game {
            let dir = tempfile::tempdir().unwrap();
            let addons = dir.path().join("_forever_/Interface/AddOns");
            fs::create_dir_all(&addons).unwrap();
            write_tree(&addons, files);
            let layout = Layout::from_addons_dir(&addons).unwrap();
            Game { dir, layout }
        }

        fn addons(&self) -> Tree {
            tree_of(&self.layout.addons_dir)
        }

        fn zip(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
            let path = self.dir.path().join(name);
            fs::write(&path, zip_bytes(files)).unwrap();
            path
        }

        fn install(&self, zip: &Path, request: &InstallRequest) -> Result<InstallOutcome> {
            install_from_zip(
                &self.layout,
                zip,
                request,
                &mut |_| {},
                &AtomicBool::new(false),
            )
        }

        /// Nothing left of any transaction, whatever state it ended in.
        fn assert_no_leftovers(&self) {
            for sub in ["j", "s", "b", "d"] {
                let dir = self.layout.work_dir.join(sub);
                if dir.exists() {
                    let left: Vec<_> = fs::read_dir(&dir)
                        .unwrap()
                        .flatten()
                        .map(|i| i.file_name())
                        .collect();
                    assert!(left.is_empty(), "left in {sub}: {left:?}");
                }
            }
        }
    }

    fn request(old: &[&str]) -> InstallRequest {
        InstallRequest {
            old_folders: old.iter().map(|s| s.to_string()).collect(),
            ..InstallRequest::default()
        }
    }

    const BEFORE: &[(&str, &[u8])] = &[
        ("Foo/Foo.toc", b"old toc"),
        ("Foo/old.lua", b"old"),
        ("Foo/Libs/old/deep.lua", b"old deep"),
        ("FooOld/FooOld.toc", b"retired"),
        ("Unclaimed/Unclaimed.toc", b"mine"),
        ("Unclaimed/settings.lua", b"mine too"),
        ("!BugGrabber/!BugGrabber.toc", b"someone else's"),
    ];
    const NEW_ZIP: &[(&str, &[u8])] = &[
        ("Foo/Foo.toc", FOREVER_TOC),
        ("Foo/new.lua", b"new"),
        ("FooPlugin/FooPlugin.toc", FOREVER_TOC),
    ];
    const AFTER: &[(&str, &[u8])] = &[
        ("Foo/Foo.toc", FOREVER_TOC),
        ("Foo/new.lua", b"new"),
        ("FooPlugin/FooPlugin.toc", FOREVER_TOC),
        ("Unclaimed/Unclaimed.toc", b"mine"),
        ("Unclaimed/settings.lua", b"mine too"),
        ("!BugGrabber/!BugGrabber.toc", b"someone else's"),
    ];

    fn scenario() -> (Game, PathBuf, InstallRequest) {
        let game = Game::with_addons(BEFORE);
        let zip = game.zip("new.zip", NEW_ZIP);
        (game, zip, request(&["Foo", "FooOld"]))
    }

    /// The steps a clean run of `run` takes, in order.
    fn steps_of(run: impl FnOnce()) -> Vec<&'static str> {
        fault::set(Plan::default());
        run();
        fault::take().log
    }

    /// A commit only counts once the rename after writing the journal has happened.
    fn commit_step(steps: &[&str]) -> usize {
        steps
            .iter()
            .position(|step| *step == "journal committed")
            .unwrap()
            + 1
    }

    #[test]
    fn a_clean_update_swaps_exactly_the_right_folders() {
        let (game, zip, request) = scenario();
        let outcome = game.install(&zip, &request).unwrap();
        assert_eq!(game.addons(), tree_from(AFTER));
        assert_eq!(outcome.folders, ["Foo", "FooPlugin"]);
        assert_eq!(outcome.forever_folders, ["Foo", "FooPlugin"]);
        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.kept_zip, None);
        game.assert_no_leftovers();
    }

    #[test]
    fn a_crash_at_every_step_ends_exactly_before_or_exactly_after() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let committed_at = commit_step(&steps);
        assert!(steps.contains(&"rename") && steps.contains(&"remove tree"));

        for (crash_at, step) in steps.iter().enumerate() {
            let (game, zip, req) = scenario();
            fault::set(Plan {
                crash_at: Some(crash_at),
                ..Plan::default()
            });
            let _ = game.install(&zip, &req);
            fault::take();

            let notes = recover(&game.layout).unwrap();
            let expected = if crash_at > committed_at {
                AFTER
            } else {
                BEFORE
            };
            assert_eq!(
                game.addons(),
                tree_from(expected),
                "crash at step {crash_at}: {step}"
            );
            game.assert_no_leftovers();
            assert!(notes.len() <= 1, "crash at step {crash_at}: {notes:?}");

            // A second startup finds nothing to do and changes nothing.
            assert!(recover(&game.layout).unwrap().is_empty());
            assert_eq!(game.addons(), tree_from(expected));
        }
    }

    #[test]
    fn a_crash_during_recovery_is_recoverable_at_every_step_too() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let committed_at = commit_step(&steps);

        for crash_at in 0..steps.len() {
            let expected = if crash_at > committed_at {
                AFTER
            } else {
                BEFORE
            };
            let mut recovery_crash = 0;
            loop {
                let (game, zip, req) = scenario();
                fault::set(Plan {
                    crash_at: Some(crash_at),
                    ..Plan::default()
                });
                let _ = game.install(&zip, &req);
                fault::take();

                fault::set(Plan {
                    crash_at: Some(recovery_crash),
                    ..Plan::default()
                });
                let first = recover(&game.layout);
                let reached = fault::take().log.len();

                recover(&game.layout).unwrap();
                assert_eq!(
                    game.addons(),
                    tree_from(expected),
                    "install crashed at {crash_at}, recovery crashed at {recovery_crash}"
                );
                game.assert_no_leftovers();

                if reached <= recovery_crash {
                    first.unwrap();
                    break;
                }
                recovery_crash += 1;
            }
        }
    }

    #[test]
    fn a_failing_step_before_the_commit_rolls_back_cleanly() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let committed_at = commit_step(&steps);

        for (fail_at, step) in steps.iter().enumerate().take(committed_at + 1) {
            let (game, zip, req) = scenario();
            fault::set(Plan {
                fail_at: vec![fail_at],
                ..Plan::default()
            });
            let result = game.install(&zip, &req);
            fault::take();
            assert!(
                result.is_err(),
                "step {fail_at} ({step}) should fail the install"
            );
            assert_eq!(
                game.addons(),
                tree_from(BEFORE),
                "failed at {fail_at}: {step}"
            );
            game.assert_no_leftovers();
            assert!(recover(&game.layout).unwrap().is_empty());
        }
    }

    #[test]
    fn a_failing_step_after_the_commit_still_installs() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let committed_at = commit_step(&steps);

        for fail_at in committed_at + 1..steps.len() {
            let (game, zip, req) = scenario();
            fault::set(Plan {
                fail_at: vec![fail_at],
                ..Plan::default()
            });
            let outcome = game.install(&zip, &req).unwrap();
            fault::take();
            assert_eq!(
                game.addons(),
                tree_from(AFTER),
                "failed at {fail_at}: {}",
                steps[fail_at]
            );
            if steps[fail_at] != "remove file" || fail_at + 1 < steps.len() {
                assert_eq!(
                    outcome.warnings.len(),
                    1,
                    "failed at {fail_at}: {:?}",
                    outcome.warnings
                );
            }
            recover(&game.layout).unwrap();
            assert_eq!(game.addons(), tree_from(AFTER));
            game.assert_no_leftovers();
        }
    }

    #[test]
    fn when_rollback_fails_too_the_journal_survives_and_recover_finishes() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let committed_at = commit_step(&steps);
        let renames: Vec<usize> = (1..committed_at)
            .filter(|i| steps[*i] == "rename" && steps[*i - 1] != "journal pending")
            .collect();
        assert!(renames.len() >= 4, "{steps:?}");

        // Fail each swap rename, then the first thing rollback tries. Rollback's own
        // steps follow straight after the failed one in the log.
        for fail_at in renames {
            let (game, zip, req) = scenario();
            fault::set(Plan {
                fail_at: vec![fail_at, fail_at + 1],
                ..Plan::default()
            });
            let result = game.install(&zip, &req);
            let log = fault::take().log;
            let text = result.unwrap_err().to_string();

            if log.len() > fail_at + 1 && log[fail_at + 1] == "rename" {
                assert!(text.contains("Nothing was deleted"), "{text}");
                assert!(
                    game.layout
                        .work_dir
                        .join("j")
                        .read_dir()
                        .unwrap()
                        .next()
                        .is_some()
                );
            }
            let notes = recover(&game.layout).unwrap();
            assert_eq!(
                game.addons(),
                tree_from(BEFORE),
                "failed at {fail_at}: {notes:?}"
            );
            game.assert_no_leftovers();
        }
    }

    #[test]
    fn transient_locks_are_waited_out() {
        let (game, zip, req) = scenario();
        fault::set(Plan {
            lock_errors: 3,
            ..Plan::default()
        });
        let result = game.install(&zip, &req);
        fault::take();
        result.unwrap();
        assert_eq!(game.addons(), tree_from(AFTER));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_lock_that_never_clears_gives_up_after_about_five_seconds() {
        let (game, zip, req) = scenario();
        let started = Instant::now();
        fault::set(Plan {
            lock_errors: usize::MAX,
            ..Plan::default()
        });
        let result = game.install(&zip, &req);
        fault::take();
        let waited = started.elapsed();
        assert!(result.is_err());
        assert!(
            waited >= Duration::from_secs(4) && waited < Duration::from_secs(10),
            "{waited:?}"
        );
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&fault::lock_error()));
        assert!(!is_transient(&io::Error::new(
            io::ErrorKind::NotFound,
            "gone"
        )));
        #[cfg(windows)]
        for code in [5, 32, 33] {
            assert!(is_transient(&io::Error::from_raw_os_error(code)), "{code}");
        }
        #[cfg(windows)]
        assert!(!is_transient(&io::Error::from_raw_os_error(2)));
    }

    /// Open a file so nothing else can touch it. Antivirus may be scanning a file this
    /// new, in which case the open itself has to wait its turn.
    #[cfg(windows)]
    fn hold_open(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;

        let mut attempts = 0;
        loop {
            match OpenOptions::new().read(true).share_mode(0).open(path) {
                Ok(file) => return file,
                Err(error) if attempts < 100 => {
                    assert_eq!(error.raw_os_error(), Some(32), "{error}");
                    attempts += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => panic!("couldn't hold {}: {error}", path.display()),
            }
        }
    }

    /// Windows refuses to rename a folder with an open file in it. `Foo` has already
    /// swapped by the time `FooOld` fails, so this needs a real rollback.
    #[cfg(windows)]
    #[test]
    fn a_held_open_file_makes_the_swap_retry_then_roll_back_cleanly() {
        let (game, zip, req) = scenario();
        let handle = hold_open(&game.layout.addons_dir.join("FooOld/FooOld.toc"));

        let started = Instant::now();
        let result = game.install(&zip, &req);
        assert!(started.elapsed() >= Duration::from_secs(4));
        let text = result.unwrap_err().to_string();
        assert!(
            text.contains("FooOld") && text.contains("using it"),
            "{text}"
        );

        drop(handle);
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
        assert!(recover(&game.layout).unwrap().is_empty());
    }

    /// The new folder moved in and a crash followed. Now it's held open, so `recover`
    /// can't move it back yet. It must give up without losing either copy, and finish
    /// on a later run once the file is free.
    #[cfg(windows)]
    #[test]
    fn a_held_open_file_blocks_recover_safely_until_it_is_released() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"new")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let entries = [SwapEntry {
            old: Some("Foo".into()),
            new: Some("Foo".into()),
        }];
        write_tree(&dirs.backup("aaaaaaaa"), &[("Foo/Foo.toc", b"old")]);
        fs::create_dir_all(dirs.staged("aaaaaaaa")).unwrap();
        write_journal(&dirs, "aaaaaaaa", JournalState::Pending, &entries).unwrap();
        let handle = hold_open(&game.layout.addons_dir.join("Foo/Foo.toc"));

        let started = Instant::now();
        let result = recover(&game.layout);
        assert!(started.elapsed() >= Duration::from_secs(4));
        let text = result.unwrap_err().to_string();
        assert!(
            text.contains("Foo") && text.contains("Nothing was deleted"),
            "{text}"
        );
        drop(handle);
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"new")]));
        assert_eq!(
            tree_of(&dirs.backup("aaaaaaaa")),
            tree_from(&[("Foo/Foo.toc", b"old")])
        );

        let notes = recover(&game.layout).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"old")]));
        game.assert_no_leftovers();
    }

    #[test]
    fn swap_errors_say_what_to_do() {
        let failure = |error| SwapFailure {
            folder: "Foo".into(),
            error,
        };
        let text = swap_error(failure(fault::lock_error())).to_string();
        assert!(text.contains("Foo") && text.contains("Close WoW"), "{text}");
        let text = swap_error(failure(io::ErrorKind::CrossesDevices.into())).to_string();
        assert!(text.contains("another drive"), "{text}");
        let text = swap_error(failure(io::ErrorKind::NotFound.into())).to_string();
        assert!(
            text.contains("Foo") && text.contains("weren't changed"),
            "{text}"
        );
    }

    #[test]
    fn rollback_never_moves_an_old_folder_that_was_never_swapped() {
        // Staging has gone missing, as if something outside the app deleted it. With the
        // old folder not aside yet, what's in AddOns is the original and must stay.
        let game = Game::with_addons(&[("Foo/Foo.toc", b"original")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let entries = [SwapEntry {
            old: Some("Foo".into()),
            new: Some("Foo".into()),
        }];
        fs::create_dir_all(dirs.backup("aaaaaaaa")).unwrap();
        write_journal(&dirs, "aaaaaaaa", JournalState::Pending, &entries).unwrap();

        recover(&game.layout).unwrap();
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"original")]));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_rolled_back_journal_with_staging_half_gone_moves_nothing() {
        // The catastrophe this state exists to prevent: re-running rollback here would
        // mistake the restored folder for a moved-in one, and delete it with staging.
        let game = Game::with_addons(&[("Foo/Foo.toc", b"original")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let entries = [SwapEntry {
            old: Some("Foo".into()),
            new: Some("Foo".into()),
        }];
        fs::create_dir_all(dirs.staged("aaaaaaaa")).unwrap();
        write_journal(&dirs, "aaaaaaaa", JournalState::RolledBack, &entries).unwrap();

        recover(&game.layout).unwrap();
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"original")]));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_committed_journal_with_backup_half_gone_finishes() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"new")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let entries = [SwapEntry {
            old: Some("Foo".into()),
            new: Some("Foo".into()),
        }];
        write_tree(&dirs.backup("aaaaaaaa"), &[("Foo/left.lua", b"old")]);
        write_journal(&dirs, "aaaaaaaa", JournalState::Committed, &entries).unwrap();

        let notes = recover(&game.layout).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"new")]));
        game.assert_no_leftovers();
    }

    #[test]
    fn pending_state_built_by_hand_rolls_back() {
        // Old aside, new in AddOns, a second old still in place.
        let game = Game::with_addons(&[("Foo/Foo.toc", b"new"), ("Bar/Bar.toc", b"old bar")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let entries = [
            SwapEntry {
                old: Some("Foo".into()),
                new: Some("Foo".into()),
            },
            SwapEntry {
                old: Some("Bar".into()),
                new: None,
            },
        ];
        write_tree(&dirs.backup("aaaaaaaa"), &[("Foo/Foo.toc", b"old foo")]);
        fs::create_dir_all(dirs.staged("aaaaaaaa")).unwrap();
        write_journal(&dirs, "aaaaaaaa", JournalState::Pending, &entries).unwrap();

        let notes = recover(&game.layout).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("Foo") && notes[0].contains("Bar"),
            "{notes:?}"
        );
        assert_eq!(
            game.addons(),
            tree_from(&[("Foo/Foo.toc", b"old foo"), ("Bar/Bar.toc", b"old bar")])
        );
        game.assert_no_leftovers();
    }

    #[test]
    fn a_damaged_journal_leaves_everything_for_a_human() {
        for damage in [
            &b"{ not json"[..],
            b"",
            br#"{"version":1,"state":"pending","entries":[{"old":"../../evil"}]}"#,
            br#"{"version":99,"state":"pending","entries":[]}"#,
        ] {
            let game = Game::with_addons(&[("Foo/Foo.toc", b"new")]);
            let dirs = Dirs::new(&game.layout);
            dirs.create().unwrap();
            write_tree(&dirs.backup("aaaaaaaa"), &[("Foo/Foo.toc", b"old")]);
            fs::write(dirs.journal("aaaaaaaa"), damage).unwrap();

            let notes = recover(&game.layout).unwrap();
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("left in"), "{notes:?}");
            assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"new")]));
            assert_eq!(
                tree_of(&dirs.backup("aaaaaaaa")),
                tree_from(&[("Foo/Foo.toc", b"old")])
            );
        }
    }

    #[test]
    fn a_backup_without_a_journal_is_left_alone() {
        let game = Game::with_addons(&[]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        write_tree(&dirs.backup("aaaaaaaa"), &[("Foo/Foo.toc", b"unique")]);
        let notes = recover(&game.layout).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            tree_of(&dirs.backup("aaaaaaaa")),
            tree_from(&[("Foo/Foo.toc", b"unique")])
        );
    }

    #[test]
    fn recover_deletes_stale_downloads_and_staging() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"mine")]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        fs::write(dirs.zip("11111111"), b"partial").unwrap();
        fs::write(dirs.zip_part("22222222"), b"partial").unwrap();
        write_tree(&dirs.staged("33333333"), &[("Foo/Foo.toc", b"staged")]);
        fs::write(dirs.journal_tmp("44444444"), b"half a journal").unwrap();
        fs::write(dirs.lock("55555555"), b"").unwrap();
        let notes = recover(&game.layout).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", b"mine")]));
        game.assert_no_leftovers();
    }

    #[test]
    fn recover_ignores_files_it_does_not_recognize() {
        let game = Game::with_addons(&[]);
        let dirs = Dirs::new(&game.layout);
        dirs.create().unwrap();
        let foreign = dirs.staging.join("not-ours");
        write_tree(&foreign, &[("x.txt", b"hello")]);
        assert!(recover(&game.layout).unwrap().is_empty());
        assert!(foreign.join("x.txt").exists());
    }

    #[test]
    fn recover_with_no_work_folder_does_nothing_and_creates_nothing() {
        let game = Game::with_addons(&[]);
        assert!(recover(&game.layout).unwrap().is_empty());
        assert!(!game.layout.work_dir.exists());
    }

    #[test]
    fn recover_leaves_a_live_transaction_alone() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"mine")]);
        let txn = Txn::begin(&game.layout).unwrap();
        write_tree(&txn.dirs.staged(&txn.id), &[("Foo/Foo.toc", b"staged")]);
        let staged = txn.dirs.staged(&txn.id);

        assert!(recover(&game.layout).unwrap().is_empty());
        assert!(
            staged.join("Foo/Foo.toc").exists(),
            "recover deleted a running install's files"
        );

        txn.abandon();
        assert!(!staged.exists());
        game.assert_no_leftovers();
    }

    #[test]
    fn recover_rolls_back_a_transaction_whose_process_died() {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let midway = steps
            .iter()
            .rposition(|s| *s == "journal committed")
            .unwrap()
            - 1;
        let (game, zip, req) = scenario();
        fault::set(Plan {
            crash_at: Some(midway),
            ..Plan::default()
        });
        let _ = game.install(&zip, &req);
        fault::take();
        // Mixed up on purpose: this is what a player would see if the app died here.
        assert_ne!(game.addons(), tree_from(BEFORE));
        assert_ne!(game.addons(), tree_from(AFTER));
        let notes = recover(&game.layout).unwrap();
        assert!(notes[0].contains("put back"), "{notes:?}");
        assert_eq!(game.addons(), tree_from(BEFORE));
    }

    /// The process died mid-swap and nothing has run `recover` yet.
    fn scenario_after_a_crash_mid_swap() -> (Game, PathBuf, InstallRequest) {
        let (game, zip, req) = scenario();
        let steps = steps_of(|| {
            game.install(&zip, &req).unwrap();
        });
        let midway = steps
            .iter()
            .rposition(|s| *s == "journal committed")
            .unwrap()
            - 1;
        let (game, zip, req) = scenario();
        fault::set(Plan {
            crash_at: Some(midway),
            ..Plan::default()
        });
        let _ = game.install(&zip, &req);
        fault::take();
        assert_ne!(game.addons(), tree_from(BEFORE));
        (game, zip, req)
    }

    #[test]
    fn a_new_install_settles_an_unfinished_one_first() {
        let (game, zip, req) = scenario_after_a_crash_mid_swap();
        let outcome = game.install(&zip, &req).unwrap();
        assert_eq!(game.addons(), tree_from(AFTER));
        assert!(
            outcome.warnings.iter().any(|w| w.contains("put back")),
            "{:?}",
            outcome.warnings
        );
        game.assert_no_leftovers();

        // Without settling first, this rollback would quietly undo the install above.
        assert!(recover(&game.layout).unwrap().is_empty());
        assert_eq!(game.addons(), tree_from(AFTER));
    }

    #[test]
    fn a_removal_settles_an_unfinished_install_first() {
        let (game, _, _) = scenario_after_a_crash_mid_swap();
        remove(&game.layout, &["Foo".into(), "FooOld".into()]).unwrap();
        let mut expected = tree_from(BEFORE);
        expected.retain(|path, _| !path.starts_with("Foo/") && !path.starts_with("FooOld/"));
        assert_eq!(game.addons(), expected);
        game.assert_no_leftovers();
    }

    #[test]
    fn a_new_install_refuses_to_build_on_an_unfinished_one_it_cannot_settle() {
        let (game, zip, req) = scenario_after_a_crash_mid_swap();
        let stuck = game.addons();
        fault::set(Plan {
            fail_at: vec![0],
            ..Plan::default()
        });
        let result = game.install(&zip, &req);
        fault::take();
        let text = result.unwrap_err().to_string();
        assert!(text.contains("Nothing was deleted"), "{text}");
        assert_eq!(game.addons(), stuck, "nothing may move until it's settled");

        // The new install's own files are gone, the old transaction's are still safe.
        recover(&game.layout).unwrap();
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn two_interrupted_transactions_both_recover() {
        let (game, _, _) = scenario_after_a_crash_mid_swap();
        let dirs = Dirs::new(&game.layout);
        write_tree(&dirs.staged("bbbbbbbb"), &[("Stale/Stale.toc", b"x")]);
        let notes = recover(&game.layout).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn folders_claimed_by_no_one_are_untouched_on_failure_and_success() {
        let (game, zip, req) = scenario();
        let unclaimed = |game: &Game| -> Tree {
            game.addons()
                .into_iter()
                .filter(|(path, _)| path.starts_with("Unclaimed/") || path.starts_with("!Bug"))
                .collect()
        };
        let before = unclaimed(&game);
        let bad_zip = game.zip(
            "bad.zip",
            &[("Foo/Foo.toc", FOREVER_TOC), ("../evil", b"x")],
        );
        assert!(game.install(&bad_zip, &req).is_err());
        assert_eq!(unclaimed(&game), before);
        game.install(&zip, &req).unwrap();
        assert_eq!(unclaimed(&game), before);
    }

    #[test]
    fn a_same_named_unmanaged_folder_goes_through_backup_and_swap() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"unmanaged"), ("Foo/mine.lua", b"x")]);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        game.install(&zip, &request(&[])).unwrap();
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", FOREVER_TOC)]));
    }

    #[test]
    fn a_new_install_leaves_everything_else_alone() {
        let game = Game::with_addons(&[("Other/Other.toc", b"x")]);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        game.install(&zip, &request(&[])).unwrap();
        assert_eq!(
            game.addons(),
            tree_from(&[("Foo/Foo.toc", FOREVER_TOC), ("Other/Other.toc", b"x")])
        );
    }

    #[test]
    fn a_rollback_restores_the_original_spelling() {
        let game = Game::with_addons(&[("foo/foo.toc", b"lower")]);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        let steps = steps_of(|| {
            let scratch = Game::with_addons(&[("foo/foo.toc", b"lower")]);
            let zip = scratch.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
            scratch.install(&zip, &request(&["foo"])).unwrap();
        });
        let committed_at = commit_step(&steps);
        fault::set(Plan {
            fail_at: vec![committed_at],
            ..Plan::default()
        });
        assert!(game.install(&zip, &request(&["foo"])).is_err());
        fault::take();
        let names: Vec<String> = fs::read_dir(&game.layout.addons_dir)
            .unwrap()
            .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["foo"]);
        game.assert_no_leftovers();
    }

    #[test]
    fn replacing_a_folder_spelled_differently_leaves_one_folder() {
        let game = Game::with_addons(&[("foo/foo.toc", b"lower"), ("foo/old.lua", b"x")]);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        game.install(&zip, &request(&["foo"])).unwrap();
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", FOREVER_TOC)]));
    }

    #[test]
    fn swap_planning() {
        let names = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
        let step = |old: Option<&str>, new: Option<&str>| SwapEntry {
            old: old.map(String::from),
            new: new.map(String::from),
        };

        assert_eq!(
            plan_swap(
                &names(&["A", "B", "C"]),
                &names(&["B", "D"]),
                &names(&["A", "B", "Gone"])
            ),
            [
                step(Some("B"), Some("B")),
                step(None, Some("D")),
                step(Some("A"), None)
            ]
        );
        // Spelling on disk wins, and an old folder in the new set isn't listed twice.
        assert_eq!(
            plan_swap(&names(&["foo"]), &names(&["Foo"]), &names(&["FOO"])),
            [step(Some("foo"), Some("Foo"))]
        );
        // Both spellings on a case-sensitive disk go when a new one comes in.
        assert_eq!(
            plan_swap(&names(&["foo", "Foo"]), &names(&["FOO"]), &[]),
            [step(Some("Foo"), None), step(Some("foo"), Some("FOO"))]
        );
        // Removal prefers the exact name, and leaves the other spelling alone.
        assert_eq!(
            plan_swap(&names(&["foo", "Foo"]), &[], &names(&["Foo"])),
            [step(Some("Foo"), None)]
        );
        assert_eq!(
            plan_swap(&names(&["foo"]), &[], &names(&["FOO"])),
            [step(Some("foo"), None)]
        );
        assert_eq!(plan_swap(&names(&["foo"]), &[], &names(&["bar"])), []);
        assert_eq!(
            plan_swap(&names(&["a"]), &[], &names(&["a", "A"])),
            [step(Some("a"), None)]
        );
    }

    #[test]
    fn digest_and_size_mismatches_change_nothing() {
        let (game, zip, req) = scenario();
        for bad in [
            InstallRequest {
                sha256: Some("0".repeat(64)),
                ..req.clone()
            },
            InstallRequest {
                md5: Some("0".repeat(32)),
                ..req.clone()
            },
            InstallRequest {
                size: 1,
                ..req.clone()
            },
        ] {
            let result = game.install(&zip, &bad);
            assert!(
                matches!(result, Err(Error::Corrupt(_))),
                "{bad:?}: {result:?}"
            );
            assert_eq!(game.addons(), tree_from(BEFORE));
            game.assert_no_leftovers();
        }
    }

    #[test]
    fn matching_digests_install_and_sha256_takes_priority() {
        let bytes = zip_bytes(NEW_ZIP);
        let sha = to_hex(&Sha256::digest(&bytes));
        let md5 = to_hex(&Md5::digest(&bytes));
        for good in [
            (Some(sha.clone()), None),
            (None, Some(md5.clone())),
            (Some(sha.to_uppercase()), None),
            (Some(sha.clone()), Some("0".repeat(32))),
            (Some(String::new()), Some(md5.clone())),
        ] {
            let (game, _, req) = scenario();
            let zip = game.dir.path().join("exact.zip");
            fs::write(&zip, &bytes).unwrap();
            let request = InstallRequest {
                sha256: good.0,
                md5: good.1,
                size: bytes.len() as u64,
                ..req
            };
            game.install(&zip, &request).unwrap();
            assert_eq!(game.addons(), tree_from(AFTER));
        }
    }

    #[test]
    fn unsafe_zips_and_zips_without_addons_change_nothing() {
        let (game, _, req) = scenario();
        let evil = game.zip(
            "evil.zip",
            &[("Foo/Foo.toc", FOREVER_TOC), ("Foo/../../evil.lua", b"x")],
        );
        assert!(matches!(
            game.install(&evil, &req),
            Err(Error::UnsafeZip(_))
        ));
        let empty = game.zip("empty.zip", &[("readme.txt", b"x")]);
        assert!(matches!(
            game.install(&empty, &req),
            Err(Error::NoAddonInZip)
        ));
        let not_zip = game.dir.path().join("not.zip");
        fs::write(&not_zip, b"nope").unwrap();
        assert!(matches!(
            game.install(&not_zip, &req),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn cancelling_before_the_swap_changes_nothing() {
        let (game, zip, req) = scenario();
        let cancel = AtomicBool::new(true);
        let result = install_from_zip(&game.layout, &zip, &req, &mut |_| {}, &cancel);
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn cancelling_at_the_last_moment_changes_nothing() {
        let (game, zip, req) = scenario();
        let cancel = AtomicBool::new(false);
        let result = install_from_zip(
            &game.layout,
            &zip,
            &req,
            &mut |stage| {
                if matches!(stage, Stage::Extracting { done, total } if done == total && total > 0)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn stages_come_in_order() {
        let (game, zip, req) = scenario();
        let mut stages = Vec::new();
        install_from_zip(
            &game.layout,
            &zip,
            &req,
            &mut |s| stages.push(s),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(stages.first(), Some(&Stage::Verifying));
        assert!(stages.iter().any(|s| matches!(s, Stage::Extracting { .. })));
        assert_eq!(&stages[stages.len() - 2..], [Stage::Swapping, Stage::Done]);
    }

    #[test]
    fn install_from_url_downloads_verifies_and_installs() {
        let bytes = zip_bytes(NEW_ZIP);
        let sha = to_hex(&Sha256::digest(&bytes));
        let size = bytes.len() as u64;
        let server = serve(move |_, stream| respond(stream, "200 OK", &[], &bytes));
        let (game, _, req) = scenario();
        let request = InstallRequest {
            url: format!("{}/addon.zip", server.base),
            sha256: Some(sha),
            size,
            ..req
        };
        let mut stages = Vec::new();
        let outcome = install_from_url(
            &Http::allowing_loopback(),
            &game.layout,
            &request,
            &mut |s| stages.push(s),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(game.addons(), tree_from(AFTER));
        assert_eq!(outcome.forever_folders, ["Foo", "FooPlugin"]);
        assert!(matches!(stages[0], Stage::Downloading { done: 0, total: Some(t) } if t == size));
        let verifying = stages.iter().position(|s| *s == Stage::Verifying).unwrap();
        assert!(
            stages[..verifying]
                .iter()
                .all(|s| matches!(s, Stage::Downloading { .. }))
        );
        assert_eq!(stages.last(), Some(&Stage::Done));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_failed_download_changes_nothing() {
        let server = serve(|_, stream| respond(stream, "404 Not Found", &[], b""));
        let (game, _, req) = scenario();
        let request = InstallRequest {
            url: format!("{}/gone.zip", server.base),
            ..req
        };
        let result = install_from_url(
            &Http::allowing_loopback(),
            &game.layout,
            &request,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::HttpStatus { status: 404, .. })));
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_download_larger_than_the_catalog_says_is_rejected() {
        let bytes = zip_bytes(NEW_ZIP);
        let server = serve(move |_, stream| respond(stream, "200 OK", &[], &bytes));
        let (game, _, req) = scenario();
        let request = InstallRequest {
            url: format!("{}/a.zip", server.base),
            size: 10,
            ..req
        };
        let result = install_from_url(
            &Http::allowing_loopback(),
            &game.layout,
            &request,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn a_zip_with_no_forever_folder_warns_but_installs() {
        let game = Game::with_addons(&[]);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", RETAIL_TOC)]);
        let outcome = game.install(&zip, &request(&[])).unwrap();
        assert_eq!(outcome.folders, ["Foo"]);
        assert!(outcome.forever_folders.is_empty());
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("Forever"));
        assert!(game.addons().contains_key("Foo/Foo.toc"));
    }

    #[test]
    fn forever_detection_reads_the_toc_the_client_would_load() {
        let game = Game::with_addons(&[]);
        let zip = game.zip(
            "z.zip",
            &[
                // The Questie trap: a plain TOC with a placeholder, saved by a suffixed one.
                ("Questie/Questie.toc", b"## Interface: 00000\n"),
                ("Questie/Questie_Forever.toc", FOREVER_TOC),
                ("Retail/Retail.toc", RETAIL_TOC),
                ("Mixed/Mixed.toc", FOREVER_TOC),
                ("Mixed/Mixed_Mainline.toc", RETAIL_TOC),
                ("Named/Wrong.toc", FOREVER_TOC),
            ],
        );
        let outcome = game.install(&zip, &request(&[])).unwrap();
        assert_eq!(outcome.folders, ["Mixed", "Named", "Questie", "Retail"]);
        assert_eq!(outcome.forever_folders, ["Questie"]);
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn skipped_files_are_reported() {
        let game = Game::with_addons(&[]);
        let zip = game.zip(
            "z.zip",
            &[("Foo/Foo.toc", FOREVER_TOC), ("Foo/tool.exe", b"MZ")],
        );
        let outcome = game.install(&zip, &request(&[])).unwrap();
        assert_eq!(outcome.skipped_files, ["Foo/tool.exe"]);
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", FOREVER_TOC)]));
    }

    #[test]
    fn unsafe_old_folder_names_are_refused_up_front() {
        let (game, zip, _) = scenario();
        for bad in ["../..", "..", "a/b", "C:\\Windows", "", "CON"] {
            let result = game.install(&zip, &request(&[bad]));
            assert!(result.is_err(), "{bad}");
            let result = remove(&game.layout, &[bad.to_string()]);
            assert!(result.is_err(), "{bad}");
            assert_eq!(game.addons(), tree_from(BEFORE));
        }
    }

    #[test]
    fn remove_takes_only_the_named_folders() {
        let game = Game::with_addons(BEFORE);
        let removed = remove(
            &game.layout,
            &["Foo".into(), "FooOld".into(), "NotThere".into()],
        )
        .unwrap();
        assert_eq!(removed, ["Foo", "FooOld"]);
        assert_eq!(
            game.addons(),
            tree_from(&[
                ("Unclaimed/Unclaimed.toc", b"mine"),
                ("Unclaimed/settings.lua", b"mine too"),
                ("!BugGrabber/!BugGrabber.toc", b"someone else's"),
            ])
        );
        game.assert_no_leftovers();
    }

    #[test]
    fn removing_nothing_is_fine() {
        let game = Game::with_addons(BEFORE);
        assert!(
            remove(&game.layout, &["NotThere".into()])
                .unwrap()
                .is_empty()
        );
        assert!(remove(&game.layout, &[]).unwrap().is_empty());
        assert_eq!(game.addons(), tree_from(BEFORE));
        game.assert_no_leftovers();
    }

    #[test]
    fn remove_never_climbs_out_of_addons() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"x")]);
        let outside = game.layout.interface_dir.join("Precious.txt");
        fs::write(&outside, b"keep").unwrap();
        assert!(remove(&game.layout, &["../Precious.txt".into()]).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn a_crash_at_every_step_of_remove_ends_exactly_before_or_exactly_after() {
        let names: Vec<String> = vec!["Foo".into(), "FooOld".into()];
        let removed: &[(&str, &[u8])] = &[
            ("Unclaimed/Unclaimed.toc", b"mine"),
            ("Unclaimed/settings.lua", b"mine too"),
            ("!BugGrabber/!BugGrabber.toc", b"someone else's"),
        ];
        let game = Game::with_addons(BEFORE);
        let steps = steps_of(|| {
            remove(&game.layout, &names).unwrap();
        });
        let committed_at = commit_step(&steps);

        for crash_at in 0..steps.len() {
            let expected = if crash_at > committed_at {
                removed
            } else {
                BEFORE
            };
            let mut recovery_crash = 0;
            loop {
                let game = Game::with_addons(BEFORE);
                fault::set(Plan {
                    crash_at: Some(crash_at),
                    ..Plan::default()
                });
                let _ = remove(&game.layout, &names);
                fault::take();

                fault::set(Plan {
                    crash_at: Some(recovery_crash),
                    ..Plan::default()
                });
                let _ = recover(&game.layout);
                let reached = fault::take().log.len();

                recover(&game.layout).unwrap();
                assert_eq!(
                    game.addons(),
                    tree_from(expected),
                    "crash at {crash_at}/{recovery_crash}"
                );
                game.assert_no_leftovers();
                if reached <= recovery_crash {
                    break;
                }
                recovery_crash += 1;
            }
        }
    }

    #[test]
    fn a_failing_step_of_remove_rolls_back() {
        let names: Vec<String> = vec!["Foo".into(), "FooOld".into()];
        let game = Game::with_addons(BEFORE);
        let steps = steps_of(|| {
            remove(&game.layout, &names).unwrap();
        });
        for (fail_at, step) in steps.iter().enumerate().take(commit_step(&steps) + 1) {
            let game = Game::with_addons(BEFORE);
            fault::set(Plan {
                fail_at: vec![fail_at],
                ..Plan::default()
            });
            let result = remove(&game.layout, &names);
            fault::take();
            assert!(result.is_err(), "step {fail_at}");
            assert_eq!(game.addons(), tree_from(BEFORE), "step {fail_at}: {step}");
            game.assert_no_leftovers();
        }
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
    fn removing_a_linked_folder_keeps_what_it_points_at() {
        let game = Game::with_addons(&[("Other/Other.toc", b"x")]);
        let repo = game.dir.path().join("dev-repo");
        write_tree(
            &repo,
            &[
                ("Foo.toc", b"precious"),
                ("src/core.lua", b"work in progress"),
            ],
        );
        link_folder(&repo, &game.layout.addons_dir.join("Foo"));
        remove(&game.layout, &["Foo".into()]).unwrap();
        assert!(fs::symlink_metadata(game.layout.addons_dir.join("Foo")).is_err());
        assert_eq!(
            fs::read(repo.join("src/core.lua")).unwrap(),
            b"work in progress"
        );
        game.assert_no_leftovers();
    }

    #[test]
    fn replacing_a_linked_folder_keeps_what_it_points_at_and_rolls_back_the_link() {
        let game = Game::with_addons(&[]);
        let repo = game.dir.path().join("dev-repo");
        write_tree(&repo, &[("Foo.toc", b"precious")]);
        let link = game.layout.addons_dir.join("Foo");
        link_folder(&repo, &link);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);

        let steps = steps_of(|| {
            game.install(&zip, &request(&[])).unwrap();
        });
        assert_eq!(fs::read(repo.join("Foo.toc")).unwrap(), b"precious");
        assert_eq!(fs::read(link.join("Foo.toc")).unwrap(), FOREVER_TOC);

        let game = Game::with_addons(&[]);
        let repo = game.dir.path().join("dev-repo");
        write_tree(&repo, &[("Foo.toc", b"precious")]);
        let link = game.layout.addons_dir.join("Foo");
        link_folder(&repo, &link);
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        fault::set(Plan {
            fail_at: vec![commit_step(&steps)],
            ..Plan::default()
        });
        assert!(game.install(&zip, &request(&[])).is_err());
        fault::take();
        assert_eq!(fs::read(link.join("Foo.toc")).unwrap(), b"precious");
        game.assert_no_leftovers();
    }

    #[test]
    fn old_files_marked_read_only_do_not_block_an_update() {
        let game = Game::with_addons(&[("Foo/Foo.toc", b"old"), ("Foo/locked.lua", b"old")]);
        let locked = game.layout.addons_dir.join("Foo/locked.lua");
        let mut permissions = fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&locked, permissions).unwrap();
        let zip = game.zip("z.zip", &[("Foo/Foo.toc", FOREVER_TOC)]);
        let outcome = game.install(&zip, &request(&["Foo"])).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(game.addons(), tree_from(&[("Foo/Foo.toc", FOREVER_TOC)]));
        game.assert_no_leftovers();
    }

    #[test]
    fn everything_works_under_a_very_long_game_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut game_dir = long_path(dir.path());
        for i in 0..6 {
            game_dir.push(format!("a-long-folder-name-for-testing-long-paths-{i:02}"));
        }
        let addons = game_dir.join("_forever_").join("Interface").join("AddOns");
        assert!(addons.as_os_str().len() > 260);
        fs::create_dir_all(&addons).unwrap();
        let layout = Layout::from_addons_dir(&addons).unwrap();

        let deep = format!(
            "Foo/{}/deep.lua",
            (0..8)
                .map(|i| format!("nested-folder-{i}"))
                .collect::<Vec<_>>()
                .join("/")
        );
        let zip = dir.path().join("deep.zip");
        fs::write(
            &zip,
            zip_bytes(&[("Foo/Foo.toc", FOREVER_TOC), (&deep, b"deep")]),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let req = request(&["Foo"]);
        install_from_zip(&layout, &zip, &req, &mut |_| {}, &cancel).unwrap();
        let mut deep_path = addons.clone();
        deep_path.extend(deep.split('/'));
        assert_eq!(fs::read(&deep_path).unwrap(), b"deep");

        // A second install swaps the whole deep tree into backup and back out.
        install_from_zip(&layout, &zip, &req, &mut |_| {}, &cancel).unwrap();
        assert_eq!(fs::read(&deep_path).unwrap(), b"deep");
        remove(&layout, &["Foo".to_string()]).unwrap();
        assert!(!addons.join("Foo").exists());
        assert!(recover(&layout).unwrap().is_empty());
    }

    #[test]
    fn concurrent_installs_of_the_same_folder_never_mix() {
        let game = Arc::new(Game::with_addons(&[("Shared/Shared.toc", b"old")]));
        let zip_a = game.zip(
            "a.zip",
            &[("Shared/Shared.toc", FOREVER_TOC), ("Shared/a.lua", b"a")],
        );
        let zip_b = game.zip(
            "b.zip",
            &[("Shared/Shared.toc", FOREVER_TOC), ("Shared/b.lua", b"b")],
        );
        let threads: Vec<_> = [zip_a, zip_b]
            .into_iter()
            .flat_map(|zip| [zip.clone(), zip])
            .map(|zip| {
                let game = game.clone();
                thread::spawn(move || game.install(&zip, &request(&["Shared"])).unwrap())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let tree = game.addons();
        let only_a = tree_from(&[("Shared/Shared.toc", FOREVER_TOC), ("Shared/a.lua", b"a")]);
        let only_b = tree_from(&[("Shared/Shared.toc", FOREVER_TOC), ("Shared/b.lua", b"b")]);
        assert!(tree == only_a || tree == only_b, "{tree:?}");
        game.assert_no_leftovers();
    }

    #[test]
    fn a_kept_zip_lands_in_the_cache_and_keeps_one_previous_version() {
        let game = Game::with_addons(&[]);
        let cache = tempfile::tempdir().unwrap();
        let mut req = InstallRequest {
            addon_id: "gh:owner/repo".into(),
            cache_dir: Some(cache.path().to_path_buf()),
            old_folders: vec!["Foo".into()],
            ..InstallRequest::default()
        };
        let mut kept = Vec::new();
        for version in ["1.0", "2.0", "3.0"] {
            let body = version.as_bytes();
            let zip = game.zip(
                "z.zip",
                &[("Foo/Foo.toc", FOREVER_TOC), ("Foo/version.txt", body)],
            );
            req.version = version.into();
            kept.push(game.install(&zip, &req).unwrap().kept_zip.unwrap());
            // Also proves the zip we were handed stays where it was.
            assert!(zip.exists());
            thread::sleep(Duration::from_millis(20));
        }
        assert!(kept[2].starts_with(cache.path().join("zips")));
        assert!(kept[2].ends_with("3.0.zip"));
        assert!(!kept[0].exists(), "only one previous version stays");
        assert_eq!(
            kept_zips(cache.path(), "gh:owner/repo"),
            [kept[2].clone(), kept[1].clone()]
        );
        assert!(kept_zips(cache.path(), "gh:someone/else").is_empty());

        // Rolling back to the previous version makes it the newest again.
        req.version = "2.0".into();
        let rolled_back = game.install(&kept[1], &req).unwrap();
        assert_eq!(game.addons()["Foo/version.txt"], b"2.0");
        assert_eq!(rolled_back.kept_zip.as_ref(), Some(&kept[1]));
        assert_eq!(
            kept_zips(cache.path(), "gh:owner/repo"),
            [kept[1].clone(), kept[2].clone()]
        );
        game.assert_no_leftovers();
    }

    #[test]
    fn a_downloaded_zip_is_kept_after_its_staging_copy_is_gone() {
        let bytes = zip_bytes(NEW_ZIP);
        let body = bytes.clone();
        let server = serve(move |_, stream| respond(stream, "200 OK", &[], &body));
        let cache = tempfile::tempdir().unwrap();
        let (game, _, req) = scenario();
        let request = InstallRequest {
            addon_id: "wowi:42".into(),
            version: "1.2".into(),
            url: format!("{}/a.zip", server.base),
            cache_dir: Some(cache.path().to_path_buf()),
            ..req
        };
        let outcome = install_from_url(
            &Http::allowing_loopback(),
            &game.layout,
            &request,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(outcome.kept_zip.unwrap()).unwrap(), bytes);
        game.assert_no_leftovers();
    }

    #[test]
    fn loose_zips_and_requests_without_a_cache_keep_nothing() {
        let (game, zip, req) = scenario();
        let cache = tempfile::tempdir().unwrap();
        let no_id = InstallRequest {
            cache_dir: Some(cache.path().to_path_buf()),
            ..req.clone()
        };
        assert_eq!(game.install(&zip, &no_id).unwrap().kept_zip, None);
        let no_cache = InstallRequest {
            addon_id: "gh:a/b".into(),
            ..req
        };
        assert_eq!(game.install(&zip, &no_cache).unwrap().kept_zip, None);
        assert!(!cache.path().join("zips").exists());
    }

    #[test]
    fn a_failure_to_keep_the_zip_only_warns() {
        let (game, zip, req) = scenario();
        let not_a_dir = game.dir.path().join("cache-is-a-file");
        fs::write(&not_a_dir, b"x").unwrap();
        let request = InstallRequest {
            addon_id: "gh:a/b".into(),
            cache_dir: Some(not_a_dir),
            ..req
        };
        let outcome = game.install(&zip, &request).unwrap();
        assert_eq!(outcome.kept_zip, None);
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert_eq!(game.addons(), tree_from(AFTER));
    }

    #[test]
    fn the_kept_zip_cache_evicts_the_oldest_first_and_spares_the_new_one() {
        let cache = tempfile::tempdir().unwrap();
        let source = cache.path().join("source.zip");
        fs::write(&source, vec![0u8; 400]).unwrap();
        let cancel_free_keep = |id: &str, version: &str| {
            let kept = keep_zip(cache.path(), id, version, &source, 1000).unwrap();
            thread::sleep(Duration::from_millis(20));
            kept
        };
        let first = cancel_free_keep("gh:a/one", "1").unwrap();
        let second = cancel_free_keep("gh:a/two", "1").unwrap();
        assert!(first.exists() && second.exists());

        let third = cancel_free_keep("gh:a/three", "1").unwrap();
        assert!(!first.exists(), "oldest should go past the cap");
        assert!(second.exists() && third.exists());
        assert!(
            !first.parent().unwrap().exists(),
            "its emptied folder goes too"
        );

        // Too big for the cache at all: nothing kept, and nothing else evicted for it.
        fs::write(&source, vec![0u8; 2000]).unwrap();
        assert_eq!(
            keep_zip(cache.path(), "gh:a/four", "1", &source, 1000).unwrap(),
            None
        );
        assert!(second.exists() && third.exists());
    }

    #[test]
    fn safe_names_for_ids_and_versions() {
        assert!(safe_id("gh:owner/repo").starts_with("gh_owner_repo-"));
        assert_ne!(safe_id("gh:a/b_c"), safe_id("gh:a_b/c"));
        for id in [
            "direct:con",
            "gh:../../evil",
            "a\\b:c",
            "wowi:1",
            "...",
            "x".repeat(500).as_str(),
        ] {
            let safe = safe_id(id);
            assert!(is_safe_folder_name(&safe), "{id} became {safe}");
            assert!(safe.len() < 120);
        }
        assert_eq!(kept_zip_name("v1.2.3"), "v1.2.3.zip");
        assert_eq!(
            kept_zip_name("bundle/v12.6.2+v1.0.11"),
            "bundle_v12.6.2+v1.0.11.zip"
        );
        for version in [
            "",
            "con",
            "NUL",
            "../../evil",
            "a:b",
            "COM1",
            "x. ",
            "ok.",
            "*",
        ] {
            let name = kept_zip_name(version);
            assert!(is_safe_folder_name(&name), "{version:?} became {name}");
            assert!(!name.contains('/'), "{name}");
        }
    }

    #[test]
    fn layout_comes_from_the_addons_path() {
        let layout = Layout::from_addons_dir("/games/wow/_forever_/Interface/AddOns").unwrap();
        assert!(layout.addons_dir.ends_with("Interface/AddOns"));
        assert!(layout.interface_dir.ends_with("Interface"));
        assert!(layout.work_dir.ends_with("Interface/.wiaf"));
        assert!(
            Layout::from_addons_dir("relative/Interface/addons")
                .unwrap()
                .addons_dir
                .is_absolute()
        );
        for bad in ["/games/wow", "/", "/games/AddOns2"] {
            assert!(Layout::from_addons_dir(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn staging_names_stay_short() {
        let game = Game::with_addons(&[]);
        let txn = Txn::begin(&game.layout).unwrap();
        assert_eq!(txn.id.len(), 8);
        assert!(txn.id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(txn.dirs.staged(&txn.id).parent().unwrap(), txn.dirs.staging);
        assert!(txn.dirs.staging.ends_with(".wiaf/s"));
        txn.abandon();
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(to_hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    #[ignore = "reaches the real internet; run by hand with --ignored"]
    fn live_install_of_a_real_addon_release_into_a_temp_folder() {
        let http = Http::new();
        let release = http
            .get_bytes(
                "https://api.github.com/repos/funkydude/BugSack/releases/latest",
                1024 * 1024,
            )
            .unwrap();
        let release: serde_json::Value = serde_json::from_slice(&release).unwrap();
        let asset = release["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|asset| asset["name"].as_str().unwrap().ends_with(".zip"))
            .unwrap();
        let game = Game::with_addons(&[]);
        let request = InstallRequest {
            url: asset["browser_download_url"].as_str().unwrap().to_string(),
            size: asset["size"].as_u64().unwrap(),
            ..InstallRequest::default()
        };
        let cancel = AtomicBool::new(false);
        let outcome =
            install_from_url(&http, &game.layout, &request, &mut |_| {}, &cancel).unwrap();
        println!("{outcome:#?}");
        assert!(!outcome.folders.is_empty());
        game.assert_no_leftovers();
    }
}
