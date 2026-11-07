//! Static popups: dark fill, gold border, near the top, yellow text, red buttons. One at
//! a time, and Escape cancels.

use crate::actions::Action;
use crate::app::App;
use crate::ops::{FolderConflict, Verb};
use crate::theme;
use crate::widgets::{self, ButtonKind};
use crate::worker::{DirListing, FoundGame};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Key, Margin, Modal, Modifiers, RichText,
    ScrollArea, Shadow, Stroke, TextEdit, Ui, UiBuilder, vec2,
};
use feeling_core::catalog::Channel;
use feeling_core::selfupdate::UpdateInfo;
use std::path::{Path, PathBuf};

/// What a removal will touch, worked out when the popup opens. What it really deletes is
/// worked out again when it starts, in case an install queued ahead of it shares a folder.
#[derive(Clone, Debug)]
pub struct RemovePlan {
    /// The managed record that goes away with the folders, if there is one.
    pub addon_id: Option<String>,
    pub name: String,
    pub folders: Vec<String>,
    pub dependents: Vec<String>,
}

/// What an install would take from something else, worked out when the popup opens.
#[derive(Clone, Debug)]
pub struct ReplacePlan {
    pub addon_id: String,
    pub name: String,
    /// Who it's from, because a display name isn't unique and this is.
    pub source: String,
    pub verb: Verb,
    pub conflicts: Vec<FolderConflict>,
}

impl ReplacePlan {
    /// Saying yes to this.
    pub fn confirmed(&self) -> Action {
        Action::ReplaceAndInstall(
            self.addon_id.clone(),
            self.conflicts.iter().map(|c| c.folder.clone()).collect(),
        )
    }
}

/// What a pasted list would do.
#[derive(Clone, Debug, Default)]
pub struct ListPreview {
    /// `(id, name, channel)`.
    pub add: Vec<(String, String, Channel)>,
    pub have: Vec<String>,
    pub unknown: Vec<String>,
}

pub enum Popup {
    AlreadyRunning,
    Remove(RemovePlan),
    Replace(ReplacePlan),
    AppUpdate(Box<UpdateInfo>),
    Restore { zip: PathBuf, when: String },
    Folder(Box<FolderBrowser>),
    List(ListPreview),
}

/// The built-in folder browser. Everything it knows came from a worker.
#[derive(Default)]
pub struct FolderBrowser {
    /// What's in the path line, which the player can type over.
    pub typed: String,
    pub listing: Option<DirListing>,
    pub waiting_for: Option<PathBuf>,
}

impl FolderBrowser {
    pub fn usable(&self) -> Option<&FoundGame> {
        self.listing.as_ref()?.verdict.as_ref()?.as_ref().ok()
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(popup) = app.popup.take() else {
        return;
    };
    // There's nothing behind this one to go back to.
    let escape = !matches!(popup, Popup::AlreadyRunning)
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let mut keep = !escape;
    let width: f32 = match &popup {
        Popup::Folder(_) => 560.0,
        Popup::AppUpdate(_) | Popup::List(_) | Popup::Replace(_) => 460.0,
        _ => 400.0,
    };
    let mut popup = popup;
    let frame = Frame::new()
        .fill(Color32::from_rgba_premultiplied(12, 10, 8, 250))
        .stroke(Stroke::new(2.0, theme::GOLD_DIM))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(16))
        .shadow(Shadow {
            offset: [0, 4],
            blur: 18,
            spread: 2,
            color: Color32::from_black_alpha(180),
        });
    let id = Id::unique("static-popup");
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, 92.0));
    Modal::new(id)
        .area(area)
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(110))
        .show(ctx, |ui| {
            let screen = ui.ctx().content_rect();
            ui.set_width(width.min(screen.width() - 72.0));
            match &mut popup {
                Popup::AlreadyRunning => already_running(ui, ctx),
                Popup::Remove(plan) => remove(ui, app, plan, &mut keep),
                Popup::Replace(plan) => replace(ui, app, plan, &mut keep),
                Popup::AppUpdate(info) => app_update(ui, app, info, &mut keep),
                Popup::Restore { zip, when } => restore(ui, app, zip, when, &mut keep),
                Popup::Folder(browser) => folder(ui, app, browser, &mut keep),
                Popup::List(preview) => list(ui, app, preview, &mut keep),
            }
        });
    if escape && matches!(popup, Popup::Folder(_)) {
        app.actions.push(Action::CancelFolder);
    }
    // A button may have opened another one while this one was out of its slot.
    if keep {
        if let Some(opened) = app.popup.replace(popup) {
            app.show_popup(opened);
        }
    } else if app.popup.is_none() {
        app.popup = app.next_popup();
    }
    if !keep {
        ctx.request_repaint();
    }
}

fn yellow(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::strong(15.5))
                .color(theme::SYSTEM_YELLOW),
        )
        .wrap(),
    );
}

