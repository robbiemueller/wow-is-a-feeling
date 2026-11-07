//! What the player can ask for. Views only push these, so a frame never changes state
//! halfway through being drawn.

use crate::app::App;
use crate::dialogs::{ListAddon, ListDep, ListPreview, PackPlan, Popup, RemovePlan, ReplacePlan};
use crate::easter;
use crate::model::{self, CatalogBundle, HeaderId, Section, Selection, Tab};
use crate::ops::{self, FolderConflict, Op, OpKind, Verb, folder_set};
use crate::views::settings::VERSION;
use crate::worker::FoundGame;
use eframe::egui::OpenUrl;
use feeling_core::backup::{self, BackupEntry};
use feeling_core::catalog::{Addon, Channel, Release};
use feeling_core::deps::{self, TooBig, Unreviewed};
use feeling_core::listfile;
use feeling_core::search::SortBy;
use feeling_core::selfupdate::UpdateInfo;
use feeling_core::state::{ManagedAddon, Origin};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// The main button on an addon's page. A first install of an addon that needs a pack
    /// nothing here provides asks about that before anything else.
    InstallFromPage(String),
    /// The player answered the packs popup: on with installing this addon, and these packs
    /// beside it, which may be none.
    InstallWithPacks {
        id: String,
        packs: Vec<String>,
    },
    /// The player said yes to the popup: go ahead, and this addon may replace these folders.
    /// The packs they chose before that go too.
    ReplaceAndInstall {
        id: String,
        folders: Vec<String>,
        packs: Vec<String>,
    },
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
    ConfirmList(Vec<ListAddon>),
    CheckUpdateNow,
    UpdateNow(Box<UpdateInfo>),
    SkipVersion(String),
    CopyDiagnostics,
    RetryCatalog,
    PortraitClicked,
    ZipDropped(PathBuf),
}

/// What installing a batch would queue and say, worked out with nothing changed yet.
#[derive(Default)]
struct InstallPlan {
    /// What joins the queue, in order.
    steps: Vec<Step>,
    /// The yellow lines installing says, in order.
    lines: Vec<String>,
}

impl InstallPlan {
    fn queues(&self, id: &str) -> bool {
        self.steps.iter().any(|step| step.id == id)
    }
}

