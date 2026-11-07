//! Everything slow happens here. Each job gets its own thread, which sends what it found
//! down a channel, pokes the window and exits. Nothing polls, and the UI thread never
//! waits on any of it.

use crate::demo;
use crate::model::CatalogBundle;
use crate::ops::{Op, OpKind, stage_fraction};
use eframe::egui::{self, ColorImage};
use feeling_core::backup::{self, BackupEntry, BackupProgress};
use feeling_core::catalog::{CATALOG_SCHEMA, Catalog};
use feeling_core::detect::{self, FoundBy, Install};
use feeling_core::details::{self, Details};
use feeling_core::error::Error;
use feeling_core::gameproc;
use feeling_core::install::{self, InstallOutcome, InstallRequest, Layout, Stage};
use feeling_core::logging;
use feeling_core::net::{Fetched, Http};
use feeling_core::scan::{self, LocalFolder};
use feeling_core::selfupdate::{self, Applied, UpdateInfo};
use feeling_core::state::State;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// A catalog this big is something going wrong, not a lot of addons.
const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
/// Limits from the spec: anything over these is skipped, not squeezed in.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 4096;
const MAX_SHOWN_WIDTH: u32 = 640;
/// Two thirds of a typical monitor's refresh, and all a progress bar needs.
const PROGRESS_EVERY: Duration = Duration::from_millis(33);
const KEEP_BACKUPS: usize = 10;
const CATALOG_FILE: &str = "catalog.json";

/// Where this run keeps its files. Normally `paths`, but tests point it at a temp folder.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub state_file: PathBuf,
    pub cache: PathBuf,
    pub data: PathBuf,
}

impl Dirs {
    pub fn real() -> Self {
        Self {
            state_file: feeling_core::state::state_path(),
            cache: feeling_core::paths::cache_dir(),
            data: feeling_core::paths::data_dir(),
        }
    }

    pub fn backups(&self) -> PathBuf {
        self.data.join("wtf-backups")
    }
}

/// How the current install came to be chosen, for the Game folder page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HowFound {
    /// Saved from an earlier run and still good.
    Remembered,
    Detected(FoundBy),
    /// From the folder browser.
    Chosen,
    /// From `--game`.
    CommandLine,
}

impl HowFound {
    pub fn describe(&self) -> &str {
        match self {
            HowFound::Remembered => "Remembered from last time",
            HowFound::Chosen => "You chose this folder",
            HowFound::CommandLine => "Given on the command line",
            HowFound::Detected(by) => match by {
                FoundBy::BattleNet => "Found through Battle.net",
                FoundBy::Registry => "Found in the Windows registry",
                FoundBy::CommonPath => "Found in a usual install folder",
                FoundBy::WinePrefix(launcher) => launcher,
                FoundBy::Manual => "You chose this folder",
            },
        }
    }
}

/// A located game, with the facts that took a disk read to learn.
#[derive(Debug, Clone)]
pub struct FoundGame {
    pub install: Install,
    pub how: HowFound,
    pub key: String,
    /// The folder as the disk spells it.
    pub shown_path: String,
}

