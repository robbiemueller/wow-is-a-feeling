//! The running app: its state, the events that change it, and one frame of work.

use crate::actions::Action;
use crate::args::{Launch, Scene};
use crate::cursor::Cursors;
use crate::details_cache::{DetailsCache, PAUSE};
use crate::dialogs::{FolderBrowser, Popup, RemovePlan};
use crate::easter::{self, Facts};
use crate::fun::Fun;
use crate::messages::{Kind, Messages};
use crate::model::{
    self, CatalogBundle, CatalogStatus, GameState, HeaderId, Row, Section, Selection, Tab,
};
use crate::ops::{self, Casting, Op, OpKind, Queue, Verb};
use crate::script::Script;
use crate::skin::Skin;
use crate::views::{self, settings::VERSION, toolbar};
use crate::worker::{
    CatalogSource, Dirs, Event, FoundGame, HowFound, LocalZipFacts, OpEnv, OpFailure, OpOutcome,
    Worker,
};
use crate::{demo, theme};
use eframe::egui::{
    self, Key, Modifiers, TextureHandle, TextureOptions, Ui, Vec2, ViewportCommand, vec2,
};
use feeling_core::backup::BackupEntry;
use feeling_core::catalog::{CATALOG_URL, Channel};
use feeling_core::install::InstallOutcome;
use feeling_core::scan::{self, AddonGroup, Adoption, LocalFolder};
use feeling_core::search::SortBy;
use feeling_core::state::{ManagedAddon, Origin, State};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOOT_LINE_FOR: Duration = Duration::from_secs(8);
const MIN_POINTS: Vec2 = vec2(820.0, 560.0);
/// How long closing the window waits for running operations to stop, in the worst case.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

const GAME_RUNNING: &str = "WoW is running. Type /reload in game to load updates. \
                            New addons appear after restarting the game.";

/// What the parchment is showing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PageKind {
    Addon(String),
    Other(String),
    Kit(usize),
    Gap(usize),
    NoGame,
    Empty,
    NoCatalog,
    Loading,
    Nothing,
}

/// What `main` already worked out before the window opened.
pub struct Startup {
    pub dirs: Dirs,
    pub state: State,
    pub lock: Option<File>,
    pub already_running: bool,
}

/// Numbers for `--frame-report`, kept where its thread can read them.
#[derive(Default, Clone)]
pub struct ReportMarks {
    pub first_frame_ms: Option<u128>,
    pub catalog_ms: Option<u128>,
    pub addons: usize,
    pub search_ms: f64,
    pub search_worst_ms: f64,
}

pub struct App {
    pub ctx: egui::Context,
    pub skin: Skin,
    pub launch: Launch,
    pub worker: Worker,
    rx: Receiver<Event>,
    pub demo: bool,
    pub started: Instant,
    pub state: State,
    pub lock: Arc<Mutex<Option<File>>>,

    pub game: Option<FoundGame>,
    pub game_state: GameState,
    pub write_problem: Option<String>,

    pub bundle: Option<Arc<CatalogBundle>>,
    pub catalog_status: CatalogStatus,

    pub folders: Vec<LocalFolder>,
    folder_lookup: HashMap<String, usize>,
    pub scanned: bool,
    scan_running: bool,
    scan_again: bool,
    /// What the fake AddOns folder holds, in `--demo`.
    demo_disk: Vec<LocalFolder>,

    pub groups: Vec<AddonGroup>,
    pub adoptions: Vec<Adoption>,
    pub updates: Vec<String>,

    pub tab: Tab,
    tab_chosen: bool,
    pub section: Section,
    pub query: String,
    pub sort: SortBy,
    toggled: HashSet<HeaderId>,
    pub rows: Vec<Row>,
    rows_dirty: bool,
    local_dirty: bool,
    sel_my: Option<Selection>,
    sel_browse: Option<Selection>,
    pub list_offset: f32,
    pub pending_scroll: Option<usize>,
    pub list_to_top: bool,
    pub page_to_top: bool,
    pub focus_search: bool,

    pub popup: Option<Popup>,
    popup_queue: VecDeque<Popup>,
    pub messages: Messages,
    pub loot: Option<(String, Instant)>,
    pub queue: Queue,
    demo_casting: Option<Casting>,
    cancelled_ops: usize,
    pub details: DetailsCache<TextureHandle>,
    pub fun: Fun,
    cursors: Cursors,
    pub channel_choice: HashMap<String, Channel>,
    pub changelog_open: bool,
    pub actions: Vec<Action>,
    /// What to carry on with once the player has chosen a game folder.
    pub after_folder: Vec<Action>,
    pub backups: Option<Vec<BackupEntry>>,
    pub list_text: String,
    diagnostics_wanted: bool,
    /// The slider's position, which only becomes the real scale when it's let go.
    pub scale_draft: f32,
    /// Frames left before checking that the window still fits the scale.
    fit_window_in: u8,
    window_rect: Option<(egui::Pos2, Vec2)>,
    relaunching: bool,
    shut_down: bool,

    pub frames: Arc<AtomicU64>,
    pub marks: Arc<Mutex<ReportMarks>>,
    script: Option<Script>,
    shot_frames: u32,
    shot_taken: bool,
}

impl App {
    pub fn new(ctx: &egui::Context, launch: Launch, startup: Startup) -> Self {
        theme::install(ctx);
        // The one scale setting is the slider, so egui's own shortcuts stay out of it.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let Startup {
            dirs,
            state,
            lock,
            already_running,
        } = startup;
        let scale = launch
            .scale
            .unwrap_or(state.settings.ui_scale)
            .clamp(0.8, 2.0);
        ctx.set_zoom_factor(scale);

