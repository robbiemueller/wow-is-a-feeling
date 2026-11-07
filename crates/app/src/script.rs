//! `--script <file>`: drive a real run from a text file, for end-to-end tests against a
//! fake install. One command per line, each waiting for the one before to finish:
//!
//! ```text
//! wait              # until the game, catalog and every operation have settled
//! install <id>      # the page's Install button, whatever it says today
//! replace           # Replace them, on the popup that can open next
//! packs both        # Install both (or Install them too), on the packs popup that can open next
//! packs just        # Just <addon>, on that popup: the addon without its packs
//! rollback <id>
//! remove <id>
//! adopt <id>
//! zip <path>
//! list <path>       # paste the list in this file and press Install these, which shows the list
//! confirmlist       # Install these, on that list, which logs what the list showed first
//! updateall
//! select <id>
//! tab my|browse|settings
//! shot <path>
//! sleep <ms>
//! quit
//! ```
//!
//! A log of what happened lands beside the script, as `<file>.log`.

use crate::actions::Action;
use crate::app::App;
use crate::model::{Selection, Tab};
use eframe::egui::ViewportCommand;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WAIT_LIMIT: Duration = Duration::from_secs(120);

pub struct Script {
    lines: VecDeque<String>,
    log_path: PathBuf,
    /// When the current wait began.
    waiting_since: Option<Instant>,
    sleeping_until: Option<Instant>,
}

impl Script {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self {
            lines: text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect(),
            log_path: path.with_extension("log"),
            waiting_since: None,
            sleeping_until: None,
        })
    }

    fn log(&self, line: &str) {
        use std::io::Write as _;
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path);
        if let Ok(mut file) = opened {
            let _ = writeln!(file, "{line}");
        }
    }

    pub fn tick(&mut self, app: &mut App, now: Instant) {
        if let Some(until) = self.sleeping_until {
            if now < until {
                return;
            }
            self.sleeping_until = None;
        }
        let Some(line) = self.lines.front().cloned() else {
            return;
        };
        let (command, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        if command == "wait" {
            let since = *self.waiting_since.get_or_insert(now);
            if !app.is_settled() {
                if now.duration_since(since) > WAIT_LIMIT {
                    self.log("wait timed out");
                    self.lines.clear();
                    app.ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                return;
            }
            self.waiting_since = None;
            self.log(&describe(app));
            self.lines.pop_front();
            return;
        }
        self.lines.pop_front();
        self.log(&format!("> {line}"));
        match command {
            "install" => app.actions.push(Action::InstallFromPage(rest.to_owned())),
            "replace" => {
                if let Some(crate::dialogs::Popup::Replace(plan)) = app.popup.take() {
                    app.actions.push(plan.confirmed());
                }
            }
            "packs" => match (rest, app.popup.take()) {
                ("both", Some(crate::dialogs::Popup::Packs(plan))) => {
                    app.actions.push(plan.with_packs());
                }
                ("just", Some(crate::dialogs::Popup::Packs(plan))) => {
                    app.actions.push(plan.without_packs());
                }
                (_, other) => {
                    app.popup = other;
                    self.log("no packs are being offered, or no answer to give");
                }
            },
            "rollback" => app.actions.push(Action::Rollback(rest.to_owned())),
            "remove" => {
                app.ask_remove(&Selection::Addon(rest.to_owned()));
                if let Some(crate::dialogs::Popup::Remove(plan)) = app.popup.take() {
                    app.actions.push(Action::ConfirmRemove(plan));
                }
            }
            "adopt" => app.actions.push(Action::Adopt(rest.to_owned())),
            "zip" => app.actions.push(Action::ZipDropped(PathBuf::from(rest))),
            "list" => match std::fs::read_to_string(rest) {
                Ok(text) => {
                    app.list_text = text;
                    app.actions.push(Action::PreviewList);
                }
                Err(_) => self.log("couldn't read that list"),
            },
            "confirmlist" => match app.popup.take() {
                Some(crate::dialogs::Popup::List(preview)) => {
                    self.log(&describe_list(&preview));
                    app.actions.push(Action::ConfirmList(preview.add));
                }
                other => {
                    app.popup = other;
                    self.log("no list is being shown");
                }
            },
            "updateall" => app.actions.push(Action::UpdateAll),
            "checkapp" => app.actions.push(Action::CheckUpdateNow),
            // What the update prompt's main button does, for a rig testing a real update.
            "updateapp" => match app.popup.take() {
                Some(crate::dialogs::Popup::AppUpdate(info)) => {
                    app.actions.push(Action::UpdateNow(info));
                }
                other => {
                    app.popup = other;
                    self.log("no update is being offered");
                }
            },
            "select" => app
                .actions
                .push(Action::Select(Selection::Addon(rest.to_owned()))),
            "tab" => {
                let tab = match rest {
                    "my" => Tab::MyAddons,
                    "settings" => Tab::Settings,
                    _ => Tab::Browse,
                };
                app.actions.push(Action::SetTab(tab));
            }
            "shot" => crate::save_screenshot(&app.ctx, PathBuf::from(rest), false),
            "sleep" => {
                let ms = rest.parse().unwrap_or(100);
                self.sleeping_until = Some(now + Duration::from_millis(ms));
            }
            "quit" => app.ctx.send_viewport_cmd(ViewportCommand::Close),
            other => self.log(&format!("unknown command: {other}")),
        }
    }
}

/// What a list's popup showed, one line for each part of it.
fn describe_list(preview: &crate::dialogs::ListPreview) -> String {
    let mut lines: Vec<String> = vec![format!("list: {} to add", preview.count())];
    lines.extend(
        preview
            .add
            .iter()
            .map(|addon| format!("  {}", addon.line())),
    );
    lines.extend(preview.brings.iter().map(|dep| format!("  {}", dep.line())));
    lines.extend(
        preview
            .left_alone
            .iter()
            .map(|name| format!("  {}", crate::dialogs::left_alone_line(name))),
    );
    if let Some(total) = preview.total() {
        lines.push(format!("  {total}"));
    }
    lines.join("\n")
}

/// One line about what the app believes right now, for a test to compare with the disk.
fn describe(app: &App) -> String {
    let mut out = String::from("state:");
    if let Some(managed) = app.managed_map() {
        for record in managed.values() {
            let _ = write!(
                out,
                " {}={}[{}]{}",
                record.id,
                record.version,
                record.folders.join(","),
                record
                    .previous_version
                    .as_deref()
                    .map_or(String::new(), |v| format!("<-{v}")),
            );
        }
    }
    let _ = write!(out, " | updates={}", app.updates.len());
    let _ = write!(out, " | others={}", app.groups.len());
    let _ = write!(out, " | status={}", app.status_text());
    out
}