/// One folder's contents, for the built-in folder browser.
#[derive(Debug, Clone, Default)]
pub struct DirListing {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub folders: Vec<String>,
    pub drives: Vec<PathBuf>,
    /// `Some` when this folder is a Forever install, else the reason it isn't.
    pub verdict: Option<Result<FoundGame, String>>,
    /// Why the folder couldn't be read at all.
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum OpOutcome {
    Installed {
        outcome: InstallOutcome,
        kept: Vec<PathBuf>,
        /// For a dropped zip: the group its folders make, read back from disk.
        local: Option<LocalZipFacts>,
    },
    Removed,
    BackedUp,
    BackupSkipped,
    Restored {
        aside: Option<PathBuf>,
    },
    UpdateApplied(Box<Applied>),
}

/// What a dropped zip turned out to be.
#[derive(Debug, Clone)]
pub struct LocalZipFacts {
    pub main_folder: String,
    pub title: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpFailure {
    pub cancelled: bool,
    pub text: String,
}

impl From<Error> for OpFailure {
    fn from(error: Error) -> Self {
        OpFailure {
            cancelled: matches!(error, Error::Cancelled),
            text: error.to_string(),
        }
    }
}

/// Which way the catalog arrived.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatalogSource {
    Cache,
    Network,
}

pub enum Event {
    GameFound(Option<FoundGame>),
    Recovered(Result<Vec<String>, String>),
    WriteTest(Option<String>),
    Scanned {
        folders: Vec<LocalFolder>,
        /// Managed addons whose kept zip is gone.
        missing_zips: Vec<String>,
        error: Option<String>,
    },
    Catalog {
        bundle: Arc<CatalogBundle>,
        source: CatalogSource,
        etag: Option<String>,
    },
    CatalogUnchanged,
    CatalogFailed {
        offline: bool,
        /// The server says there's no catalog there at all, which is true until the first
        /// one is published.
        unpublished: bool,
        text: String,
    },
    Details {
        id: String,
        result: Result<Details, String>,
    },
    Image {
        id: String,
        index: usize,
        image: ColorImage,
    },
    OpProgress {
        id: u64,
        fraction: f32,
    },
    OpDone {
        id: u64,
        result: Result<OpOutcome, OpFailure>,
    },
    GameRunning(bool),
    UpdateCheck {
        manual: bool,
        result: Result<Option<Box<UpdateInfo>>, String>,
    },
    Dir(Box<DirListing>),
    Backups(Result<Vec<BackupEntry>, String>),
    LogLines(Vec<String>),
    SaveFailed(String),
    Relaunch(Result<(), String>),
}

/// What an op needs from the UI's state, cloned when it starts.
#[derive(Clone)]
pub struct OpEnv {
    pub install: Option<Install>,
    pub keep_previous: bool,
}

#[derive(Clone)]
pub struct Worker {
    tx: Sender<Event>,
    ctx: egui::Context,
    shared: Arc<Shared>,
}

struct Shared {
    http: OnceLock<Http>,
    /// Plain http to this machine is allowed. Test rigs only.
    allow_loopback: bool,
    demo: bool,
    sim: Duration,
    dirs: Dirs,
    catalog_url: String,
    saver: Saver,
}

impl Worker {
    pub fn new(
        ctx: egui::Context,
        tx: Sender<Event>,
        dirs: Dirs,
        demo: bool,
        allow_loopback: bool,
        catalog_url: String,
        sim: Duration,
    ) -> Self {
        Self {
            tx,
            ctx,
            shared: Arc::new(Shared {
                http: OnceLock::new(),
                allow_loopback,
                demo,
                sim,
                dirs,
                catalog_url,
                saver: Saver::default(),
            }),
        }
    }

    pub fn dirs(&self) -> &Dirs {
        &self.shared.dirs
    }

    /// Creating a client loads the system's certificates, so it waits until a worker
    /// actually needs one.
    fn http(&self) -> &Http {
        self.shared.http.get_or_init(|| {
            if self.shared.allow_loopback {
                Http::allowing_loopback()
            } else {
                Http::new()
            }
        })
    }

    pub fn send(&self, event: Event) {
        // The other end only goes away when the app is closing.
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }

    fn spawn(&self, name: &str, job: impl FnOnce(Worker) + Send + 'static) {
        let worker = self.clone();
        let spawned = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || job(worker));
        if let Err(e) = spawned {
            log::error!("Couldn't start a thread for {name}: {e}");
        }
    }

    /// Saved path first, else look around. `only` skips all of that: it's that folder or nothing.
    pub fn find_game(&self, only: Option<PathBuf>, saved: Option<String>) {
        self.spawn("find-game", move |w| {
            let found = match only {
                Some(path) => match detect::validate(&path) {
                    Ok(install) => Some(found_game(install, HowFound::CommandLine)),
                    Err(e) => {
                        log::warn!("--game: {e}");
                        None
                    }
                },
                None => look_for_game(saved),
            };
            w.send(Event::GameFound(found));
        });
    }