        let (tx, rx) = mpsc::channel();
        let demo = launch.demo;
        let catalog_url = launch
            .catalog_url
            .clone()
            .filter(|_| launch.test_rig)
            .unwrap_or_else(|| CATALOG_URL.to_owned());
        let sim = Duration::from_millis(launch.sim_ms.unwrap_or(1200));
        let worker = Worker::new(
            ctx.clone(),
            tx,
            dirs,
            demo,
            launch.test_rig,
            catalog_url,
            sim,
        );
        let now = Instant::now();
        let script = launch.script.as_deref().and_then(Script::load);
        let fun = Fun::new(state.stats.launches, now);
        let pause = if demo { Duration::ZERO } else { PAUSE };
        let mut app = Self {
            ctx: ctx.clone(),
            skin: Skin::load(ctx),
            worker,
            rx,
            demo,
            started: now,
            state,
            lock: Arc::new(Mutex::new(lock)),
            game: None,
            game_state: GameState::Searching,
            write_problem: None,
            bundle: None,
            catalog_status: CatalogStatus::Loading,
            folders: Vec::new(),
            folder_lookup: HashMap::new(),
            scanned: false,
            scan_running: false,
            scan_again: false,
            demo_disk: Vec::new(),
            groups: Vec::new(),
            adoptions: Vec::new(),
            updates: Vec::new(),
            tab: Tab::Browse,
            tab_chosen: false,
            section: Section::Game,
            query: String::new(),
            sort: SortBy::Popular,
            toggled: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            local_dirty: true,
            sel_my: None,
            sel_browse: None,
            list_offset: 0.0,
            pending_scroll: None,
            list_to_top: false,
            page_to_top: false,
            focus_search: false,
            popup: None,
            popup_queue: VecDeque::new(),
            messages: Messages::default(),
            loot: None,
            queue: Queue::default(),
            demo_casting: None,
            cancelled_ops: 0,
            details: DetailsCache::new(pause),
            fun,
            cursors: Cursors::default(),
            channel_choice: HashMap::new(),
            changelog_open: false,
            actions: Vec::new(),
            after_folder: Vec::new(),
            backups: None,
            list_text: String::new(),
            diagnostics_wanted: false,
            scale_draft: scale,
            fit_window_in: 2,
            window_rect: None,
            relaunching: false,
            shut_down: false,
            frames: Arc::new(AtomicU64::new(0)),
            marks: Arc::new(Mutex::new(ReportMarks::default())),
            script,
            shot_frames: 0,
            shot_taken: false,
            launch,
        };
        if already_running {
            app.popup = Some(Popup::AlreadyRunning);
            app.game_state = GameState::NotFound;
            return app;
        }
        if app.managed_map().is_some_and(|m| !m.is_empty()) {
            app.tab = Tab::MyAddons;
        }
        if demo {
            app.start_demo();
        } else {
            app.start_real();
        }
        app.apply_launch_flags();
        app.start_frame_report();
        app
    }

    fn start_real(&mut self) {
        let saved = self.state.current_install.clone();
        self.worker.find_game(self.launch.game.clone(), saved);
        self.worker.load_catalog(self.state.catalog_etag.clone());
        if self.state.settings.check_app_updates {
            self.worker.check_for_update(VERSION, false);
        }
    }

    fn start_demo(&mut self) {
        let scene = self.launch.scene;
        self.demo_disk = demo::disk(&self.state, scene);
        match scene {
            Scene::Loading => {}
            Scene::NoGame => self.game_state = GameState::NotFound,
            _ => {
                let (install, key) = demo::install();
                let shown_path = install.root.display().to_string();
                self.game_state = GameState::Found {
                    version: install.version.clone(),
                };
                self.game = Some(FoundGame {
                    how: HowFound::Detected(install.found_by.clone()),
                    install,
                    key,
                    shown_path,
                });
                self.folders = self.demo_disk.clone();
                self.index_folders();
                self.scanned = true;
                self.backups = Some(demo::backups());
                if self.demo_has_anything() {
                    self.tab = Tab::MyAddons;
                }
            }
        }
        match scene {
            Scene::NoCatalog => self.catalog_status = CatalogStatus::Missing { offline: false },
            Scene::NoCatalogOffline => {
                self.catalog_status = CatalogStatus::Missing { offline: true }
            }
            Scene::Loading => {}
            _ => {
                self.bundle = Some(Arc::new(CatalogBundle::new(demo::catalog())));
                self.catalog_status = if scene == Scene::Offline {
                    CatalogStatus::Offline
                } else {
                    CatalogStatus::Fresh
                };
            }
        }
    }

    fn demo_has_anything(&self) -> bool {
        !self.demo_disk.is_empty()
    }

    fn apply_launch_flags(&mut self) {
        let now = Instant::now();
        if let Some(tab) = self.launch.tab {
            self.tab = tab;
            self.tab_chosen = true;
        }
        if let Some(section) = self.launch.section {
            self.section = section;
        }
        if let Some(search) = self.launch.search.clone() {
            self.query = search;
        }
        if let Some(sort) = self.launch.sort {
            self.sort = sort;
        }
        for category in self.launch.open.clone() {
            if let Some(i) = feeling_core::catalog::CATEGORIES
                .iter()
                .position(|(id, _)| *id == category)
            {
                self.toggled.insert(HeaderId::Category(i));
            }
        }
        self.changelog_open = self.launch.changelog;
        self.refresh();
        if let Some(select) = self.launch.select.clone() {
            let selection = parse_selection(&select);
            self.set_selection(Some(selection));
        }
        self.refresh();
        if self.launch.changelog {
            self.changelog_open = true;
        }

        for text in self.launch.red.clone() {
            self.messages.push(Kind::Error, text, now);
        }
        for text in self.launch.yellow.clone() {
            self.messages.push(Kind::System, text, now);
        }
        if let Some(line) = self.launch.loot.clone() {
            self.loot = Some((line, now + LOOT_LINE_FOR));
        }
        if let Some(fraction) = self.launch.progress {
            let text = if self.launch.batch {
                "Updating 3 of 7".to_owned()
            } else {
                "Installing Questie".to_owned()
            };
            self.demo_casting = Some(Casting { text, fraction });
        }
        if let Some(egg) = self.launch.egg.clone() {
            self.apply_egg(&egg, now);
        }
        if let Some(popup) = self.launch.popup.clone() {
            self.apply_popup_flag(&popup);
        }
    }

    fn apply_egg(&mut self, egg: &str, now: Instant) {
        match egg {
            "dance" => self.fun.dance(now),
            "ding" => {
                self.fun.ding(now);
                self.fun.freeze_for_screenshot(now);
                self.yellow("Ding!");
            }
            "pwnage" => {
                self.fun.pwnage(now);
                self.pwnage_message(now);
            }
            "monitor" => {
                for i in 0..5 {
                    self.fun.gag.click(now + Duration::from_millis(i));
                }
                self.yellow(easter::CLEANING_MESSAGE);
            }
            "platypus" => self.fun.platypus = true,
            "mom" => self.yellow(easter::MOM_MESSAGE),
            _ => {}
        }
    }

    fn apply_popup_flag(&mut self, name: &str) {
        match name {
            "running" => self.popup = Some(Popup::AlreadyRunning),
            "update" => self.popup = Some(Popup::AppUpdate(Box::new(demo::update_info()))),
            "restore" => {
                self.popup = Some(Popup::Restore {
                    zip: PathBuf::from("demo.zip"),
                    when: "Nov 5, 2026".to_owned(),
                });
            }
            "remove" => {
                let selection = self.selection().cloned().unwrap_or_else(|| {
                    Selection::Addon("gh:Questie/Questie".to_owned())
                });
                self.ask_remove(&selection);
            }
            "folder" => self.open_folder_browser(),
            "list" => {
                self.list_text = "# WoW is a Feeling addon list\ngh:Questie/Questie\n\
                                  gh:Nevcairiel/GatherMate2\ngh:tukui-org/ElvUI @beta\n\
                                  wowi:99999\nsomething odd\n"
                    .to_owned();
                self.actions.push(Action::PreviewList);
            }
            _ => {}
        }
    }

    /// Everything `--frame-report` needs happens on its own thread, because a timer in the
    /// UI thread would itself cause the frames being counted.
    fn start_frame_report(&self) {
        let Some(path) = self.launch.frame_report.clone() else {
            return;
        };
        let frames = self.frames.clone();
        let marks = self.marks.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            let before = frames.load(Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(5));
            let idle = frames.load(Ordering::Relaxed) - before;
            let marks = marks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let report = format!(
                "{{\"idle_frames_5s\":{idle},\"frames_before_idle\":{before},\
                 \"first_frame_ms\":{},\"catalog_ms\":{},\"addons\":{},\
                 \"search_avg_ms\":{:.3},\"search_worst_ms\":{:.3}}}\n",
                marks.first_frame_ms.unwrap_or(0),
                marks.catalog_ms.unwrap_or(0),
                marks.addons,
                marks.search_ms,
                marks.search_worst_ms,
            );
            let _ = std::fs::write(path, report);
            ctx.send_viewport_cmd(ViewportCommand::Close);
            ctx.request_repaint();
        });
    }

    fn measure_search(&self, bundle: &CatalogBundle) {
        if self.launch.frame_report.is_none() {
            return;
        }
        let queries = ["q", "qu", "que", "ques", "questi", "deadly boss", "map", "a", "zz", "addon 12"];
        let mut total = 0.0;
        let mut worst: f64 = 0.0;
        for query in queries {
            let start = Instant::now();
            let found = bundle.index.query(query, 500);
            let took = start.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(found);
            total += took;
            worst = worst.max(took);
        }
        let mut marks = self
            .marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        marks.catalog_ms = Some(self.started.elapsed().as_millis());
        marks.addons = bundle.catalog.addons.len();
        marks.search_ms = total / queries.len() as f64;
        marks.search_worst_ms = worst;
    }

    // --- Lookups the views share -------------------------------------------------------

    pub fn managed_map(&self) -> Option<&BTreeMap<String, ManagedAddon>> {
        self.state.current().map(|record| &record.managed)
    }

    pub fn managed(&self, id: &str) -> Option<&ManagedAddon> {
        self.managed_map()?.get(id)
    }

    pub fn selection(&self) -> Option<&Selection> {
        match self.tab {
            Tab::MyAddons => self.sel_my.as_ref(),
            Tab::Browse => self.sel_browse.as_ref(),
            Tab::Settings => None,
        }
    }

    pub fn group(&self, primary: &str) -> Option<&AddonGroup> {
        self.groups.iter().find(|g| g.primary == primary)
    }

    pub fn adoption_for(&self, addon_id: &str) -> Option<&Adoption> {
        self.adoptions.iter().find(|a| a.addon_id == addon_id)
    }

    /// The catalog match for an unmanaged group, by any of its folders.
    pub fn adoption_for_folder(&self, primary: &str) -> Option<&Adoption> {
        let group = self.group(primary)?;
        self.adoptions.iter().find(|adoption| {
            adoption
                .folders
                .iter()
                .any(|f| group.folders.iter().any(|g| g.eq_ignore_ascii_case(f)))
        })
    }

    pub fn toc_notes(&self, folder: &str) -> Option<String> {
        let index = *self.folder_lookup.get(&folder.to_lowercase())?;
        self.folders.get(index)?.toc.notes.clone()
    }

    /// Does anything of this addon's on disk say it runs on Forever? An addon whose folders
    /// haven't been seen gets the benefit of the doubt.
    pub fn targets_forever(&self, record: &ManagedAddon) -> bool {
        let mut seen = false;
        for folder in &record.folders {
            if let Some(&i) = self.folder_lookup.get(&folder.to_lowercase()) {
                seen = true;
                if self.folders[i].targets_forever {
                    return true;
                }
            }
        }
        !seen
    }

    pub fn is_selected_row(&self, row: &Row) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };
        match (row, selection) {
            (Row::Addon(i), Selection::Addon(id)) => self
                .bundle
                .as_deref()
                .is_some_and(|b| b.position(id) == Some(*i)),
            (Row::Managed(a), Selection::Addon(b)) => a == b,
            (Row::Other(i), Selection::Other(primary)) => {
                self.groups.get(*i as usize).is_some_and(|g| g.primary == *primary)
            }
            (Row::Kit(a), Selection::Kit(b)) | (Row::Gap(a), Selection::Gap(b)) => {
                *a as usize == *b
            }
            _ => false,
        }
    }

    /// True when there are no headers to sit under.
    pub fn rows_are_flat(&self) -> bool {
        !matches!(self.rows.first(), Some(Row::Header { .. }))
    }

    pub fn casting(&self) -> Option<Casting> {
        self.demo_casting.clone().or_else(|| self.queue.casting())
    }

    pub fn now_unix(&self) -> i64 {
        if self.demo {
            demo::NOW
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        }
    }

    pub fn status_text(&self) -> String {
        let bundle = self.bundle.as_deref();
        model::status_line(
            &self.game_state,
            bundle.map_or(0, |b| b.catalog.addons.len()),
            bundle.map_or(0, |b| b.catalog.generated),
            self.catalog_status,
            self.now_unix(),
        )
    }

    pub fn empty_list_note(&self) -> &'static str {
        if !model::effective_query(&self.query).is_empty() {
            return "Nothing matches that.";
        }
        match self.tab {
            Tab::Browse => match self.catalog_status {
                CatalogStatus::Loading => "Fetching the list of addons...",
                _ => "No addons to show.",
            },
            Tab::MyAddons => match self.game_state {
                GameState::Searching => "Looking for WoW Forever...",
                GameState::NotFound => "Choose your game folder to see your addons.",
                GameState::Found { .. } if !self.scanned => "Reading your AddOns folder...",
                GameState::Found { .. } => "Nothing here yet.",
            },
            Tab::Settings => "",
        }
    }

    pub fn page_kind(&self) -> PageKind {
        let selected = |selection: &Option<Selection>| match selection {
            Some(Selection::Addon(id)) => PageKind::Addon(id.clone()),
            Some(Selection::Other(primary)) => PageKind::Other(primary.clone()),
            Some(Selection::Kit(i)) => PageKind::Kit(*i),
            Some(Selection::Gap(i)) => PageKind::Gap(*i),
            None => PageKind::Nothing,
        };
        match self.tab {
            Tab::MyAddons => match self.game_state {
                GameState::NotFound => PageKind::NoGame,
                GameState::Searching => PageKind::Loading,
                GameState::Found { .. } => {
                    let nothing_installed = self.managed_map().is_none_or(BTreeMap::is_empty)
                        && self.groups.is_empty();
                    if self.scanned && nothing_installed && self.query.trim().is_empty() {
                        PageKind::Empty
                    } else {
                        selected(&self.sel_my)
                    }
                }
            },
            Tab::Browse => {
                if self.bundle.is_none() {
                    match self.catalog_status {
                        CatalogStatus::Loading => PageKind::Loading,
                        _ => PageKind::NoCatalog,
                    }
                } else {
                    selected(&self.sel_browse)
                }
            }
            Tab::Settings => PageKind::Nothing,
        }
    }

    pub fn main_action_for(&self, id: &str, main: model::MainButton) -> Action {
        match main {
            model::MainButton::Manage => Action::Adopt(id.to_owned()),
            _ => Action::Install(vec![id.to_owned()]),
        }
    }

    /// What Enter does: the page's main button, when it's enabled.
    fn enter_action(&self) -> Option<Action> {
        let busy_all = self.queue.busy_exclusive();
        match self.page_kind() {
            PageKind::Addon(id) => {
                let bundle = self.bundle.as_deref()?;
                let addon = bundle.addon(&id)?;
                let record = self.managed(&id);
                let main =
                    model::main_button(addon, record, self.adoption_for(&id).is_some());
                let channel = model::channel_for(
                    record,
                    self.channel_choice.get(&id).copied(),
                    self.state.settings.default_channel,
                );
                let installable =
                    addon.release(channel).is_some() || main == model::MainButton::Manage;
                let blocked = busy_all
                    || self.queue.busy_with(&id)
                    || main == model::MainButton::Pinned
                    || !installable;
                (!blocked).then(|| self.main_action_for(&id, main))
            }
            PageKind::Other(primary) => {
                let adoption = self.adoption_for_folder(&primary)?;
                (!busy_all).then(|| Action::Adopt(adoption.addon_id.clone()))
            }
            PageKind::Kit(i) => {
                let bundle = self.bundle.as_deref()?;
                let kit = bundle.catalog.kits.get(i)?;
                let missing = model::kit_missing(kit, bundle, self.managed_map());
                (!missing.is_empty() && !busy_all).then_some(Action::Install(missing))
            }
            PageKind::Gap(i) => {
                let bundle = self.bundle.as_deref()?;
                let url = bundle.catalog.gaps.get(i)?.website.as_deref()?;
                model::safe_url(url).map(|url| Action::OpenLink(url.to_owned()))
            }
            PageKind::NoGame => Some(Action::ChooseFolder),
            PageKind::Empty => Some(Action::SetTab(Tab::Browse)),
            PageKind::NoCatalog => Some(Action::RetryCatalog),
            PageKind::Loading | PageKind::Nothing => None,
        }
    }

    // --- Changing state ----------------------------------------------------------------

    pub fn yellow(&mut self, text: impl Into<String>) {
        self.messages.push(Kind::System, text, Instant::now());
    }

    pub fn red(&mut self, text: impl Into<String>) {
        self.messages.push(Kind::Error, text, Instant::now());
    }

    pub fn pwnage_message(&mut self, now: Instant) {
        self.messages.push_link(
            Kind::System,
            "PWNAGE!",
            "Watch the video",
            easter::VIDEO_URL,
            now,
        );
    }

    pub fn save_state(&self) {
        self.worker.save_state(self.state.clone());
    }

    /// The list needs another look, without touching what's derived from the disk.
    pub fn mark_rows_dirty(&mut self) {
        self.rows_dirty = true;
    }

    /// Something the lists are derived from has changed.
    pub fn mark_local_dirty(&mut self) {
        self.local_dirty = true;
    }

    pub fn toggle_header(&mut self, id: HeaderId) {
        if !self.toggled.remove(&id) {
            self.toggled.insert(id);
        }
        self.rows_dirty = true;
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.rows_dirty = true;
            self.page_to_top = true;
            self.list_offset = 0.0;
        }
        self.tab_chosen = true;
    }

    pub fn set_selection(&mut self, selection: Option<Selection>) {
        let slot = match self.tab {
            Tab::MyAddons => &mut self.sel_my,
            Tab::Browse => &mut self.sel_browse,
            Tab::Settings => return,
        };
        if *slot == selection {
            return;
        }
        *slot = selection;
        self.selection_changed();
    }

    /// The page shows something else now, so its scroll, its changelog and its pending
    /// fetch all start over.
    fn selection_changed(&mut self) {
        self.page_to_top = true;
        self.changelog_open = false;
        let now = Instant::now();
        let id = match self.selection() {
            Some(Selection::Addon(id)) => Some(id.clone()),
            _ => None,
        };
        let in_catalog = id
            .as_deref()
            .is_some_and(|id| self.bundle.as_deref().is_some_and(|b| b.addon(id).is_some()));
        match id {
            Some(id) if in_catalog => {
                if let Some(urls) = self.details.select(&id, now) {
                    self.worker.fetch_images(id, urls);
                }
            }
            _ => self.details.clear_pending(),
        }
    }

    pub fn open_folder_browser(&mut self) {
        let start = self
            .game
            .as_ref()
            .map(|game| game.install.root.clone())
            .unwrap_or_else(default_browse_folder);
        self.popup = Some(Popup::Folder(Box::new(FolderBrowser {
            typed: start.display().to_string(),
            listing: None,
            waiting_for: Some(start.clone()),
        })));
        self.worker.list_dir(start);
    }

    pub fn ask_remove(&mut self, selection: &Selection) {
        let plan = match selection {
            Selection::Addon(id) => self.managed(id).map(|record| {
                let folders = self
                    .managed_map()
                    .map(|m| ops::own_folders(id, m))
                    .unwrap_or_default();
                RemovePlan {
                    addon_id: Some(id.clone()),
                    name: record.name.clone(),
                    dependents: feeling_core::deps::dependents(
                        &folders,
                        &self.state,
                        &self.folders,
                    ),
                    folders,
                }
            }),
            Selection::Other(primary) => self.group(primary).map(|group| RemovePlan {
                addon_id: None,
                name: model::clean_line(&group.title, 80),
                folders: group.folders.clone(),
                dependents: feeling_core::deps::dependents(
                    &group.folders,
                    &self.state,
                    &self.folders,
                ),
            }),
            _ => None,
        };
        if let Some(plan) = plan {
            self.show_popup(Popup::Remove(plan));
        }
    }

    /// One popup at a time: the rest wait their turn.
    pub fn show_popup(&mut self, popup: Popup) {
        if self.popup.is_none() {
            self.popup = Some(popup);
        } else {
            self.popup_queue.push_back(popup);
        }
    }

    pub fn next_popup(&mut self) -> Option<Popup> {
        self.popup_queue.pop_front()
    }

    pub fn op_env(&self) -> OpEnv {
        OpEnv {
            install: self.game.as_ref().map(|game| game.install.clone()),
            keep_previous: self.state.settings.keep_previous_version,
        }
    }

    pub fn pump_queue(&mut self) {
        let started = self.queue.start_ready();
        if started.is_empty() {
            return;
        }
        let env = self.op_env();
        for (op, cancel) in started {
            self.worker.run_op(op, env.clone(), cancel);
        }
    }

    pub fn rescan(&mut self) {
        if self.demo {
            self.folders = self.demo_disk.clone();
            self.index_folders();
            self.scanned = true;
            self.local_dirty = true;
            return;
        }
        let Some(game) = &self.game else {
            return;
        };
        if self.scan_running {
            self.scan_again = true;
            return;
        }
        self.scan_running = true;
        let install = game.install.clone();
        self.worker.scan(install, self.kept_zip_paths());
    }

    fn kept_zip_paths(&self) -> Vec<(String, PathBuf)> {
        self.managed_map()
            .map(|managed| {
                managed
                    .values()
                    .filter_map(|m| Some((m.id.clone(), m.previous_zip.clone()?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn index_folders(&mut self) {
        self.folder_lookup = self
            .folders
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.to_lowercase(), i))
            .collect();
    }

    pub fn use_game(&mut self, game: FoundGame) {
        self.state.current_install = Some(game.key.clone());
        self.state.installs.entry(game.key.clone()).or_default();
        self.game_state = GameState::Found {
            version: game.install.version.clone(),
        };
        self.write_problem = None;
        self.scanned = false;
        if self.demo {
            self.rescan();
        } else {
            self.scan_running = true;
            self.worker
                .open_game(game.install.clone(), self.kept_zip_paths());
        }
        self.game = Some(game);
        self.local_dirty = true;
        self.save_state();
        for action in std::mem::take(&mut self.after_folder) {
            self.actions.push(action);
        }
    }

    pub fn is_settled(&self) -> bool {
        let looked = !matches!(self.game_state, GameState::Searching);
        let scanned = self.game.is_none() || (self.scanned && !self.scan_running);
        looked
            && scanned
            && self.catalog_status != CatalogStatus::Loading
            && self.queue.is_idle()
            && self.popup.is_none()
    }

    /// Enqueue an op from outside `actions.rs`: a dropped zip or a batch's backup.
    pub fn next_op_id(&mut self) -> u64 {
        self.queue.next_id()
    }

    pub fn run_slash(&mut self, line: &str) {
        let now = Instant::now();
        let managed = self.managed_map().map_or(0, BTreeMap::len);
        let stats = &self.state.stats;
        let facts = Facts {
            seconds_open: stats.seconds_open + self.started.elapsed().as_secs(),
            launches: stats.launches,
            addons_installed: stats.addons_installed,
            updates_applied: stats.updates_applied,
            managed,
        };
        match easter::parse_slash(line, facts) {
            Some(easter::Slash::Say(text)) => self.yellow(text),
            Some(easter::Slash::Reload) => {
                self.rescan();
                self.yellow("Reloading your addons.");
            }
            Some(easter::Slash::Dance) => self.fun.dance(now),
            Some(easter::Slash::Ding) => {
                self.fun.ding(now);
                self.yellow("Ding!");
            }
            Some(easter::Slash::Pwnage) => {
                self.fun.pwnage(now);
                self.pwnage_message(now);
            }
            Some(easter::Slash::OpenAbout) => {
                self.set_tab(Tab::Settings);
                self.section = Section::About;
            }
            None => {}
        }
    }

    // --- Events from workers -----------------------------------------------------------

    pub fn logic(&mut self) {
        let now = Instant::now();
        while let Ok(event) = self.rx.try_recv() {
            self.handle(event, now);
        }
        self.pump_queue();
        if let Some(id) = self.details.take_due(now) {
            let addon = self.bundle.as_deref().and_then(|b| b.addon(&id).cloned());
            if let Some(addon) = addon {
                self.worker.fetch_details(addon);
            }
        }
        if self.fun.mom_calling(now) {
            self.yellow(easter::MOM_MESSAGE);
        }
        self.messages.tick(now);
        if self.loot.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.loot = None;
        }
        self.script_tick(now);
        self.refresh();
    }

    fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::GameFound(Some(game)) => self.use_game(game),
            Event::GameFound(None) => {
                self.game = None;
                self.game_state = GameState::NotFound;
                self.scanned = false;
                self.local_dirty = true;
                if !self.tab_chosen {
                    self.tab = Tab::Browse;
                    self.yellow(
                        "Couldn't find WoW Forever. Open My Addons to choose your game folder.",
                    );
                }
            }
            Event::Recovered(Ok(lines)) => {
                for line in lines {
                    self.yellow(line);
                }
            }
            Event::Recovered(Err(text)) => self.red(text),
            Event::WriteTest(problem) => {
                if let Some(problem) = &problem {
                    self.red(problem.clone());
                }
                self.write_problem = problem;
            }
            Event::Scanned {
                folders,
                missing_zips,
                error,
            } => self.scanned_folders(folders, missing_zips, error),
            Event::Catalog {
                bundle,
                source,
                etag,
            } => {
                self.measure_search(&bundle);
                self.bundle = Some(bundle);
                self.local_dirty = true;
                match source {
                    CatalogSource::Cache => self.catalog_status = CatalogStatus::Cached,
                    CatalogSource::Network => {
                        self.catalog_status = CatalogStatus::Fresh;
                        self.state.catalog_etag = etag;
                        self.save_state();
                    }
                }
                self.selection_changed();
            }
            Event::CatalogUnchanged => self.catalog_status = CatalogStatus::Fresh,
            Event::CatalogFailed {
                offline,
                unpublished,
                text,
            } => {
                if unpublished {
                    log::info!("No catalog is published yet: {text}");
                } else {
                    log::warn!("Couldn't fetch the catalog: {text}");
                }
                self.catalog_status = match (self.bundle.is_some(), offline) {
                    (true, true) => CatalogStatus::Offline,
                    (true, false) => CatalogStatus::Cached,
                    (false, offline) => CatalogStatus::Missing { offline },
                };
                self.rows_dirty = true;
            }
            Event::Details { id, result } => {
                if let Some(urls) = self.details.store(&id, result, now) {
                    self.worker.fetch_images(id, urls);
                }
            }
            Event::Image { id, index, image } => {
                let name = format!("shot-{id}-{index}");
                let texture = self.ctx.load_texture(name, image, TextureOptions::LINEAR);
                self.details.store_texture(&id, texture);
            }
            Event::OpProgress { id, fraction } => self.queue.set_progress(id, fraction),
            Event::OpDone { id, result } => self.op_done(id, result, now),
            Event::GameRunning(running) => {
                if running {
                    self.yellow(GAME_RUNNING);
                }
            }
            Event::UpdateCheck { manual, result } => self.update_checked(manual, result),
            Event::Dir(listing) => {
                if let Some(Popup::Folder(browser)) = &mut self.popup
                    && browser.waiting_for.as_ref() == Some(&listing.path)
                {
                    browser.waiting_for = None;
                    browser.typed = listing.path.display().to_string();
                    browser.listing = Some(*listing);
                }
            }
            Event::Backups(Ok(list)) => self.backups = Some(list),
            Event::Backups(Err(text)) => {
                self.backups = Some(Vec::new());
                self.red(text);
            }
            Event::LogLines(lines) => {
                if std::mem::take(&mut self.diagnostics_wanted) {
                    let text = self.diagnostics_text(&lines);
                    self.ctx.copy_text(text);
                    self.yellow("Copied the diagnostics to the clipboard.");
                }
            }
            Event::SaveFailed(text) => self.red(text),
            Event::Relaunch(Ok(())) => {
                self.relaunching = true;
                self.ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Event::Relaunch(Err(text)) => {
                self.red(text);
                // The new version didn't start, so this one carries on holding the lock.
                let lock = crate::take_lock(&self.worker.dirs().state_file);
                *self
                    .lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = lock;
            }
        }
    }

    fn scanned_folders(
        &mut self,
        folders: Vec<LocalFolder>,
        missing_zips: Vec<String>,
        error: Option<String>,
    ) {
        self.scan_running = false;
        self.scanned = true;
        if let Some(error) = error {
            self.red(error);
        }
        self.folders = folders;
        self.index_folders();
        if let Some(record) = self.state.current_mut() {
            let mut changed = false;
            for id in missing_zips {
                if let Some(addon) = record.managed.get_mut(&id) {
                    addon.previous_zip = None;
                    addon.previous_version = None;
                    changed = true;
                }
            }
            if changed {
                self.save_state();
            }
        }
        self.local_dirty = true;
        if !self.tab_chosen {
            let anything =
                self.managed_map().is_some_and(|m| !m.is_empty()) || !self.folders.is_empty();
            let tab = if anything { Tab::MyAddons } else { Tab::Browse };
            if self.tab != tab {
                self.tab = tab;
                self.rows_dirty = true;
            }
        }
        if std::mem::take(&mut self.scan_again) {
            self.rescan();
        }
    }

    fn op_done(&mut self, id: u64, result: Result<OpOutcome, OpFailure>, now: Instant) {
        let Some(op) = self.queue.finish(id) else {
            return;
        };
        match result {
            Ok(OpOutcome::Installed {
                outcome,
                kept,
                local,
            }) => self.op_installed(&op, outcome, &kept, local),
            Ok(OpOutcome::Removed) => self.op_removed(&op),
            Ok(OpOutcome::BackedUp) => {
                let unix = self.now_unix();
                if let Some(record) = self.state.current_mut() {
                    record.last_wtf_backup = unix;
                }
                self.save_state();
                if self.tab == Tab::Settings && self.section == Section::Backups {
                    self.worker.list_backups();
                }
            }
            Ok(OpOutcome::BackupSkipped) => {}
            Ok(OpOutcome::Restored { aside }) => match aside {
                Some(aside) => self.yellow(format!(
                    "Settings restored. Your old ones are in {}.",
                    aside.display()
                )),
                None => self.yellow("Settings restored."),
            },
            Ok(OpOutcome::UpdateApplied(applied)) => {
                self.yellow("Update ready. Restarting...");
                self.record_session();
                self.worker
                    .relaunch(*applied, self.state.clone(), self.lock.clone());
            }
            Err(failure) if failure.cancelled => self.cancelled_ops += 1,
            Err(failure) => self.red(failure.text),
        }
        if self.queue.is_idle() && self.cancelled_ops > 0 {
            self.cancelled_ops = 0;
            self.yellow("Cancelled.");
        }
        let _ = now;
    }

    fn op_installed(
        &mut self,
        op: &Op,
        outcome: InstallOutcome,
        kept: &[PathBuf],
        local: Option<LocalZipFacts>,
    ) {
        let now = self.now_unix();
        for warning in &outcome.warnings {
            self.yellow(warning.clone());
        }
        if !outcome.skipped_files.is_empty() {
            log::info!("Left out of {}: {:?}", op.name, outcome.skipped_files);
        }
        let keep_previous = self.state.settings.keep_previous_version;
        let (id, name, version, channel, origin) = match &op.kind {
            OpKind::Download {
                release,
                channel,
                origin,
                ..
            } => (
                op.addon_id.clone(),
                op.name.clone(),
                release.version.clone(),
                *channel,
                *origin,
            ),
            OpKind::Rollback {
                version,
                channel,
                origin,
                ..
            } => (op.addon_id.clone(), op.name.clone(), version.clone(), *channel, *origin),
            _ => {
                let (main, title, version) = match local {
                    Some(facts) => (facts.main_folder, facts.title, facts.version),
                    None => {
                        let main = ops::main_folder(&outcome.folders)
                            .cloned()
                            .unwrap_or_else(|| op.name.clone());
                        (main.clone(), main, None)
                    }
                };
                (
                    format!("local:{main}"),
                    title,
                    version.unwrap_or_default(),
                    Channel::Stable,
                    Origin::LocalZip,
                )
            }
        };
        let mut folders = outcome.folders.clone();
        folders.sort_by_key(|folder| !same_main(folder, &id));
        if let Some(addon) = self.bundle.as_deref().and_then(|b| b.addon(&id)) {
            // Main folder first, the order the catalog lists them in.
            let order = |folder: &String| {
                addon
                    .folders
                    .iter()
                    .position(|known| known.eq_ignore_ascii_case(folder))
                    .unwrap_or(usize::MAX)
            };
            folders.sort_by_key(order);
        }
        if self.demo {
            self.demo_add_folders(&folders, &name, &version);
        }
        let Some(record) = self.state.current_mut() else {
            return;
        };
        let prev = record.managed.get(&id).cloned();
        let new_record = ops::record_after_install(
            prev.as_ref(),
            ops::Installed {
                id: &id,
                name: &name,
                version: &version,
                channel,
                origin,
                folders: folders.clone(),
                kept,
                keep_previous,
                now,
            },
        );
        for stale in ops::stale_records(&record.managed, &id, origin, &folders) {
            record.managed.remove(&stale);
        }
        record.managed.insert(id.clone(), new_record);
        match op.verb {
            Verb::Install => self.state.stats.addons_installed += 1,
            Verb::Update => self.state.stats.updates_applied += 1,
            _ => {}
        }
        self.save_state();
        if op.verb == Verb::RollBack {
            self.yellow(format!(
                "Rolled back to {version}. Tick \"Don't update this addon\" to stay on it."
            ));
        }
        self.loot = Some((
            format!("You receive addon: {name}."),
            Instant::now() + LOOT_LINE_FOR,
        ));
        self.local_dirty = true;
        self.rescan();
    }

    fn demo_add_folders(&mut self, folders: &[String], name: &str, version: &str) {
        for folder in folders {
            self.demo_disk
                .retain(|f| !f.name.eq_ignore_ascii_case(folder));
            let mut local = LocalFolder {
                name: folder.clone(),
                toc_file: Some(format!("{folder}.toc")),
                targets_forever: true,
                modified: demo::NOW,
                ..LocalFolder::default()
            };
            local.toc.title = Some(name.to_owned());
            local.toc.version = Some(version.to_owned());
            local.toc.interfaces = vec![model::FOREVER_INTERFACE];
            self.demo_disk.push(local);
        }
    }

    fn op_removed(&mut self, op: &Op) {
        if let OpKind::Remove { folders } = &op.kind {
            self.demo_disk
                .retain(|f| !folders.iter().any(|gone| gone.eq_ignore_ascii_case(&f.name)));
        }
        if !op.addon_id.is_empty() && !op.addon_id.starts_with("other:") {
            if let Some(record) = self.state.current_mut() {
                record.managed.remove(&op.addon_id);
            }
            self.save_state();
            if self.sel_my == Some(Selection::Addon(op.addon_id.clone())) {
                self.sel_my = None;
            }
        }
        self.loot = Some((format!("Removed {}.", op.name), Instant::now() + LOOT_LINE_FOR));
        self.local_dirty = true;
        self.rescan();
    }

    fn update_checked(
        &mut self,
        manual: bool,
        result: Result<Option<Box<feeling_core::selfupdate::UpdateInfo>>, String>,
    ) {
        match result {
            Ok(Some(info)) => {
                let skipped = self.state.skip_app_version.as_deref() == Some(info.version.as_str());
                if manual || !skipped {
                    self.show_popup(Popup::AppUpdate(info));
                }
            }
            Ok(None) => {
                if manual {
                    self.yellow(format!("You have the latest version ({VERSION})."));
                }
            }
            // A check at launch fails silently: it's only ever been logged.
            Err(text) => {
                if manual {
                    self.red(text);
                }
            }
        }
    }

    pub fn ask_for_diagnostics(&mut self) {
        self.diagnostics_wanted = true;
        self.worker.read_log();
    }

    fn diagnostics_text(&self, log_lines: &[String]) -> String {
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(&text);
            out.push('\n');
        };
        line(format!("WoW is a Feeling {VERSION}"));
        line(format!(
            "OS: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        match &self.game {
            Some(game) => {
                line(format!("Install: {}", game.shown_path));
                line(format!("Found: {}", game.how.describe()));
                line(format!(
                    "Game version: {}",
                    game.install.version.as_deref().unwrap_or("unknown")
                ));
            }
            None => line("Install: not found".to_owned()),
        }
        line(format!(
            "Managed addons: {}",
            self.managed_map().map_or(0, BTreeMap::len)
        ));
        line(format!("Other folders: {}", self.groups.len()));
        line(format!(
            "Catalog addons: {}",
            self.bundle.as_deref().map_or(0, |b| b.catalog.addons.len())
        ));
        line(format!("Catalog status: {:?}", self.catalog_status));
        line(String::new());
        line("Last log lines:".to_owned());
        for log_line in log_lines {
            line(log_line.clone());
        }
        out
    }

    // --- Derived data ------------------------------------------------------------------

    /// Rebuild only what its inputs have invalidated. On a quiet frame this does nothing.
    pub fn refresh(&mut self) {
        if std::mem::take(&mut self.local_dirty) {
            let bundle = self.bundle.clone();
            let empty = BTreeMap::new();
            let managed = self.managed_map().unwrap_or(&empty);
            let owned: Vec<&str> = managed
                .values()
                .flat_map(|m| m.folders.iter().map(String::as_str))
                .collect();
            let groups = scan::group_unmanaged(&self.folders, owned.iter().copied());
            let adoptions = bundle
                .as_deref()
                .map(|b| scan::match_to_catalog(&self.folders, &b.catalog, owned.iter().copied()))
                .unwrap_or_default();
            let updates = model::updates_available(managed, bundle.as_deref());
            self.groups = groups;
            self.adoptions = adoptions;
            self.updates = updates;
            self.rows_dirty = true;
        }
        if std::mem::take(&mut self.rows_dirty) {
            let empty = BTreeMap::new();
            self.rows = match self.tab {
                Tab::Browse => self.bundle.as_deref().map_or_else(Vec::new, |bundle| {
                    model::browse_rows(bundle, &self.query, self.sort, &self.toggled)
                }),
                Tab::MyAddons => {
                    let managed = self.managed_map().unwrap_or(&empty);
                    model::my_rows(managed, &self.updates, &self.groups, &self.query, &self.toggled)
                }
                Tab::Settings => Vec::new(),
            };
            self.fix_selection();
        }
    }

    /// Keep the selection if it still means something. A hidden row in a closed category
    /// still does; while searching, the first match is what's wanted.
    fn fix_selection(&mut self) {
        if self.tab == Tab::Settings {
            return;
        }
        let bundle = self.bundle.clone();
        let current = self.selection().cloned();
        let searching = !model::effective_query(&self.query).is_empty();
        let keep = current.as_ref().is_some_and(|selection| {
            model::row_of(&self.rows, selection, bundle.as_deref(), &self.groups).is_some()
                || (!searching && self.selection_resolves(selection))
        });
        if keep {
            return;
        }
        let first = model::first_row(&self.rows).and_then(|row| {
            model::row_selection(&self.rows[row], bundle.as_deref(), &self.groups)
        });
        self.set_selection(first);
    }

    fn selection_resolves(&self, selection: &Selection) -> bool {
        let bundle = self.bundle.as_deref();
        match selection {
            Selection::Addon(id) => {
                self.managed(id).is_some() || bundle.is_some_and(|b| b.addon(id).is_some())
            }
            Selection::Other(primary) => self.group(primary).is_some(),
            Selection::Kit(i) => bundle.is_some_and(|b| *i < b.catalog.kits.len()),
            Selection::Gap(i) => bundle.is_some_and(|b| *i < b.catalog.gaps.len()),
        }
    }

    // --- One frame ---------------------------------------------------------------------

    pub fn draw(&mut self, ui: &mut Ui) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let ctx = ui.ctx().clone();
        {
            let mut marks = self
                .marks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if marks.first_frame_ms.is_none() {
                marks.first_frame_ms = Some(self.started.elapsed().as_millis());
            }
        }
        self.keyboard(&ctx);
        self.take_drops(&ctx);
        self.refresh();
        views::draw(self, ui);

        let had_actions = !self.actions.is_empty();
        for action in std::mem::take(&mut self.actions) {
            self.apply(action);
        }
        self.refresh();
        if had_actions {
            ctx.request_repaint();
        }
        self.remember_window(&ctx);
        self.fit_window(&ctx);
        self.cursors.apply(&ctx);
        self.schedule_wake(&ctx);
        self.take_screenshot_if_asked(&ctx);
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.popup.is_some() {
            return;
        }
        let search_id = toolbar::search_id();
        let focused = ctx.memory(|m| m.focused());
        let search_focused = focused == Some(search_id);
        // Typing `/` or Ctrl+F jumps to the search box from anywhere that isn't text.
        let typing_elsewhere = focused.is_some() && !search_focused && ctx.egui_wants_keyboard_input();
        if typing_elsewhere {
            return;
        }
        let (down, up, enter, escape, find, slash) = ctx.input_mut(|i| {
            let slash = !search_focused
                && i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Text(t) if t == "/"));
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.key_pressed(Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::COMMAND, Key::F),
                slash,
            )
        });
        if slash || find {
            ctx.memory_mut(|m| m.request_focus(search_id));
        }
        if escape {
            if self.query.is_empty() {
                ctx.memory_mut(|m| m.surrender_focus(search_id));
            } else {
                self.query.clear();
                self.rows_dirty = true;
                self.list_to_top = true;
            }
        }
        if enter {
            if search_focused {
                self.enter_in_search(ctx);
            } else if focused.is_none()
                && let Some(action) = self.enter_action()
            {
                self.actions.push(action);
            }
        }
        if (down || up) && self.tab != Tab::Settings {
            self.move_selection(down);
        }
    }

    /// Enter in the search box runs a command if that's what it holds. Otherwise it just
    /// hands the keyboard to the list, so an install is always a separate, deliberate press.
    fn enter_in_search(&mut self, ctx: &egui::Context) {
        ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        let line = self.query.trim().to_owned();
        if line.starts_with('/') {
            self.query.clear();
            self.rows_dirty = true;
            self.run_slash(&line);
        } else {
            ctx.memory_mut(|m| m.surrender_focus(toolbar::search_id()));
        }
    }

    fn move_selection(&mut self, down: bool) {
        let bundle = self.bundle.clone();
        let from = self
            .selection()
            .and_then(|s| model::row_of(&self.rows, s, bundle.as_deref(), &self.groups));
        let Some(next) = model::step_selection(&self.rows, from, down) else {
            return;
        };
        let selection =
            model::row_selection(&self.rows[next], bundle.as_deref(), &self.groups);
        self.set_selection(selection);
        self.pending_scroll = Some(next);
    }

    fn take_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            self.actions.push(Action::ZipDropped(path));
        }
    }

    fn remember_window(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        let rect = ctx.input(|i| {
            let viewport = i.viewport();
            if viewport.minimized == Some(true) || viewport.maximized == Some(true) {
                return None;
            }
            Some((viewport.outer_rect?.min, viewport.inner_rect?.size()))
        });
        if let Some((pos, size)) = rect {
            // Stored without the UI scale, because that's how the window gets created.
            self.window_rect = Some(((pos.to_vec2() * zoom).to_pos2(), size * zoom));
        }
    }

    /// Everything's laid out for at least 820x560 points, so a bigger UI scale needs a
    /// bigger window, when the screen has room for one.
    fn fit_window(&mut self, ctx: &egui::Context) {
        if self.fit_window_in == 0 {
            return;
        }
        self.fit_window_in -= 1;
        if self.fit_window_in > 0 {
            ctx.request_repaint();
            return;
        }
        let (monitor, inner) = ctx.input(|i| {
            (
                i.viewport().monitor_size,
                i.viewport().inner_rect.map(|r| r.size()),
            )
        });
        let room = monitor.map_or(MIN_POINTS, |m| m - vec2(16.0, 64.0));
        let least = MIN_POINTS.min(room).max(vec2(320.0, 240.0));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(least));
        if let Some(inner) = inner
            && (inner.x < least.x - 0.5 || inner.y < least.y - 0.5)
        {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(inner.max(least)));
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        let scale = scale.clamp(0.8, 2.0);
        self.scale_draft = scale;
        self.ctx.set_zoom_factor(scale);
        self.state.settings.ui_scale = scale;
        self.fit_window_in = 2;
        self.save_state();
    }

    /// Sleep until the next thing on a timer. No timers, no frames.
    fn schedule_wake(&self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut wake = self.messages.next_wake(now);
        let mut sooner = |d: Option<Duration>| {
            if let Some(d) = d {
                wake = Some(wake.map_or(d, |w: Duration| w.min(d)));
            }
        };
        sooner(self.fun.next_wake(now));
        sooner(self.details.next_wake(now));
        sooner(
            self.loot
                .as_ref()
                .map(|(_, until)| until.saturating_duration_since(now)),
        );
        match wake {
            Some(Duration::ZERO) => ctx.request_repaint(),
            Some(delay) => ctx.request_repaint_after(delay),
            None => {}
        }
    }

    /// `--screenshot`: let everything settle, save a PNG, quit.
    fn take_screenshot_if_asked(&mut self, ctx: &egui::Context) {
        let Some(path) = self.launch.screenshot.clone() else {
            return;
        };
        if self.shot_taken {
            return;
        }
        self.shot_frames += 1;
        ctx.request_repaint();
        let waited = self.started.elapsed();
        let hover_ready = self.launch.hover.is_none() || waited > Duration::from_millis(700);
        let settled = self.demo || self.is_settled() || waited > Duration::from_secs(20);
        if self.shot_frames >= 8 && hover_ready && settled {
            self.shot_taken = true;
            crate::save_screenshot(ctx, path, true);
        }
    }

    pub fn script_tick(&mut self, now: Instant) {
        if let Some(mut script) = self.script.take() {
            script.tick(self, now);
            self.script = Some(script);
            self.ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    /// Add the time this session has been open.
    fn record_session(&mut self) {
        self.state.stats.seconds_open += self.started.elapsed().as_secs();
        self.started = Instant::now();
    }

    /// The window is closing. Stop what can stop, let what can't finish, and save.
    pub fn shut_down(&mut self) {
        if std::mem::replace(&mut self.shut_down, true) || self.demo || self.relaunching {
            return;
        }
        self.queue.cancel_all();
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while self.queue.running_count() > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                log::warn!("Closing with an operation still running. It'll recover next time.");
                break;
            }
            match self.rx.recv_timeout(left) {
                Ok(event) => self.handle(event, Instant::now()),
                Err(_) => break,
            }
        }
        self.record_session();
        if let Some((pos, size)) = self.window_rect {
            self.state.settings.window_pos = Some([pos.x, pos.y]);
            self.state.settings.window_size = Some([size.x, size.y]);
        }
        self.worker.finish_saves();
        if let Err(e) = self.state.save_to(&self.worker.dirs().state_file) {
            log::error!("Couldn't save settings: {e}");
        }
    }

    /// For the test harness: keep the pointer over one spot.
    pub fn input_hook(&self, raw: &mut egui::RawInput) {
        if let Some([x, y]) = self.launch.hover {
            raw.events
                .push(egui::Event::PointerMoved(egui::pos2(x, y)));
        }
    }

}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.05, 0.04, 0.03, 1.0]
    }

    /// Runs even while minimized, so a finished job gets recorded either way.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        App::logic(self);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.input_hook(raw_input);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shut_down();
    }
}

