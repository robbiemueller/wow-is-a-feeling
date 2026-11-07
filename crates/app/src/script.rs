//! `--script <file>`: drive a real run from a text file, for end-to-end tests against a
//! fake install. One command per line, each waiting for the one before to finish:
//!
//! ```text
//! wait              # until the game, catalog and every operation have settled
//! install <id>      # the Install button, whatever it says today
//! rollback <id>
//! remove <id>
//! adopt <id>
//! zip <path>
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
            "install" => app.actions.push(Action::Install(vec![rest.to_owned()])),
            "rollback" => app.actions.push(Action::Rollback(rest.to_owned())),
            "remove" => {
                app.ask_remove(&Selection::Addon(rest.to_owned()));
                if let Some(crate::dialogs::Popup::Remove(plan)) = app.popup.take() {
                    app.actions.push(Action::ConfirmRemove(plan));
                }
            }
            "adopt" => app.actions.push(Action::Adopt(rest.to_owned())),
            "zip" => app.actions.push(Action::ZipDropped(PathBuf::from(rest))),
            "updateall" => app.actions.push(Action::UpdateAll),
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