    /// Settle anything an earlier run left unfinished, then test the folder and scan it.
    pub fn open_game(&self, install: Install, kept: Vec<(String, PathBuf)>) {
        self.spawn("open-game", move |w| {
            if let Ok(layout) = Layout::from_addons_dir(&install.addons_dir) {
                let result = install::recover(&layout).map_err(|e| e.to_string());
                if let Err(e) = &result {
                    log::error!("Recovery isn't finished: {e}");
                }
                w.send(Event::Recovered(result));
            }
            w.send(Event::WriteTest(
                detect::check_writable(&install).err().map(|e| e.to_string()),
            ));
            w.scan_now(&install, &kept);
        });
    }

    pub fn scan(&self, install: Install, kept: Vec<(String, PathBuf)>) {
        self.spawn("scan", move |w| w.scan_now(&install, &kept));
    }

    fn scan_now(&self, install: &Install, kept: &[(String, PathBuf)]) {
        let (folders, error) = match scan::scan_addons(&install.addons_dir) {
            Ok(folders) => (folders, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let missing_zips = kept
            .iter()
            .filter(|(_, zip)| !zip.exists())
            .map(|(id, _)| id.clone())
            .collect();
        self.send(Event::Scanned {
            folders,
            missing_zips,
            error,
        });
    }

    /// The cached catalog first, so there's something to show, then a fresh one.
    pub fn load_catalog(&self, etag: Option<String>) {
        self.spawn("catalog", move |w| {
            let file = w.dirs().cache.join(CATALOG_FILE);
            let cached = read_catalog_file(&file);
            let have_cache = cached.is_some();
            if let Some(catalog) = cached {
                w.send(Event::Catalog {
                    bundle: Arc::new(CatalogBundle::new(catalog)),
                    source: CatalogSource::Cache,
                    etag: None,
                });
            }
            // An ETag with no file behind it would only earn a useless 304.
            let etag = etag.filter(|_| have_cache);
            let url = w.shared.catalog_url.clone();
            let event = match w
                .http()
                .get_if_modified(&url, etag.as_deref(), MAX_CATALOG_BYTES)
            {
                Ok(Fetched::NotModified) => Event::CatalogUnchanged,
                Ok(Fetched::Body { bytes, etag }) => match parse_catalog(&bytes) {
                    Ok(catalog) => {
                        let etag = save_catalog_file(&file, &bytes).then_some(etag).flatten();
                        Event::Catalog {
                            bundle: Arc::new(CatalogBundle::new(catalog)),
                            source: CatalogSource::Network,
                            etag,
                        }
                    }
                    Err(text) => Event::CatalogFailed {
                        offline: false,
                        unpublished: false,
                        text,
                    },
                },
                Err(e) => Event::CatalogFailed {
                    offline: matches!(e, Error::Offline { .. }),
                    unpublished: matches!(e, Error::HttpStatus { status: 404, .. }),
                    text: e.to_string(),
                },
            };
            w.send(event);
        });
    }

    pub fn fetch_details(&self, addon: feeling_core::catalog::Addon) {
        if self.shared.demo {
            self.send(Event::Details {
                result: Ok(demo::details(&addon.summary)),
                id: addon.id,
            });
            return;
        }
        self.spawn("details", move |w| {
            let result = details::fetch(w.http(), &addon).map_err(|e| e.to_string());
            w.send(Event::Details {
                id: addon.id,
                result,
            });
        });
    }

    /// Fetch and decode up to three screenshots. Anything that fails is just left out.
    pub fn fetch_images(&self, id: String, urls: Vec<String>) {
        if self.shared.demo {
            self.send(Event::Image {
                id,
                index: 0,
                image: demo::screenshot(),
            });
            return;
        }
        self.spawn("images", move |w| {
            for (index, url) in urls.iter().take(3).enumerate() {
                // These addresses were written by the addon's author, so they get the
                // stricter fetch. See `Http::get_untrusted`.
                let bytes = match w.http().get_untrusted(url, MAX_IMAGE_BYTES) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        log::info!("Left out a screenshot: {e}");
                        continue;
                    }
                };
                match decode_screenshot(&bytes) {
                    Some(image) => w.send(Event::Image {
                        id: id.clone(),
                        index,
                        image,
                    }),
                    None => log::info!("Left out a screenshot in a format we can't read"),
                }
            }
        });
    }