fn same_main(folder: &str, id: &str) -> bool {
    id.strip_prefix("local:")
        .is_some_and(|main| main.eq_ignore_ascii_case(folder))
}

/// `kit:0`, `gap:0`, `other:Folder`, or an addon id.
fn parse_selection(text: &str) -> Selection {
    if let Some(i) = text.strip_prefix("kit:").and_then(|i| i.parse().ok()) {
        Selection::Kit(i)
    } else if let Some(i) = text.strip_prefix("gap:").and_then(|i| i.parse().ok()) {
        Selection::Gap(i)
    } else if let Some(primary) = text.strip_prefix("other:") {
        Selection::Other(primary.to_owned())
    } else {
        Selection::Addon(text.to_owned())
    }
}

fn default_browse_folder() -> PathBuf {
    if cfg!(windows) {
        return PathBuf::from(r"C:\Program Files (x86)");
    }
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_parse_from_the_command_line() {
        assert_eq!(parse_selection("kit:2"), Selection::Kit(2));
        assert_eq!(parse_selection("gap:1"), Selection::Gap(1));
        assert_eq!(parse_selection("other:Foo"), Selection::Other("Foo".to_owned()));
        assert_eq!(
            parse_selection("gh:a/b"),
            Selection::Addon("gh:a/b".to_owned())
        );
    }

    #[test]
    fn a_dropped_zips_main_folder_sorts_first() {
        assert!(same_main("Foo", "local:foo"));
        assert!(!same_main("Bar", "local:foo"));
        assert!(!same_main("Foo", "gh:a/foo"));
    }
}
