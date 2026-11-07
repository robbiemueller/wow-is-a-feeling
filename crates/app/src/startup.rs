//! Getting going: real launches, the fixed demo, and the switches that set up a screenshot.

use crate::actions::Action;
use crate::app::{App, LOOT_LINE_FOR, parse_selection};
use crate::args::Scene;
use crate::demo;
use crate::dialogs::Popup;
use crate::easter;
use crate::messages::Kind;
use crate::model::{CatalogBundle, CatalogStatus, GameState, HeaderId, Selection, Tab};
use crate::ops::Casting;
use crate::views::settings::VERSION;
use crate::worker::{FoundGame, HowFound};
use eframe::egui::ViewportCommand;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl App {
    pub fn start_real(&mut self) {
        let saved = self.state.current_install.clone();
        self.worker.find_game(self.launch.game.clone(), saved);
        self.fetch_catalog();
        if self.state.settings.check_app_updates {
            self.worker.check_for_update(VERSION, false);
        }
    }

    pub fn start_demo(&mut self) {
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

    pub fn apply_launch_flags(&mut self) {
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
            "sort" => self.open_sort = true,
            "running" => self.popup = Some(Popup::AlreadyRunning),
            "update" => self.popup = Some(Popup::AppUpdate(Box::new(demo::update_info()))),
            "restore" => {
                self.popup = Some(Popup::Restore {
                    zip: PathBuf::from("demo.zip"),
                    when: "Nov 5, 2026".to_owned(),
                });
            }
            "remove" => {
                let selection = self
                    .selection()
                    .cloned()
                    .unwrap_or_else(|| Selection::Addon("gh:Questie/Questie".to_owned()));
                self.ask_remove(&selection);
            }
            "folder" => self.open_folder_browser(),
            "replace" => self
                .actions
                .push(Action::Install(vec![demo::UNTRUSTED_ID.to_owned()])),
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
    pub fn start_frame_report(&self) {
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

    pub fn measure_search(&self, bundle: &CatalogBundle) {
        if self.launch.frame_report.is_none() {
            return;
        }
        let queries = [
            "q",
            "qu",
            "que",
            "ques",
            "questi",
            "deadly boss",
            "map",
            "a",
            "zz",
            "addon 12",
        ];
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
}