    pub fn check_game_running(&self) {
        if self.shared.demo {
            return;
        }
        self.spawn("game-check", |w| {
            w.send(Event::GameRunning(gameproc::is_game_running()));
        });
    }

    /// Look for an app update. A launch check waits a second first and stays quiet on failure.
    pub fn check_for_update(&self, version: &'static str, manual: bool) {
        if self.shared.demo {
            if manual {
                self.send(Event::UpdateCheck {
                    manual,
                    result: Ok(None),
                });
            }
            return;
        }
        self.spawn("update-check", move |w| {
            if !manual {
                thread::sleep(Duration::from_secs(1));
            }
            let result = selfupdate::check(w.http(), version)
                .map(|found| found.map(Box::new))
                .map_err(|e| e.to_string());
            if let Err(e) = &result {
                log::info!("Update check: {e}");
            }
            w.send(Event::UpdateCheck { manual, result });
        });
    }

    pub fn list_dir(&self, path: PathBuf) {
        if self.shared.demo {
            self.send(Event::Dir(Box::new(demo_listing(path))));
            return;
        }
        self.spawn("list-dir", move |w| {
            w.send(Event::Dir(Box::new(read_listing(path))));
        });
    }

    pub fn list_backups(&self) {
        if self.shared.demo {
            self.send(Event::Backups(Ok(demo::backups())));
            return;
        }
        self.spawn("list-backups", |w| {
            let result = backup::list_backups(&w.dirs().backups()).map_err(|e| e.to_string());
            w.send(Event::Backups(result));
        });
    }

    /// The last log lines, for "Copy diagnostics".
    pub fn read_log(&self) {
        if self.shared.demo {
            self.send(Event::LogLines(demo::log_lines()));
            return;
        }
        self.spawn("read-log", |w| {
            w.send(Event::LogLines(logging::recent_lines(50)));
        });
    }

    /// Write the state, newest wins. Saves that pile up while one's in progress collapse
    /// into the last of them.
    pub fn save_state(&self, state: State) {
        if self.shared.demo {
            return;
        }
        let saver = &self.shared.saver;
        let start_thread = {
            let mut slot = saver.lock();
            slot.latest = Some(Box::new(state));
            !std::mem::replace(&mut slot.writing, true)
        };
        if start_thread {
            self.spawn("save-state", |w| w.write_saves());
        }
    }

    fn write_saves(&self) {
        let saver = &self.shared.saver;
        loop {
            let next = {
                let mut slot = saver.lock();
                let next = slot.latest.take();
                if next.is_none() {
                    slot.writing = false;
                    saver.done.notify_all();
                }
                next
            };
            let Some(state) = next else {
                return;
            };
            if let Err(e) = state.save_to(&self.shared.dirs.state_file) {
                log::error!("Couldn't save settings: {e}");
                self.send(Event::SaveFailed(e.to_string()));
            }
        }
    }