fn plain(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::body(14.0))
                .color(theme::TEXT),
        )
        .wrap(),
    );
}

/// A row of buttons, centered, the way static popups always had them.
fn button_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
            add(ui);
        });
    });
}

fn already_running(ui: &mut Ui, ctx: &egui::Context) {
    yellow(ui, "WoW is a Feeling is already running.");
    ui.add_space(4.0);
    plain(
        ui,
        "Look for its window. Only one can run at a time, so nothing gets installed twice.",
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Close", ButtonKind::Main, true).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

fn remove(ui: &mut Ui, app: &mut App, plan: &RemovePlan, keep: &mut bool) {
    yellow(
        ui,
        &format!(
            "Remove {}? Its folders are deleted. Your saved settings in WTF stay.",
            plan.name
        ),
    );
    if !plan.dependents.is_empty() {
        ui.add_space(6.0);
        plain(
            ui,
            &format!(
                "These installed addons require it and may stop working: {}.",
                plan.dependents.join(", ")
            ),
        );
    }
    if !plan.folders.is_empty() {
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!("Folders: {}", plan.folders.join(", ")))
                    .font(theme::body(13.0))
                    .color(theme::TEXT_DIM),
            )
            .wrap(),
        );
    }
    button_row(ui, |ui| {
        if widgets::button(ui, "Remove", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::ConfirmRemove(plan.clone()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

fn replace(ui: &mut Ui, app: &mut App, plan: &ReplacePlan, keep: &mut bool) {
    yellow(
        ui,
        &format!(
            "{} {} would replace these folders already in your AddOns:",
            plan.verb.gerund(),
            plan.name
        ),
    );
    ui.add_space(6.0);
    ScrollArea::vertical()
        .id_salt("replace-list")
        .max_height(160.0)
        .show(ui, |ui| {
            // A real addon never gets near this. A strange catalog entry could.
            const MOST_SHOWN: usize = 40;
            for conflict in plan.conflicts.iter().take(MOST_SHOWN) {
                plain(ui, &conflict.describe());
            }
            if plan.conflicts.len() > MOST_SHOWN {
                plain(
                    ui,
                    &format!("and {} more", plan.conflicts.len() - MOST_SHOWN),
                );
            }
        });
    ui.add_space(6.0);
    plain(
        ui,
        "What's in them now gets deleted, and anything that needs them may stop working until you install it again. Your saved settings in WTF stay.",
    );
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new(format!("Source: {}", plan.source))
                .font(theme::body(13.0))
                .color(theme::TEXT_DIM),
        )
        .wrap(),
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Replace them", ButtonKind::Main, true).clicked() {
            app.actions.push(plan.confirmed());
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

fn app_update(ui: &mut Ui, app: &mut App, info: &UpdateInfo, keep: &mut bool) {
    yellow(ui, &format!("Version {} is available.", info.version));
    plain(
        ui,
        &format!("(You have {}.)", crate::views::settings::VERSION),
    );
    if !info.notes.is_empty() {
        ui.add_space(8.0);
        let height = (ui.ctx().content_rect().height() - 320.0).clamp(60.0, 150.0);
        sunken_list(ui, "update-notes", height, Color32::BLACK, |ui| {
            plain(ui, &crate::model::clean_paragraphs(&info.notes));
        });
    }
    button_row(ui, |ui| {
        if widgets::button(ui, "Update now", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::UpdateNow(Box::new(info.clone())));
            *keep = false;
        }
        if widgets::button(ui, "Later", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
        if widgets::button(ui, "Skip this version", ButtonKind::Quiet, true).clicked() {
            app.actions.push(Action::SkipVersion(info.version.clone()));
            *keep = false;
        }
    });
}

fn restore(ui: &mut Ui, app: &mut App, zip: &Path, when: &str, keep: &mut bool) {
    yellow(ui, &format!("Restore your settings from {when}?"));
    ui.add_space(4.0);
    plain(
        ui,
        "Your current WTF folder is kept beside it under a new name. This can't run while the game is open.",
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Restore", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::ConfirmRestore(zip.to_path_buf()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

fn folder(ui: &mut Ui, app: &mut App, browser: &mut FolderBrowser, keep: &mut bool) {
    yellow(ui, "Choose the World of Warcraft folder");
    ui.add_space(6.0);

    let parent = browser.listing.as_ref().and_then(|l| l.parent.clone());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let up = widgets::button(ui, "Up", ButtonKind::Quiet, parent.is_some());
        if up.clicked()
            && let Some(parent) = &parent
        {
            app.actions.push(Action::BrowseTo(parent.clone()));
        }
        let room = (ui.available_width() - 2.0).max(60.0);
        let edit = TextEdit::singleline(&mut browser.typed)
            .id_salt("folder-path")
            .font(theme::body(14.0))
            .text_color(Color32::WHITE)
            .frame(Frame::NONE);
        let response = widgets::well_edit(ui, edit, vec2(room, 26.0));
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            app.actions
                .push(Action::BrowseTo(PathBuf::from(browser.typed.trim())));
        }
    });

    let drives = browser
        .listing
        .as_ref()
        .map(|l| l.drives.clone())
        .unwrap_or_default();
    if !drives.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for drive in drives {
                let label = drive.display().to_string();
                if drive_button(ui, &label).clicked() {
                    app.actions.push(Action::BrowseTo(drive));
                }
            }
        });
    }
    ui.add_space(6.0);

    // Room for the list without running off a small window.
    let screen = ui.ctx().content_rect().height();
    let list_height = (screen - 340.0).clamp(90.0, 280.0);
    sunken_list(ui, "folder-list", list_height, theme::WELL, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        match &browser.listing {
            None => plain(ui, "Reading..."),
            Some(listing) => {
                if let Some(error) = &listing.error {
                    plain(ui, error);
                } else if listing.folders.is_empty() {
                    plain(ui, "No folders in here.");
                }
                for name in &listing.folders {
                    if folder_row(ui, name).clicked() {
                        app.actions.push(Action::BrowseTo(listing.path.join(name)));
                    }
                }
            }
        }
    });
    ui.add_space(8.0);

    let usable = browser.usable().cloned();
    if let Some(game) = &usable {
        let version = game.install.version.as_deref().unwrap_or("unknown version");
        ui.add(
            egui::Label::new(
                RichText::new(format!("WoW Forever found here ({version})."))
                    .font(theme::strong(14.0))
                    .color(theme::USE_GREEN),
            )
            .wrap(),
        );
    } else if let Some(Some(Err(why))) = browser.listing.as_ref().map(|l| l.verdict.as_ref()) {
        ui.add(
            egui::Label::new(
                RichText::new(why.as_str())
                    .font(theme::body(13.0))
                    .color(theme::TEXT_DIM),
            )
            .wrap(),
        );
    }
    button_row(ui, |ui| {
        let use_it = widgets::button(ui, "Use this folder", ButtonKind::Main, usable.is_some());
        if use_it.clicked()
            && let Some(game) = usable
        {
            app.actions.push(Action::UseFolder(Box::new(game)));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            app.actions.push(Action::CancelFolder);
            *keep = false;
        }
    });
}