/// One addon an install plan queues.
struct Step {
    id: String,
    /// The asked-for addon it comes with: itself, or one that requires it.
    with: String,
    /// It isn't the first of what its addon brings, so it runs after the step before it.
    chained: bool,
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
            Action::InstallFromPage(id) => self.install_from_page(id),
            Action::InstallWithPacks { id, packs } => self.packs_answered(id, packs),
            Action::ReplaceAndInstall { id, folders, packs } => {
                self.queue_with_packs(&id, &folders, &packs);
            }
            Action::UpdateAll => self.update_all(),
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
                for addon in add {
                    self.channel_choice.insert(addon.id.clone(), addon.channel);
                    ids.push(addon.id);
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

    /// No game yet means ask for one first. The request picks up again afterward. Asked again
    /// while it waits, it's already in hand, and the folder browser is already up.
    fn needs_a_folder(&mut self, action: Action) -> bool {
        if self.game.is_some() {
            return false;
        }
        if !self.after_folder.iter().any(|w| same_request(w, &action)) {
            self.after_folder.push(action);
            self.open_folder_browser();
        }
        true
    }

    /// What's on disk decides what an install may replace. If the first look at the disk is
    /// still on its way, the request waits for it, so it doesn't get decided blind. A second
    /// click while it waits adds nothing, so nothing is asked twice.
    fn needs_a_scan(&mut self, action: Action) -> bool {
        if self.scanned {
            return false;
        }
        if !self.after_scan.iter().any(|w| same_request(w, &action)) {
            self.after_scan.push(action);
        }
        true
    }

    /// Install, update or reinstall these. One addon gets to ask first if it would replace a
    /// folder that isn't its own. Several at once can't, so those sit out and say so.
    pub fn install_addons(&mut self, ids: Vec<String>) {
        if ids.is_empty()
            || self.needs_a_folder(Action::Install(ids.clone()))
            || self.needs_a_scan(Action::Install(ids.clone()))
        {
            return;
        }
        match ids.as_slice() {
            [id] if self.asking_about(id) => {}
            [id] => self.ask_about_folders(id.clone(), Vec::new()),
            _ => self.queue_installs(&ids, &[]),
        }
    }

    /// The main button on an addon's page. A first install of an addon that needs a pack
    /// nothing here provides, such as Spoken and its quest voices, asks about that first. Once
    /// answered it goes on as any other click would, so each question is asked once.
    fn install_from_page(&mut self, id: String) {
        let again = Action::InstallFromPage(id.clone());
        if self.needs_a_folder(again.clone()) || self.needs_a_scan(again) || self.asking_about(&id)
        {
            return;
        }
        match self.pack_plan(&id) {
            Some(plan) => self.show_popup(Popup::Packs(plan)),
            None => self.ask_about_folders(id, Vec::new()),
        }
    }

    /// Is a question about installing this addon on screen, or waiting its turn? Its answer
    /// covers every click that asked it.
    fn asking_about(&self, id: &str) -> bool {
        self.popup
            .iter()
            .chain(&self.popup_queue)
            .any(|popup| match popup {
                Popup::Packs(plan) => plan.addon_id == id,
                Popup::Replace(plan) => plan.addon_id == id,
                _ => false,
            })
    }

    /// The player answered the packs question. One that comes after the addon is already
    /// here or on its way, from a question left open, only adds the packs still missing:
    /// whatever got the addon there has seen to it, and it isn't installed again.
    fn packs_answered(&mut self, id: String, packs: Vec<String>) {
        if self.managed(&id).is_none() && !self.queue.busy_with(&id) {
            self.ask_about_folders(id, packs);
            return;
        }
        let installed = folder_set(&self.installed_folders());
        let missing: Vec<String> = packs
            .into_iter()
            .filter(|pack| self.still_missing(pack, &installed))
            .collect();
        self.queue_installs(&missing, &[]);
    }

    /// A pack nothing has brought: not on its way, with no record, and none of its folders in
    /// AddOns or in a record. `installed` is those folders, lowercased.
    fn still_missing(&self, id: &str, installed: &HashSet<String>) -> bool {
        if self.queue.busy_with(id) || self.managed(id).is_some() {
            return false;
        }
        let pack = self.bundle.as_deref().and_then(|bundle| bundle.addon(id));
        pack.is_some_and(|pack| {
            !pack
                .folders
                .iter()
                .any(|folder| installed.contains(&folder.to_lowercase()))
        })
    }

    /// Ask before an install that would replace folders that aren't this addon's, then queue
    /// it, and the packs the player chose to go with it.
    fn ask_about_folders(&mut self, id: String, packs: Vec<String>) {
        if !self.queue.busy_with(&id)
            && let Some(mut plan) = self.replace_plan(&id)
        {
            plan.packs = packs;
            self.show_popup(Popup::Replace(plan));
            return;
        }
        self.queue_with_packs(&id, &[], &packs);
    }

    /// Queue an addon, then its packs. A pack only follows an addon that really joined the
    /// queue: voices are no use without the addon that plays them. The player just chose the
    /// packs, so the addon requiring one of them is nothing to tell them about.
    fn queue_with_packs(&mut self, id: &str, agreed: &[String], packs: &[String]) {
        self.queue_installs_beside(&[id.to_owned()], agreed, packs);
        if !packs.is_empty() && self.queue.busy_with(id) {
            self.queue_installs(packs, &[]);
        }
    }

    /// The question to ask before a first install of an addon with a needed pack group that
    /// nothing meets: nothing in AddOns or in a record, nothing this install brings with it,
    /// and nothing already on its way. It offers each such group's first pack. A required pack
    /// that's too big to come along by itself, or of unknown size, doesn't count as brought.
    fn pack_plan(&self, id: &str) -> Option<PackPlan> {
        if self.managed(id).is_some() || self.queue.busy_with(id) {
            return None;
        }
        let bundle = self.bundle.as_deref()?;
        let addon = bundle.addon(id)?;
        let installed = self.installed_folders();
        // The addon itself and what it requires, which join the same batch.
        let batch = deps::resolve(
            id,
            &bundle.catalog,
            &bundle.folders,
            installed.iter(),
            |dep| self.channel_of(dep),
        )
        .to_install;
        let batch_folders: HashSet<String> = batch
            .iter()
            .filter_map(|step| bundle.addon(step))
            .flat_map(|step| folder_set(&step.folders))
            .collect();
        let arrives = |choice: &model::PackChoice| {
            batch.contains(&choice.id)
                || bundle.addon(&choice.id).is_some_and(|pack| {
                    pack.folders.iter().any(|folder| {
                        let lower = folder.to_lowercase();
                        batch_folders.contains(&lower) || self.queue.incoming(&lower, id).is_some()
                    })
                })
        };
        // A folder the group's prefixes take, such as another language's voices, on its way.
        let met_on_arrival = |line: &model::PackLine| {
            !line.met_by.is_empty()
                && (batch_folders.iter().any(|f| line.met_by_folder(f))
                    || self.queue.incoming_any(id, |f| line.met_by_folder(f)))
        };
        let lines = model::pack_lines(addon, bundle, &folder_set(&installed), |pack| {
            self.channel_of(pack)
        });
        let unmet: Vec<model::PackLine> = lines
            .into_iter()
            .filter(|line| {
                line.needed
                    && !line.met()
                    && !line.choices.iter().any(arrives)
                    && !met_on_arrival(line)
            })
            .collect();
        if unmet.is_empty() {
            return None;
        }
        let mut packs: Vec<&model::PackChoice> = Vec::new();
        for line in &unmet {
            let first = &line.choices[0];
            if !packs.iter().any(|pack| pack.id == first.id) {
                packs.push(first);
            }
        }
        // Sizes come from the catalog, so a silly one mustn't overflow the sum.
        let size = packs.iter().try_fold(0u64, |total, pack| {
            (pack.size > 0).then(|| total.saturating_add(pack.size))
        });
        Some(PackPlan {
            addon_id: id.to_owned(),
            name: addon.name.clone(),
            needs: unmet.iter().map(|line| line.name.clone()).collect(),
            packs: packs
                .iter()
                .map(|pack| (pack.id.clone(), pack.name.clone()))
                .collect(),
            size,
        })
    }

    /// Update all never asks: it just does what it can without taking anything from anyone.
    fn update_all(&mut self) {
        if self.updates.is_empty()
            || self.needs_a_folder(Action::UpdateAll)
            || self.needs_a_scan(Action::UpdateAll)
        {
            return;
        }
        let ids = self.updates.clone();
        self.queue_installs(&ids, &[]);
    }

    fn folder_conflicts(&self, addon: &Addon) -> Vec<FolderConflict> {
        let empty = BTreeMap::new();
        ops::folder_conflicts(
            addon,
            self.managed_map().unwrap_or(&empty),
            &self.folders,
            &self.folder_lookup,
            &self.queue,
        )
    }

    /// The question to put to the player before installing this, if there is one.
    fn replace_plan(&self, id: &str) -> Option<ReplacePlan> {
        let record = self.managed(id);
        let addon = model::entry_for(self.bundle.as_deref()?, id, record)?;
        let conflicts = self.folder_conflicts(addon);
        if conflicts.is_empty() {
            return None;
        }
        Some(ReplacePlan {
            addon_id: id.to_owned(),
            name: addon.name.clone(),
            source: model::source_text(addon),
            verb: verb_for(record, addon.release(self.channel_of(id))?),
            conflicts,
            packs: Vec::new(),
        })
    }

    /// Why an installed addon whose catalog entry has since become a different addon is left
    /// alone, or `None` while it's still the same addon. Updating to that one would swap this
    /// addon's folders for its.
    fn another_addon_now(&self, id: &str) -> Option<String> {
        let record = self.managed(id)?;
        let addon = self.bundle.as_deref()?.addon(id)?;
        (!model::same_addon(record, addon)).then(|| another_addon_line(&record.name, &addon.name))
    }

    /// Is this addon here already, or about to be? `ids` and `coming` are what this batch
    /// installs and what the player chose to install right after it.
    fn here_or_coming(&self, id: &str, ids: &[String], coming: &[String]) -> bool {
        ids.iter().chain(coming).any(|other| other == id)
            || self.queue.busy_with(id)
            || self.managed(id).is_some()
    }

    /// Queue these and what they require. `agreed` is what the player said one of them may
    /// replace. Nothing here asks: what would take a folder from someone else stays out of
    /// the queue, with a yellow line saying why.
    fn queue_installs(&mut self, ids: &[String], agreed: &[String]) {
        self.queue_installs_beside(ids, agreed, &[]);
    }

    /// [`Self::queue_installs`], when the player has chosen to install `coming` right after:
    /// a requirement among those is on its way, and nothing needs saying about it.
    fn queue_installs_beside(&mut self, ids: &[String], agreed: &[String], coming: &[String]) {
        let Some(bundle) = self.bundle.clone() else {
            return;
        };
        let plan = self.install_plan(&bundle, ids, agreed, coming, |id| self.channel_of(id));
        for line in plan.lines {
            self.yellow(line);
        }
        let mut drafts = Vec::with_capacity(plan.steps.len());
        let mut earlier: HashSet<String> = HashSet::new();
        for step in &plan.steps {
            let id = &step.id;
            // Two addons in one batch can both bring a folder that's new to the disk. Whichever
            // lands second replaces the first's copy, and they share it from then on.
            let mut allowed = if ids.contains(id) {
                agreed.to_vec()
            } else {
                Vec::new()
            };
            if let Some(addon) = bundle.addon(id) {
                allowed.extend(
                    addon
                        .folders
                        .iter()
                        .filter(|folder| earlier.contains(&folder.to_lowercase()))
                        .cloned(),
                );
                earlier.extend(folder_set(&addon.folders));
            }
            if let Some(mut draft) = self.install_draft(id, allowed) {
                draft.chained = step.chained && !drafts.is_empty();
                drafts.push(draft);
            }
        }
        self.enqueue(drafts);
    }

    /// What [`Self::queue_installs_beside`] queues for these, and what it says about them,
    /// worked out without changing anything, so a pasted list's preview can show the same.
    /// `channel` is the channel each addon would install from.
    fn install_plan(
        &self,
        bundle: &CatalogBundle,
        ids: &[String],
        agreed: &[String],
        coming: &[String],
        channel: impl Fn(&str) -> Channel,
    ) -> InstallPlan {
        let installed = self.installed_folders();
        let mut plan = InstallPlan::default();
        let mut told: HashSet<String> = HashSet::new();
        for id in ids {
            if self.queue.busy_with(id) {
                continue;
            }
            if let Some(line) = self.another_addon_now(id) {
                plan.lines.push(line);
                continue;
            }
            let name = bundle.addon(id).map_or(id.as_str(), |a| a.name.as_str());
            if agreed.is_empty()
                && let Some(addon) = bundle.addon(id)
            {
                let conflicts = self.folder_conflicts(addon);
                if !conflicts.is_empty() {
                    let folders: Vec<&str> = conflicts.iter().map(|c| c.folder.as_str()).collect();
                    let line = ops::sitting_out_line(name, self.managed(id).is_some(), &folders);
                    plan.lines.push(line);
                    continue;
                }
            }
            let resolved = deps::resolve(
                id,
                &bundle.catalog,
                &bundle.folders,
                installed.iter(),
                &channel,
            );
            for missing in &resolved.missing {
                plan.lines.push(format!(
                    "{name} requires {}, which this app can't pick for you. Look for it yourself, \
                     in Browse or on the web. Installing {name} anyway.",
                    model::clean_line(missing, 60)
                ));
            }
            // Nothing to say about one that's on its way or already here.
            for left in &resolved.unreviewed {
                if !self.here_or_coming(&left.provider, ids, coming)
                    && told.insert(left.provider.clone())
                {
                    plan.lines.push(unreviewed_line(bundle, left));
                }
            }
            for left in &resolved.too_big {
                if !self.here_or_coming(&left.provider, ids, coming)
                    && told.insert(left.provider.clone())
                {
                    plan.lines.push(too_big_line(bundle, left));
                }
            }
            for (position, step) in resolved.to_install.iter().enumerate() {
                if plan.queues(step) || self.queue.busy_with(step) {
                    continue;
                }
                if step != id
                    && let Some(dep) = bundle.addon(step)
                {
                    let asked_for = ids.contains(step);
                    let conflicts = self.folder_conflicts(dep);
                    if !conflicts.is_empty() {
                        // One that was asked for says so in its own turn.
                        if !asked_for {
                            plan.lines.push(format!(
                                "{name} requires {}, which wasn't added because it would replace \
                                 {}. Install it from its own page to choose.",
                                dep.name,
                                conflict_names(&conflicts)
                            ));
                        }
                        continue;
                    }
                    if !asked_for {
                        plan.lines.push(format!(
                            "{name} requires {}, so that installs first.",
                            dep.name
                        ));
                    }
                }
                plan.steps.push(Step {
                    id: step.clone(),
                    with: id.clone(),
                    chained: position > 0,
                });
            }
        }
        plan
    }

    /// Everything that counts as already there for a dependency: on disk, or in a record.
    pub fn installed_folders(&self) -> Vec<String> {
        let mut folders: Vec<String> = self.folders.iter().map(|f| f.name.clone()).collect();
        if let Some(managed) = self.managed_map() {
            folders.extend(managed.values().flat_map(|m| m.folders.iter().cloned()));
        }
        folders
    }

    /// `allowed` is what this may replace on top of its own folders.
    fn install_draft(&mut self, id: &str, allowed: Vec<String>) -> Option<Draft> {
        let bundle = self.bundle.clone()?;
        if let Some(line) = self.another_addon_now(id) {
            self.yellow(line);
            return None;
        }
        let addon = bundle.addon(id)?;
        let record = self.managed(id).cloned();
        let channel = self.channel_of(id);
        let Some(release) = addon.release(channel) else {
            self.red(format!("{} has no build for Forever yet.", addon.name));
            return None;
        };
        let verb = verb_for(record.as_ref(), release);
        let old_folders = self
            .managed_map()
            .map(|m| ops::own_folders(id, m))
            .unwrap_or_default();
        // The record's folders, not `old_folders`, which leaves out what it shares: an update
        // goes on replacing a folder two addons claim without asking every time.
        let mut may_replace = record.as_ref().map_or_else(Vec::new, |m| m.folders.clone());
        may_replace.extend(allowed);
        let mut folders = folder_set(&addon.folders);
        folders.extend(folder_set(&old_folders));
        folders.extend(folder_set(&may_replace));
        let origin = record.as_ref().map_or(Origin::Catalog, |m| m.origin);
        Some(Draft {
            verb,
            kind: OpKind::Download {
                release: release.clone(),
                channel,
                origin,
                old_folders,
                may_replace,
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
        // Trying again: the casting bar takes over from the old reason.
        self.failures.remove(&draft.addon_id);
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
    /// it just has a record, so it gets updates. One already managed keeps its record, even
    /// when its catalog entry has since become another addon whose folders are here too.
    fn adopt(&mut self, id: &str) {
        if self.managed(id).is_some() {
            return;
        }
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
            not_in_zip: Vec::new(),
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

    /// A list is data from a stranger: it can only name addons the catalog already has, and
    /// the player sees what installing it would do, and how big each download is, before
    /// anything happens. That includes what its addons require that it doesn't name. An
    /// installed addon whose entry has become another addon is left alone, and says so.
    fn preview_list(&mut self) {
        let Some(bundle) = self.bundle.clone() else {
            return;
        };
        let parsed = listfile::parse(&self.list_text);
        let mut preview = ListPreview::default();
        for (id, channel) in parsed.entries {
            let record = self.managed(&id);
            match bundle.addon(&id) {
                None => preview.unknown.push(model::clean_line(&id, 80)),
                Some(addon) if record.is_some_and(|record| model::same_addon(record, addon)) => {
                    preview.have.push(addon.name.clone());
                }
                // Installing it would only say why it's left alone.
                Some(_) if self.another_addon_now(&id).is_some() => {
                    let yours = record.map_or(id.as_str(), |record| record.name.as_str());
                    preview.left_alone.push(model::clean_line(yours, 60));
                }
                Some(addon) => preview.add.push(ListAddon {
                    name: addon.name.clone(),
                    size: addon.release(channel).map_or(0, |release| release.size),
                    id,
                    channel,
                }),
            }
        }
        preview.unknown.extend(
            parsed
                .unknown_lines
                .iter()
                .map(|line| model::clean_line(line, 80)),
        );
        preview.brings = self.brought_along(&bundle, &preview.add);
        self.show_popup(Popup::List(preview));
    }

    /// What installing these listed addons brings with them, from the same plan installing
    /// follows: each addon they require that the list doesn't name and the player doesn't
    /// have or have on its way, once, with the listed addon it comes with.
    fn brought_along(&self, bundle: &CatalogBundle, listed: &[ListAddon]) -> Vec<ListDep> {
        let ids: Vec<String> = listed.iter().map(|addon| addon.id.clone()).collect();
        // Install these sets each listed addon's channel before installing. The rest install
        // from their own.
        let chosen: HashMap<&str, Channel> = listed
            .iter()
            .map(|addon| (addon.id.as_str(), addon.channel))
            .collect();
        let channel = |id: &str| {
            chosen
                .get(id)
                .copied()
                .unwrap_or_else(|| self.channel_of(id))
        };
        // One addon on its own asks before it replaces anything, and goes ahead with what it
        // requires once the player says yes. Several never ask, so one that would replace
        // something sits out, and so does what it requires.
        let agreed: Vec<String> = match ids.as_slice() {
            [id] => self.replace_plan(id).map_or_else(Vec::new, |plan| {
                plan.conflicts.into_iter().map(|c| c.folder).collect()
            }),
            _ => Vec::new(),
        };
        let plan = self.install_plan(bundle, &ids, &agreed, &[], channel);
        plan.steps
            .iter()
            .filter(|step| !ids.contains(&step.id) && self.managed(&step.id).is_none())
            .filter_map(|step| {
                let addon = bundle.addon(&step.id)?;
                let channel = channel(&step.id);
                Some(ListDep {
                    addon: ListAddon {
                        id: step.id.clone(),
                        name: addon.name.clone(),
                        channel,
                        size: addon.release(channel).map_or(0, |release| release.size),
                    },
                    needed_by: bundle
                        .addon(&step.with)
                        .map_or_else(|| step.with.clone(), |with| with.name.clone()),
                })
            })
            .collect()
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

/// Is this the same request as one already waiting? A second click while the first waits
/// adds nothing, so nothing gets asked twice.
fn same_request(waiting: &Action, action: &Action) -> bool {
    match (waiting, action) {
        (Action::InstallFromPage(a), Action::InstallFromPage(b)) => a == b,
        (Action::Install(a), Action::Install(b)) => a == b,
        (Action::UpdateAll, Action::UpdateAll) => true,
        _ => false,
    }
}

/// Why an installed addon whose entry has become a different addon isn't touched, in the
/// words its page uses. `yours` is what the player has, `listed` what the catalog has now.
fn another_addon_line(yours: &str, listed: &str) -> String {
    format!(
        "{yours} doesn't get updates: the catalog now lists a different addon in its place, \
         {listed}."
    )
}

fn verb_for(record: Option<&ManagedAddon>, release: &Release) -> Verb {
    match record {
        None => Verb::Install,
        Some(m) if model::same_version(&m.version, &release.version) => Verb::Reinstall,
        Some(_) => Verb::Update,
    }
}

/// Why a required addon wasn't added, and where the player can read who made it.
fn unreviewed_line(bundle: &CatalogBundle, left: &Unreviewed) -> String {
    let named = |id: &str, fallback: &str| {
        bundle
            .addon(id)
            .map_or_else(|| model::clean_line(fallback, 60), |a| a.name.clone())
    };
    let needed_by = named(&left.needed_by, &left.needed_by);
    let provider = named(&left.provider, &left.folder);
    format!(
        "{needed_by} requires {provider}, which wasn't added because nobody has reviewed it. \
         Click it under Requires on the {needed_by} page to see who made it, and install it \
         yourself."
    )
}

/// Why a required pack with a big download wasn't added, and how big it is, so getting it is
/// the player's own choice. The addon that requires it won't load until they do. A pack the
/// catalog gives no size for could be any size, and the line says so rather than "1 KB".
fn too_big_line(bundle: &CatalogBundle, left: &TooBig) -> String {
    let named = |id: &str, fallback: &str| {
        bundle
            .addon(id)
            .map_or_else(|| model::clean_line(fallback, 60), |a| a.name.clone())
    };
    let download = if left.size > 0 {
        format!("a {} download", model::size_text(left.size))
    } else {
        "a download of unknown size".to_owned()
    };
    format!(
        "{} won't load without {}, {download}. Install it from its page.",
        named(&left.needed_by, &left.needed_by),
        named(&left.provider, &left.folder),
    )
}

fn conflict_names(conflicts: &[FolderConflict]) -> String {
    let folders: Vec<&str> = conflicts.iter().map(|c| c.folder.as_str()).collect();
    ops::short_list(&folders)
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