    /// Wait for saves in progress. Only for shutting down, where blocking is fine.
    pub fn finish_saves(&self) {
        let saver = &self.shared.saver;
        let mut slot = saver.lock();
        while slot.writing {
            slot = saver
                .done
                .wait(slot)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Start one op on its own thread.
    pub fn run_op(&self, op: Op, env: OpEnv, cancel: Arc<AtomicBool>) {
        let id = op.id;
        let name = format!("op-{id}");
        self.spawn(&name, move |w| {
            let result = if w.shared.demo {
                w.simulate_op(&op, &cancel)
            } else {
                w.perform_op(&op, &env, &cancel)
            };
            if let Err(failure) = &result
                && !failure.cancelled
            {
                log::warn!("{} failed: {}", op.name, failure.text);
            }
            w.send(Event::OpDone { id, result });
        });
    }

    fn progress(&self, id: u64, fraction: f32) {
        self.send(Event::OpProgress { id, fraction });
    }

    fn perform_op(
        &self,
        op: &Op,
        env: &OpEnv,
        cancel: &AtomicBool,
    ) -> Result<OpOutcome, OpFailure> {
        match &op.kind {
            OpKind::Download {
                release,
                old_folders,
                ..
            } => {
                let (install, layout) = game_layout(env)?;
                detect::ensure_addons_dir(&install)?;
                let mut request = InstallRequest::for_release(&op.addon_id, release);
                request.old_folders = old_folders.clone();
                request.cache_dir = self.zip_cache(env);
                let size = request.size;
                let mut report = self.stage_reporter(op.id, size);
                let outcome =
                    install::install_from_url(self.http(), &layout, &request, &mut report, cancel)?;
                Ok(self.installed(op, outcome, None))
            }
            OpKind::Rollback {
                zip,
                version,
                old_folders,
                ..
            } => {
                let (install, layout) = game_layout(env)?;
                detect::ensure_addons_dir(&install)?;
                let request = InstallRequest {
                    addon_id: op.addon_id.clone(),
                    version: version.clone(),
                    old_folders: old_folders.clone(),
                    cache_dir: self.zip_cache(env),
                    ..InstallRequest::default()
                };
                let mut report = self.stage_reporter(op.id, 0);
                let outcome = install::install_from_zip(&layout, zip, &request, &mut report, cancel)?;
                Ok(self.installed(op, outcome, None))
            }
            OpKind::LocalZip { path } => {
                let (install, layout) = game_layout(env)?;
                detect::ensure_addons_dir(&install)?;
                let request = InstallRequest::default();
                let mut report = self.stage_reporter(op.id, 0);
                let outcome =
                    install::install_from_zip(&layout, path, &request, &mut report, cancel)?;
                let local = read_local_facts(&install.addons_dir, &outcome.folders);
                Ok(self.installed(op, outcome, local))
            }
            OpKind::Remove { folders } => {
                let (_, layout) = game_layout(env)?;
                install::remove(&layout, folders)?;
                Ok(OpOutcome::Removed)
            }
            OpKind::BackupWtf => {
                let (install, _) = game_layout(env)?;
                if !install.wtf_dir.exists() {
                    return Ok(OpOutcome::BackupSkipped);
                }
                let mut throttle = Throttle::default();
                let mut report = |progress: BackupProgress| {
                    if let BackupProgress::Writing { done, total } = progress
                        && throttle.ready()
                    {
                        self.progress(op.id, done as f32 / total.max(1) as f32);
                    }
                };
                backup::backup_wtf(
                    &install.wtf_dir,
                    &self.dirs().backups(),
                    KEEP_BACKUPS,
                    &mut report,
                    cancel,
                )?;
                Ok(OpOutcome::BackedUp)
            }
            OpKind::RestoreWtf { zip } => {
                let (install, _) = game_layout(env)?;
                let running = gameproc::is_game_running();
                let aside = backup::restore_wtf(zip, &install.wtf_dir, running)?;
                Ok(OpOutcome::Restored { aside })
            }
            OpKind::SelfUpdate { update } => {
                let mut throttle = Throttle::default();
                let size = update.asset.size;
                let mut report = |p: feeling_core::net::Progress| {
                    if throttle.ready() {
                        let total = p.total.unwrap_or(size).max(1);
                        self.progress(op.id, p.done as f32 / total as f32);
                    }
                };
                let applied = selfupdate::apply(self.http(), update, &mut report, cancel)?;
                Ok(OpOutcome::UpdateApplied(Box::new(applied)))
            }
        }
    }

    /// Fill the bar over a moment instead of touching anything. `--demo` only.
    fn simulate_op(&self, op: &Op, cancel: &AtomicBool) -> Result<OpOutcome, OpFailure> {
        const STEPS: u32 = 30;
        let step = self.shared.sim / STEPS;
        for i in 1..=STEPS {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled.into());
            }
            thread::sleep(step);
            self.progress(op.id, i as f32 / STEPS as f32);
        }
        let folders = match &op.kind {
            OpKind::Remove { .. }
            | OpKind::BackupWtf
            | OpKind::RestoreWtf { .. }
            | OpKind::SelfUpdate { .. } => {
                return Ok(match op.kind {
                    OpKind::Remove { .. } => OpOutcome::Removed,
                    OpKind::BackupWtf => OpOutcome::BackedUp,
                    OpKind::RestoreWtf { .. } => OpOutcome::Restored { aside: None },
                    _ => OpOutcome::BackupSkipped,
                });
            }
            OpKind::LocalZip { path } => vec![
                path.file_stem()
                    .map_or("Dropped".to_owned(), |s| s.to_string_lossy().into_owned()),
            ],
            OpKind::Download { .. } | OpKind::Rollback { .. } => {
                op.folders.iter().cloned().collect()
            }
        };
        let outcome = InstallOutcome {
            folders,
            ..InstallOutcome::default()
        };
        Ok(OpOutcome::Installed {
            local: None,
            outcome,
            kept: Vec::new(),
        })
    }