/// A sunken box of exactly this height, which scrolls whatever doesn't fit in it.
fn sunken_list(ui: &mut Ui, id: &str, height: f32, fill: Color32, content: impl FnOnce(&mut Ui)) {
    let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), height));
    crate::skin::well(ui.painter(), rect, fill);
    ui.scope_builder(
        UiBuilder::new().max_rect(rect.shrink2(vec2(6.0, 4.0))),
        |ui| {
            ScrollArea::vertical()
                .id_salt(id)
                .auto_shrink(false)
                .show(ui, content);
        },
    );
    // A scope only reserves what its content used, and this box is always the same size.
    ui.advance_cursor_after_rect(rect);
}

fn drive_button(ui: &mut Ui, label: &str) -> egui::Response {
    let size = vec2(44.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Drive {label}"))
    });
    let fill = if response.hovered() {
        Color32::from_rgb(60, 46, 30)
    } else {
        Color32::from_rgb(36, 28, 18)
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    ui.painter().rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, theme::GOLD_LINE),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::strong(13.0),
        theme::TEXT,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn folder_row(ui: &mut Ui, name: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 22.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter()
                .rect_filled(rect, 0.0, Color32::from_white_alpha(14));
        }
        crate::skin::shadow_text_fit(
            ui.painter(),
            egui::pos2(rect.min.x + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            theme::body(14.5),
            theme::TEXT,
            width - 12.0,
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn list(ui: &mut Ui, app: &mut App, preview: &ListPreview, keep: &mut bool) {
    let count = preview.add.len();
    yellow(
        ui,
        &match count {
            0 => "This list wouldn't add anything.".to_owned(),
            1 => "This list adds 1 addon:".to_owned(),
            n => format!("This list adds {n} addons:"),
        },
    );
    ui.add_space(6.0);
    ScrollArea::vertical()
        .id_salt("list-preview")
        .max_height(200.0)
        .show(ui, |ui| {
            for (_, name, channel) in &preview.add {
                let beta = if *channel == Channel::Beta {
                    " (beta)"
                } else {
                    ""
                };
                plain(ui, &format!("{name}{beta}"));
            }
            if !preview.have.is_empty() {
                ui.add_space(6.0);
                plain(
                    ui,
                    &format!("Already installed: {}.", preview.have.join(", ")),
                );
            }
            if !preview.unknown.is_empty() {
                ui.add_space(6.0);
                plain(
                    ui,
                    &format!("Not in the catalog: {}.", preview.unknown.join(", ")),
                );
            }
        });
    button_row(ui, |ui| {
        if widgets::button(ui, "Install these", ButtonKind::Main, count > 0).clicked() {
            app.actions.push(Action::ConfirmList(preview.add.clone()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}
