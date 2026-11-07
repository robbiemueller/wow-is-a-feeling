//! What happens when a worker reports back.

use crate::app::{App, GAME_RUNNING, LOOT_LINE_FOR, same_main};
use crate::demo;
use crate::dialogs::Popup;
use crate::easter;
use crate::model::{self, CatalogStatus, GameState, Section, Selection, Tab};
use crate::ops::{self, Op, OpKind, Verb, folder_set};
use crate::views::settings::VERSION;
use crate::worker::{CatalogSource, Event, LocalZipFacts, OpFailure, OpOutcome};
use eframe::egui::{TextureOptions, ViewportCommand};
use feeling_core::catalog::Channel;
use feeling_core::error::Error;
use feeling_core::install::InstallOutcome;
use feeling_core::scan::LocalFolder;
use feeling_core::state::Origin;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

impl App {
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
        self.refresh();
        self.script_tick(now);
        if self.trim_when_quiet && self.is_quiet() {
            self.trim_when_quiet = false;
            self.worker.release_idle_memory();
        }
    }

    pub fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::GameFound(Some(game)) => self.use_game(game),
            Event::GameFound(None) => {
                self.game = None;
                self.game_state = GameState::NotFound;
                self.scanned = false;
                self.local_dirty = true;
                if !self.tab_chosen {
                    self.tab = Tab::Browse;
                }
                self.yellow(
                    "Couldn't find WoW Forever. Open My Addons to choose your game folder.",
                );
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
                        self.catalog_fetching = false;
                        self.catalog_status = CatalogStatus::Fresh;
                        self.state.catalog_etag = etag;
                        self.save_state();
                        self.trim_when_quiet = !self.demo;
                    }
                }
                self.selection_changed();
            }
            Event::CatalogUnchanged => {
                self.catalog_fetching = false;
                self.catalog_status = CatalogStatus::Fresh;
            }
            Event::CatalogFailed {
                offline,
                unpublished,
                text,
            } => {
                self.catalog_fetching = false;
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
            Event::Relaunch(Err(text)) => self.red(text),
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
        self.actions.append(&mut self.after_scan);
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
        // The default tab is settled once, by the first look at the disk. Installing
        // something later shouldn't pull the player away from Browse.
        if !std::mem::replace(&mut self.default_tab_set, true) && !self.tab_chosen {
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
            Err(failure) if !failure.would_replace.is_empty() => self.refused(&op, failure),
            Err(failure) => self.red(failure.text),
        }
        if self.queue.is_idle() && self.cancelled_ops > 0 {
            self.cancelled_ops = 0;
            self.yellow("Cancelled.");
        }
        let _ = now;
    }

    /// An install stopped because its zip would replace folders it may not. Either something
    /// put them there since the last look at the disk, or a zip brought a folder its earlier
    /// ones didn't. Look again, so the next press asks, and say what to do. A folder the
    /// catalog doesn't list means the zip isn't what the catalog says, and that's said too.
    fn refused(&mut self, op: &Op, failure: OpFailure) {
        let folders = failure.would_replace;
        // The player's yes covered a zip without these. One that brings them asks again.
        let mut forgot = false;
        if let Some(record) = self
            .state
            .current_mut()
            .and_then(|install| install.managed.get_mut(&op.addon_id))
        {
            let before = record.not_in_zip.len();
            record
                .not_in_zip
                .retain(|kept| !folders.iter().any(|f| ops::same_folder(f, kept)));
            forgot = record.not_in_zip.len() != before;
        }
        if forgot {
            self.save_state();
        }
        self.rescan();
        let listed = self
            .bundle
            .as_deref()
            .and_then(|b| b.addon(&op.addon_id))
            .map_or_else(Vec::new, |addon| addon.folders.clone());
        let unlisted: Vec<String> = folders
            .iter()
            .filter(|f| !listed.iter().any(|known| ops::same_folder(known, f)))
            .cloned()
            .collect();
        let text = if unlisted.is_empty() {
            ops::sitting_out_line(&op.name, op.verb != Verb::Install, &folders)
        } else {
            Error::WouldReplace { folders: unlisted }.to_string()
        };
        self.red(text);
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
            } => (
                op.addon_id.clone(),
                op.name.clone(),
                version.clone(),
                *channel,
                *origin,
            ),
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
        // A dropped zip and a rollback don't ask first, because the player chose exactly that
        // file. They still get told what went.
        let how = match &op.kind {
            OpKind::LocalZip { .. } => Some("The zip you dropped"),
            OpKind::Rollback { .. } => Some("Rolling back"),
            _ => None,
        };
        let replaced_line = how
            .zip(self.managed_map())
            .and_then(|(how, managed)| ops::replaced_line(how, &id, &outcome.replaced, managed));
        if let Some(line) = replaced_line {
            self.yellow(line);
        }
        let mut folders = outcome.folders.clone();
        folders.sort_by_key(|folder| !same_main(folder, &id));
        let listed = self
            .bundle
            .as_deref()
            .and_then(|b| b.addon(&id))
            .map_or_else(Vec::new, |addon| addon.folders.clone());
        let asked: &[String] = match &op.kind {
            OpKind::Download { may_replace, .. } => may_replace,
            _ => &[],
        };
        if let Some(addon) = self.bundle.as_deref().and_then(|b| b.addon(&id)) {
            // A folder this addon shares stays claimed while the catalog lists it, even if this
            // zip didn't bring it, or the next update would ask about it all over again. One
            // nothing else claims stays out unless the zip really brought it, because a later
            // version without it would then delete a folder that was never replaced.
            if let OpKind::Download { may_replace, .. } = &op.kind
                && let Some(managed) = self.managed_map()
            {
                let listed = folder_set(&addon.folders);
                let mut have = folder_set(&folders);
                for folder in may_replace {
                    let lower = folder.to_lowercase();
                    if listed.contains(&lower)
                        && ops::claimed_by_another(managed, &id, &lower)
                        && have.insert(lower)
                    {
                        folders.push(folder.clone());
                    }
                }
            }
            // Main folder first, the order the catalog lists them in.
            folders.sort_by_key(|folder| ops::listed_position(&addon.folders, folder));
        }
        if self.demo {
            self.demo_add_folders(&folders, &name, &version);
        }
        let Some(record) = self.state.current_mut() else {
            return;
        };
        let prev = record.managed.get(&id).cloned();
        // The demo keeps no zips, so it pretends the previous version's is there.
        let pretend_kept;
        let kept = if self.demo {
            pretend_kept = demo::kept_zips(&version, prev.as_ref());
            pretend_kept.as_slice()
        } else {
            kept
        };
        // What the player let this replace and the zip didn't bring stays theirs, and the
        // next update doesn't ask about it all over again.
        let not_in_zip = ops::not_in_zip(prev.as_ref(), &listed, asked, &folders);
        let new_record = ops::record_after_install(
            prev.as_ref(),
            ops::Installed {
                id: &id,
                name: &name,
                version: &version,
                channel,
                origin,
                folders: folders.clone(),
                not_in_zip,
                kept,
                keep_previous,
                now,
            },
        );
        // Only what the zip really brought replaced anything. A folder kept in the record
        // because another addon claims it is still that addon's too.
        for stale in ops::stale_records(&record.managed, &id, origin, &outcome.folders) {
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
                .retain(|f| !ops::same_folder(&f.name, folder));
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
            let gone = |f: &LocalFolder| folders.iter().any(|g| ops::same_folder(g, &f.name));
            self.demo_disk.retain(|f| !gone(f));
            // Known already, so the rows don't flash an "other folder" until the rescan.
            self.folders.retain(|f| !gone(f));
            self.index_folders();
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
        self.loot = Some((
            format!("Removed {}.", op.name),
            Instant::now() + LOOT_LINE_FOR,
        ));
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
}