    fn zip_cache(&self, env: &OpEnv) -> Option<PathBuf> {
        env.keep_previous.then(|| self.dirs().cache.clone())
    }

    fn installed(
        &self,
        op: &Op,
        outcome: InstallOutcome,
        local: Option<LocalZipFacts>,
    ) -> OpOutcome {
        let kept = if op.addon_id.is_empty() {
            Vec::new()
        } else {
            install::kept_zips(&self.dirs().cache, &op.addon_id)
        };
        OpOutcome::Installed {
            outcome,
            kept,
            local,
        }
    }

    /// Turns stages into bar positions, sending at most about thirty a second. A change
    /// of stage always gets through, so the end of a job is never skipped.
    fn stage_reporter(&self, id: u64, size: u64) -> impl FnMut(Stage) + use<'_> {
        let mut throttle = Throttle::default();
        move |stage| {
            let moving = matches!(
                stage,
                Stage::Downloading { .. } | Stage::Extracting { .. }
            );
            if !moving || throttle.ready() {
                self.progress(id, stage_fraction(stage, size));
            }
        }
    }

    /// Save everything, let go of the lock and start the new version. The app exits next.
    pub fn relaunch(&self, applied: Applied, state: State, lock: Arc<Mutex<Option<File>>>) {
        self.spawn("relaunch", move |w| {
            w.finish_saves();
            if let Err(e) = state.save_to(&w.dirs().state_file) {
                log::warn!("Couldn't save settings before restarting: {e}");
            }
            let held = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(held);
            let result = selfupdate::relaunch(applied).map_err(|e| e.to_string());
            w.send(Event::Relaunch(result));
        });
    }
}

#[derive(Default)]
struct Saver {
    slot: Mutex<SaveSlot>,
    done: Condvar,
}

#[derive(Default)]
struct SaveSlot {
    latest: Option<Box<State>>,
    writing: bool,
}

