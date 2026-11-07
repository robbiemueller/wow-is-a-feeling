//! What the player can ask for. Views only push these, so a frame never changes state
//! halfway through being drawn.

use crate::app::App;
use crate::dialogs::{ListPreview, Popup, RemovePlan};
use crate::easter;
use crate::model::{self, HeaderId, Section, Selection, Tab};
use crate::ops::{self, Op, OpKind, Verb, folder_set};
use crate::views::settings::VERSION;
use crate::worker::FoundGame;
use eframe::egui::OpenUrl;
use feeling_core::backup::{self, BackupEntry};
use feeling_core::catalog::Channel;
use feeling_core::deps;
use feeling_core::listfile;
use feeling_core::search::SortBy;
use feeling_core::selfupdate::UpdateInfo;
use feeling_core::state::{ManagedAddon, Origin};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone)]
pub enum Action {
    SetTab(Tab),
    SetSection(Section),
    Select(Selection),
    /// From the empty My Addons page: over to Browse, with this kit up.
    GoToKit(usize),
    ToggleHeader(HeaderId),
    SetSort(SortBy),
    SearchChanged,
    /// Install, update or reinstall these, whichever each one needs.
    Install(Vec<String>),
    UpdateAll,
    Rollback(String),
    AskRemove(Selection),
    ConfirmRemove(RemovePlan),
    Adopt(String),
    OpenLink(String),
    Share(String),
    SetPin(String, bool),
    SetBeta(String, bool),
    CancelOps,
    ChooseFolder,
    LookAgain,
    BrowseTo(PathBuf),
    UseFolder(Box<FoundGame>),
    CancelFolder,
    BackupNow,
    AskRestore(BackupEntry),
    ConfirmRestore(PathBuf),
    SetBackupWtf(bool),
    SetKeepPrevious(bool),
    SetCheckUpdates(bool),
    SetScale(f32),
    CopyList,
    PreviewList,
    ConfirmList(Vec<(String, String, Channel)>),
    CheckUpdateNow,
    UpdateNow(Box<UpdateInfo>),
    SkipVersion(String),
    CopyDiagnostics,
    RetryCatalog,
    PortraitClicked,
    ZipDropped(PathBuf),
}

/// One op about to join the queue.
struct Draft {
    verb: Verb,
    kind: OpKind,
    addon_id: String,
    name: String,
    folders: HashSet<String>,
    /// Run after the previous draft in this list.
    chained: bool,
    exclusive: bool,
}