impl Saver {
    fn lock(&self) -> std::sync::MutexGuard<'_, SaveSlot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn ready(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < PROGRESS_EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

fn game_layout(env: &OpEnv) -> Result<(Install, Layout), OpFailure> {
    let install = env.install.clone().ok_or_else(|| OpFailure {
        cancelled: false,
        text: "Choose your game folder first.".to_owned(),
    })?;
    let layout = Layout::from_addons_dir(&install.addons_dir)?;
    Ok((install, layout))
}

fn look_for_game(saved: Option<String>) -> Option<FoundGame> {
    if let Some(saved) = saved {
        match detect::validate(Path::new(&saved)) {
            Ok(install) => return Some(found_game(install, HowFound::Remembered)),
            Err(e) => log::info!("The saved game folder is no longer good: {e}"),
        }
    }
    detect::detect_all().into_iter().next().map(|install| {
        let how = HowFound::Detected(install.found_by.clone());
        found_game(install, how)
    })
}

pub fn found_game(install: Install, how: HowFound) -> FoundGame {
    let key = install.key();
    FoundGame {
        shown_path: shown_path(&install.root),
        install,
        how,
        key,
    }
}

/// The folder spelled the way the disk has it, without Windows's long-path prefix.
fn shown_path(path: &Path) -> String {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = real.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

fn parse_catalog(bytes: &[u8]) -> Result<Catalog, String> {
    let catalog: Catalog =
        serde_json::from_slice(bytes).map_err(|e| format!("The catalog is damaged: {e}"))?;
    if catalog.schema > CATALOG_SCHEMA {
        return Err("The catalog needs a newer version of this app.".to_owned());
    }
    Ok(catalog)
}

fn read_catalog_file(path: &Path) -> Option<Catalog> {
    let bytes = fs::read(path).ok()?;
    match parse_catalog(&bytes) {
        Ok(catalog) => Some(catalog),
        Err(e) => {
            log::warn!("Ignoring the cached catalog: {e}");
            None
        }
    }
}

/// The cache is only ever replaced whole. Returns whether it worked, because an ETag for
/// a file that didn't get saved would be a lie.
fn save_catalog_file(path: &Path, bytes: &[u8]) -> bool {
    let part = path.with_extension("json.part");
    let saved = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&part, bytes))
        .and_then(|()| fs::rename(&part, path));
    if let Err(e) = &saved {
        log::warn!("Couldn't cache the catalog: {e}");
        let _ = fs::remove_file(&part);
    }
    saved.is_ok()
}

/// Decode with limits, then scale down to the page's width. `None` for anything we can't
/// read, so a strange image is simply left out.
pub fn decode_screenshot(bytes: &[u8]) -> Option<ColorImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(u64::from(MAX_IMAGE_SIDE) * u64::from(MAX_IMAGE_SIDE) * 4);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let scaled = if decoded.width() > MAX_SHOWN_WIDTH {
        let height = u64::from(decoded.height()) * u64::from(MAX_SHOWN_WIDTH)
            / u64::from(decoded.width());
        decoded.thumbnail_exact(MAX_SHOWN_WIDTH, (height as u32).max(1))
    } else {
        decoded
    };
    let rgba = scaled.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// What the dropped zip's folders say about themselves, read back after the install.
fn read_local_facts(addons_dir: &Path, folders: &[String]) -> Option<LocalZipFacts> {
    let main = crate::ops::main_folder(folders)?.clone();
    let local = scan::scan_addons(addons_dir).unwrap_or_default();
    let found = local
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(&main))
        .map(|f| (f.toc.title.clone(), f.toc.version.clone()));
    let (title, version) = found.unwrap_or((None, None));
    Some(LocalZipFacts {
        title: title
            .map(|t| crate::model::clean_line(&t, 80))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| main.clone()),
        version: version.map(|v| crate::model::clean_line(&v, 40)),
        main_folder: main,
    })
}

/// A sorted list of what's in a folder, and whether it's a game. Runs on a worker because
/// any of this can stall on a sleeping disk.
pub fn read_listing(path: PathBuf) -> DirListing {
    let mut listing = DirListing {
        parent: path.parent().map(Path::to_path_buf),
        drives: drive_list(),
        ..DirListing::default()
    };
    match fs::read_dir(&path) {
        Ok(items) => {
            let mut folders: Vec<String> = items
                .flatten()
                .filter(|item| item.path().is_dir())
                .map(|item| item.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') && !name.starts_with('$'))
                .take(5000)
                .collect();
            folders.sort_by_cached_key(|name| name.to_lowercase());
            listing.folders = folders;
        }
        Err(e) => listing.error = Some(format!("Couldn't read this folder: {e}")),
    }
    listing.verdict = Some(
        detect::validate(&path)
            .map(|install| found_game(install, HowFound::Chosen))
            .map_err(|e| e.to_string()),
    );
    listing.path = path;
    listing
}

fn demo_listing(path: PathBuf) -> DirListing {
    let (install, _) = demo::install();
    let is_game = path == install.root || path == install.forever_dir;
    DirListing {
        parent: path.parent().map(Path::to_path_buf),
        folders: if is_game {
            vec!["_forever_".to_owned()]
        } else {
            vec![
                "Battle.net".to_owned(),
                "Games".to_owned(),
                "Program Files".to_owned(),
                "Program Files (x86)".to_owned(),
                "World of Warcraft".to_owned(),
            ]
        },
        drives: vec![PathBuf::from(r"C:\"), PathBuf::from(r"D:\")],
        verdict: Some(if is_game {
            Ok(FoundGame {
                shown_path: install.root.display().to_string(),
                install,
                how: HowFound::Chosen,
                key: "demo".to_owned(),
            })
        } else {
            Err("This folder has no _forever_ folder, so WoW Forever isn't installed here. \
                 Pick the World of Warcraft folder that holds _forever_."
                .to_owned())
        }),
        path,
        error: None,
    }
}

#[cfg(windows)]
fn drive_list() -> Vec<PathBuf> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    // SAFETY: takes no arguments and returns a plain bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| PathBuf::from(format!("{}:\\", char::from(b'A' + bit))))
        .collect()
}

#[cfg(not(windows))]
fn drive_list() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::new(width, height);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn screenshots_are_scaled_to_the_page() {
        let image = decode_screenshot(&png(1280, 720)).unwrap();
        assert_eq!(image.size, [640, 360]);
        let small = decode_screenshot(&png(100, 50)).unwrap();
        assert_eq!(small.size, [100, 50], "small ones stay as they are");
    }

    #[test]
    fn oversized_and_unreadable_images_are_left_out() {
        assert!(decode_screenshot(&png(4097, 10)).is_none());
        assert!(decode_screenshot(b"not an image at all").is_none());
        assert!(decode_screenshot(b"GIF89a....").is_none());
    }

    #[test]
    fn the_catalog_schema_is_respected() {
        assert!(parse_catalog(br#"{"schema":1,"generated":1,"addons":[]}"#).is_ok());
        assert!(parse_catalog(br#"{"schema":999,"generated":1,"addons":[]}"#).is_err());
        assert!(parse_catalog(b"<html>nope</html>").is_err());
    }

    #[test]
    fn the_catalog_file_round_trips_and_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("deep").join("catalog.json");
        assert!(read_catalog_file(&file).is_none());
        assert!(save_catalog_file(
            &file,
            br#"{"schema":1,"generated":5,"addons":[]}"#
        ));
        assert_eq!(read_catalog_file(&file).unwrap().generated, 5);
        assert!(!file.with_extension("json.part").exists());
    }

    #[test]
    fn a_folder_listing_says_what_is_wrong_with_it() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("Beta")).unwrap();
        fs::create_dir(temp.path().join("alpha")).unwrap();
        fs::create_dir(temp.path().join(".hidden")).unwrap();
        fs::write(temp.path().join("file.txt"), b"x").unwrap();
        let listing = read_listing(temp.path().to_path_buf());
        assert_eq!(listing.folders, vec!["alpha".to_owned(), "Beta".to_owned()]);
        assert!(matches!(listing.verdict, Some(Err(_))));
        assert!(listing.parent.is_some());
    }

    #[test]
    fn a_folder_listing_recognizes_a_game() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        fs::create_dir_all(&forever).unwrap();
        fs::write(forever.join(".flavor.info"), "Product Flavor!STRING:0\nwowf\n").unwrap();
        fs::write(forever.join("Wow.exe"), b"").unwrap();
        let listing = read_listing(temp.path().to_path_buf());
        let found = listing.verdict.unwrap().unwrap();
        assert_eq!(found.how, HowFound::Chosen);
        assert!(found.install.addons_dir.ends_with("AddOns"));
    }

    #[test]
    fn a_missing_folder_lists_nothing_and_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let listing = read_listing(temp.path().join("gone"));
        assert!(listing.folders.is_empty());
        assert!(listing.error.is_some());
    }

    #[test]
    fn the_progress_throttle_lets_the_first_through_and_holds_the_rest() {
        let mut throttle = Throttle::default();
        assert!(throttle.ready());
        assert!(!throttle.ready());
    }
}