impl App {
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::SetTab(tab) => {
                self.set_tab(tab);
                if tab == Tab::Settings && self.section == Section::Backups {
                    self.worker.list_backups();
                }
            }
            Action::SetSection(section) => {
                self.section = section;
                self.page_to_top = true;
                if section == Section::Backups {
                    self.worker.list_backups();
                }
            }
            Action::Select(selection) => self.set_selection(Some(selection)),
            Action::GoToKit(index) => {
                self.set_tab(Tab::Browse);
                self.refresh();
                self.set_selection(Some(Selection::Kit(index)));
            }
            Action::ToggleHeader(id) => self.toggle_header(id),
            Action::SetSort(sort) => {
                self.sort = sort;
                self.mark_rows_dirty();
                self.list_to_top = true;
            }
            Action::SearchChanged => {
                let looks_like_search = !model::effective_query(&self.query).is_empty();
                if self.tab == Tab::Settings && looks_like_search {
                    self.set_tab(Tab::Browse);
                }
                self.mark_rows_dirty();
                self.list_to_top = true;
            }
            Action::Install(ids) => self.install_addons(ids),
            Action::UpdateAll => {
                let ids = self.updates.clone();
                self.install_addons(ids);
            }
            Action::Rollback(id) => self.roll_back(&id),
            Action::AskRemove(selection) => self.ask_remove(&selection),
            Action::ConfirmRemove(plan) => self.remove(plan),
            Action::Adopt(id) => self.adopt(&id),
            Action::OpenLink(url) => self.open_link(&url),
            Action::Share(id) => self.share(&id),
            Action::SetPin(id, on) => self.set_pin(&id, on),
            Action::SetBeta(id, on) => self.set_beta(&id, on),
            Action::CancelOps => {
                self.queue.cancel_all();
            }
            Action::ChooseFolder => self.open_folder_browser(),
            Action::LookAgain => self.look_again(),
            Action::BrowseTo(path) => {
                if let Some(Popup::Folder(browser)) = &mut self.popup {
                    browser.typed = path.display().to_string();
                    browser.waiting_for = Some(path.clone());
                    self.worker.list_dir(path);
                }
            }
            Action::UseFolder(game) => self.use_game(*game),
            Action::CancelFolder => self.after_folder.clear(),
            Action::BackupNow => self.enqueue(vec![backup_draft()]),
            Action::AskRestore(entry) => self.show_popup(Popup::Restore {
                zip: entry.path,
                when: model::date_text(entry.created),
            }),
            Action::ConfirmRestore(zip) => self.enqueue(vec![Draft {
                verb: Verb::Restore,
                kind: OpKind::RestoreWtf { zip },
                addon_id: String::new(),
                name: String::new(),
                folders: HashSet::new(),
                chained: false,
                exclusive: true,
            }]),
            Action::SetBackupWtf(on) => {
                self.state.settings.backup_wtf = on;
                self.save_state();
            }
            Action::SetKeepPrevious(on) => {
                self.state.settings.keep_previous_version = on;
                self.save_state();
            }
            Action::SetCheckUpdates(on) => {
                self.state.settings.check_app_updates = on;
                self.save_state();
            }
            Action::SetScale(scale) => self.set_scale(scale),
            Action::CopyList => self.copy_list(),
            Action::PreviewList => self.preview_list(),
            Action::ConfirmList(add) => {
                let mut ids = Vec::with_capacity(add.len());
                for (id, _, channel) in add {
                    self.channel_choice.insert(id.clone(), channel);
                    ids.push(id);
                }
                self.install_addons(ids);
            }
            Action::CheckUpdateNow => self.worker.check_for_update(VERSION, true),
            Action::UpdateNow(update) => self.enqueue(vec![Draft {
                verb: Verb::AppUpdate,
                name: update.version.clone(),
                kind: OpKind::SelfUpdate { update },
                addon_id: String::new(),
                folders: HashSet::new(),
                chained: false,
                exclusive: true,
            }]),
            Action::SkipVersion(version) => {
                self.state.skip_app_version = Some(version);
                self.save_state();
            }
            Action::CopyDiagnostics => self.ask_for_diagnostics(),
            Action::RetryCatalog => self.retry_catalog(),
            Action::PortraitClicked => {
                if self.fun.gag.click(Instant::now()) {
                    self.yellow(easter::CLEANING_MESSAGE);
                }
            }
            Action::ZipDropped(path) => self.zip_dropped(path),
        }
    }

    /// A link only ever reaches the browser if it's plainly a web address.
    pub fn open_link(&mut self, url: &str) {
        match model::safe_url(url) {
            Some(url) => self.ctx.open_url(OpenUrl::new_tab(url)),
            None => log::info!("Refused to open a link that isn't http or https"),
        }
    }

    /// No game yet means ask for one first. The request picks up again afterward.
    fn needs_a_folder(&mut self, action: Action) -> bool {
        if self.game.is_some() {
            return false;
        }
        self.after_folder.push(action);
        self.open_folder_browser();
        true
    }

    pub fn install_addons(&mut self, ids: Vec<String>) {
        if ids.is_empty() || self.needs_a_folder(Action::Install(ids.clone())) {
            return;
        }
        let Some(bundle) = self.bundle.clone() else {
            return;
        };
        let installed = self.installed_folders();
        let mut plan: Vec<String> = Vec::new();
        let mut chained: HashSet<String> = HashSet::new();
        for id in &ids {
            let resolved = deps::resolve(id, &bundle.catalog, &bundle.folders, installed.iter());
            let name = bundle.addon(id).map_or(id.as_str(), |a| a.name.as_str());
            for missing in &resolved.missing {
                self.yellow(format!(
                    "{name} requires {}, which isn't in the catalog. Installing it anyway.",
                    model::clean_line(missing, 60)
                ));
            }
            for (position, step) in resolved.to_install.iter().enumerate() {
                if step != id
                    && !ids.contains(step)
                    && !plan.contains(step)
                    && let Some(dep) = bundle.addon(step)
                {
                    self.yellow(format!(
                        "{name} requires {}, so that installs first.",
                        dep.name
                    ));
                }
                if !plan.contains(step) {
                    plan.push(step.clone());
                    if position > 0 {
                        chained.insert(step.clone());
                    }
                }
            }
        }
        let mut drafts = Vec::with_capacity(plan.len());
        for id in &plan {
            if self.queue.busy_with(id) {
                continue;
            }
            if let Some(mut draft) = self.install_draft(id) {
                draft.chained = chained.contains(id) && !drafts.is_empty();
                drafts.push(draft);
            }
        }
        self.enqueue(drafts);
    }

    fn installed_folders(&self) -> Vec<String> {
        let mut folders: Vec<String> = self.folders.iter().map(|f| f.name.clone()).collect();
        if let Some(managed) = self.managed_map() {
            folders.extend(managed.values().flat_map(|m| m.folders.iter().cloned()));
        }
        folders
    }

    fn install_draft(&mut self, id: &str) -> Option<Draft> {
        let bundle = self.bundle.clone()?;
        let addon = bundle.addon(id)?;
        let record = self.managed(id).cloned();
        let channel = model::channel_for(
            record.as_ref(),
            self.channel_choice.get(id).copied(),
            self.state.settings.default_channel,
        );
        let Some(release) = addon.release(channel) else {
            self.red(format!("{} has no build for Forever yet.", addon.name));
            return None;
        };
        let verb = match &record {
            None => Verb::Install,
            Some(m) if model::same_version(&m.version, &release.version) => Verb::Reinstall,
            Some(_) => Verb::Update,
        };
        let old_folders = self
            .managed_map()
            .map(|m| ops::own_folders(id, m))
            .unwrap_or_default();
        let mut folders = folder_set(&addon.folders);
        folders.extend(folder_set(&old_folders));
        let origin = record.as_ref().map_or(Origin::Catalog, |m| m.origin);
        Some(Draft {
            verb,
            kind: OpKind::Download {
                release: release.clone(),
                channel,
                origin,
                old_folders,
            },
            addon_id: id.to_owned(),
            name: addon.name.clone(),
            folders,
            chained: false,
            exclusive: false,
        })
    }

    /// Hand a batch to the queue, with the checks a batch gets: is the game running, and
    /// is a settings backup due first?
    fn enqueue(&mut self, drafts: Vec<Draft>) {
        if drafts.is_empty() {
            return;
        }
        let touches_addons = drafts.iter().any(|d| {
            matches!(
                d.verb,
                Verb::Install | Verb::Update | Verb::Reinstall | Verb::RollBack | Verb::Remove
            )
        });
        if touches_addons && self.queue.is_idle() {
            self.worker.check_game_running();
        }
        let updating = drafts.iter().any(|d| d.verb == Verb::Update);
        if updating && self.backup_is_due() {
            let backup = backup_draft();
            self.push_draft(backup, None);
        }
        let mut previous = None;
        for draft in drafts {
            let chained = draft.chained;
            let id = self.push_draft(draft, previous.filter(|_| chained));
            previous = Some(id);
        }
        self.pump_queue();
    }

    fn push_draft(&mut self, draft: Draft, after: Option<u64>) -> u64 {
        let id = self.next_op_id();
        self.queue.push(Op {
            id,
            verb: draft.verb,
            kind: draft.kind,
            addon_id: draft.addon_id,
            name: draft.name,
            folders: draft.folders,
            after,
            exclusive: draft.exclusive,
        });
        id
    }

    fn backup_is_due(&self) -> bool {
        if !self.state.settings.backup_wtf {
            return false;
        }
        let last = self
            .state
            .current()
            .map(|record| record.last_wtf_backup)
            .filter(|last| *last > 0);
        backup::should_backup(last, self.now_unix())
    }

    fn roll_back(&mut self, id: &str) {
        let Some(record) = self.managed(id).cloned() else {
            return;
        };
        let (Some(zip), Some(version)) = (record.previous_zip, record.previous_version) else {
            return;
        };
        let old_folders = self
            .managed_map()
            .map(|m| ops::own_folders(id, m))
            .unwrap_or_default();
        let folders = folder_set(&old_folders);
        self.enqueue(vec![Draft {
            verb: Verb::RollBack,
            kind: OpKind::Rollback {
                zip,
                version,
                channel: record.channel,
                origin: record.origin,
                old_folders,
            },
            addon_id: id.to_owned(),
            name: record.name,
            folders,
            chained: false,
            exclusive: false,
        }]);
    }

    fn remove(&mut self, plan: RemovePlan) {
        let folders = folder_set(&plan.folders);
        // Unmanaged folders still get an id, so the same page can't start it twice.
        let addon_id = plan.addon_id.clone().unwrap_or_else(|| {
            format!("other:{}", plan.folders.first().map_or("", String::as_str))
        });
        self.enqueue(vec![Draft {
            verb: Verb::Remove,
            kind: OpKind::Remove {
                folders: plan.folders,
            },
            addon_id,
            name: plan.name,
            folders,
            chained: false,
            exclusive: false,
        }]);
    }

    /// Take over an addon another tool installed. Nothing on disk changes: from here on
    /// it just has a record, so it gets updates.
    fn adopt(&mut self, id: &str) {
        let Some(adoption) = self.adoption_for(id).cloned() else {
            return;
        };
        let bundle = self.bundle.clone();
        let Some(addon) = bundle.as_deref().and_then(|b| b.addon(id)) else {
            return;
        };
        let now = self.now_unix();
        let channel = self.state.settings.default_channel;
        let record = ManagedAddon {
            id: id.to_owned(),
            name: addon.name.clone(),
            version: adoption.local_version.clone().unwrap_or_default(),
            channel,
            folders: adoption.folders.clone(),
            installed_at: now,
            updated_at: now,
            pinned: false,
            origin: Origin::Adopted,
            previous_zip: None,
            previous_version: None,
        };
        if let Some(install) = self.state.current_mut() {
            install.managed.insert(id.to_owned(), record);
        }
        self.save_state();
        self.mark_local_dirty();
        self.refresh();
        self.set_selection(Some(Selection::Addon(id.to_owned())));
        self.yellow(format!(
            "Now managing {}. It gets updates from here on.",
            addon.name
        ));
    }

    fn share(&mut self, id: &str) {
        let bundle = self.bundle.clone();
        let Some(addon) = bundle.as_deref().and_then(|b| b.addon(id)) else {
            return;
        };
        let text = match model::addon_page_url(addon) {
            Some(url) => format!("{}: {url}", addon.name),
            None => format!("{}: {}", addon.name, addon.id),
        };
        self.ctx.copy_text(text);
        self.yellow(format!("Copied a link to {}.", addon.name));
    }

    fn set_pin(&mut self, id: &str, on: bool) {
        if let Some(install) = self.state.current_mut()
            && let Some(record) = install.managed.get_mut(id)
        {
            record.pinned = on;
            self.save_state();
            self.mark_local_dirty();
        }
    }

    fn set_beta(&mut self, id: &str, on: bool) {
        let channel = if on { Channel::Beta } else { Channel::Stable };
        let managed = self
            .state
            .current_mut()
            .and_then(|install| install.managed.get_mut(id));
        match managed {
            Some(record) => {
                record.channel = channel;
                self.save_state();
                self.mark_local_dirty();
            }
            None => {
                self.channel_choice.insert(id.to_owned(), channel);
            }
        }
    }

    fn look_again(&mut self) {
        if self.demo {
            if self.game.is_none() {
                self.yellow("Still can't find it.");
            }
            return;
        }
        self.game = None;
        self.game_state = model::GameState::Searching;
        self.scanned = false;
        self.mark_local_dirty();
        let saved = self.state.current_install.clone();
        self.worker.find_game(self.launch.game.clone(), saved);
    }

    fn copy_list(&mut self) {
        let Some(managed) = self.managed_map() else {
            return;
        };
        let count = managed.len();
        let text = listfile::export(managed.values());
        self.ctx.copy_text(text);
        self.yellow(format!(
            "Copied your list of {count} addon{}.",
            if count == 1 { "" } else { "s" }
        ));
    }

    /// A list is data from a stranger: it can only name addons the catalog already has,
    /// and the player sees exactly what it would add before anything happens.
    fn preview_list(&mut self) {
        let Some(bundle) = self.bundle.clone() else {
            return;
        };
        let parsed = listfile::parse(&self.list_text);
        let mut preview = ListPreview::default();
        for (id, channel) in parsed.entries {
            match bundle.addon(&id) {
                None => preview.unknown.push(model::clean_line(&id, 80)),
                Some(addon) if self.managed(&id).is_some() => {
                    preview.have.push(addon.name.clone());
                }
                Some(addon) => preview.add.push((id, addon.name.clone(), channel)),
            }
        }
        preview.unknown.extend(
            parsed
                .unknown_lines
                .iter()
                .map(|line| model::clean_line(line, 80)),
        );
        self.show_popup(Popup::List(preview));
    }

    fn retry_catalog(&mut self) {
        if self.demo {
            self.yellow("The demo has no network to try.");
            return;
        }
        if self.bundle.is_none() {
            self.catalog_status = model::CatalogStatus::Loading;
        }
        self.fetch_catalog();
    }

    fn zip_dropped(&mut self, path: PathBuf) {
        if !ops::is_zip(&path) {
            self.red("Only .zip files can be installed this way.");
            return;
        }
        if self.needs_a_folder(Action::ZipDropped(path.clone())) {
            return;
        }
        let name = path.file_stem().map_or_else(
            || "the zip".to_owned(),
            |s| s.to_string_lossy().into_owned(),
        );
        self.enqueue(vec![Draft {
            verb: Verb::Install,
            kind: OpKind::LocalZip { path },
            addon_id: String::new(),
            name: model::clean_line(&name, 60),
            folders: HashSet::new(),
            chained: false,
            // What's inside isn't known until it's open, so nothing runs beside it.
            exclusive: true,
        }]);
    }
}

fn backup_draft() -> Draft {
    Draft {
        verb: Verb::Backup,
        kind: OpKind::BackupWtf,
        addon_id: String::new(),
        name: String::new(),
        folders: HashSet::new(),
        chained: false,
        exclusive: true,
    }
}
