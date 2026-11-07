//! The Settings tab: quest log layout again, with sections at left.

use crate::actions::Action;
use crate::app::App;
use crate::easter;
use crate::model::{self, GameState, SECTIONS, Section};
use crate::skin;
use crate::theme;
use crate::views::page::{self, BarButton, body, framed};
use crate::widgets::{self, ButtonKind};
use eframe::egui::{
    self, Color32, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};
use feeling_core::backup::BackupEntry;

const ROW_HEIGHT: f32 = 26.0;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOURCE_URL: &str = env!("CARGO_PKG_REPOSITORY");
const HOMEPAGE_URL: &str = env!("CARGO_PKG_HOMEPAGE");

pub fn list(app: &mut App, ui: &mut Ui, rect: Rect) {
    skin::well(ui.painter(), rect, theme::WELL);
    let inner = rect.shrink(3.0);
    ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (section, name) in SECTIONS {
            let (r, response) =
                ui.allocate_exact_size(vec2(inner.width(), ROW_HEIGHT), Sense::click());
            let selected = app.section == section;
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::Role::Button, true, selected, name)
            });
            let p = ui.painter_at(r);
            if selected {
                skin::v_gradient(
                    &p,
                    r,
                    Color32::from_rgb(92, 70, 22),
                    Color32::from_rgb(58, 44, 14),
                );
                p.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0, theme::GOLD_DIM),
                    StrokeKind::Inside,
                );
            } else if response.hovered() {
                p.rect_filled(r, 0.0, Color32::from_white_alpha(14));
            }
            skin::shadow_text(
                &p,
                pos2(r.min.x + 14.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                name,
                theme::label(14.5),
                if selected { theme::GOLD } else { theme::TEXT },
            );
            if response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                app.actions.push(Action::SetSection(section));
            }
        }
    });
}

pub fn page(app: &mut App, ui: &mut Ui, rect: Rect) {
    page::paper(app, ui, rect);
    match app.section {
        Section::Game => game_page(app, ui, rect),
        Section::Backups => backups_page(app, ui, rect),
        Section::Lists => lists_page(app, ui, rect),
        Section::Display => display_page(app, ui, rect),
        Section::Updates => updates_page(app, ui, rect),
        Section::About => about_page(app, ui, rect),
    }
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::display(27.0))
            .color(theme::INK_HEAD),
    );
    ui.add_space(4.0);
    widgets::page_rule(ui);
    ui.add_space(8.0);
}

fn game_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    if app.game.is_none() && app.game_state == GameState::NotFound {
        page::no_game_page(app, ui, rect);
        return;
    }
    let buttons = vec![
        BarButton::main("Choose folder", true, Action::ChooseFolder),
        BarButton::quiet("Look again", true, Action::LookAgain),
    ];
    let game = app.game.clone();
    let problem = app.write_problem.clone();
    framed(app, ui, rect, buttons, Vec::new(), |_, ui| {
        heading(ui, "Game folder");
        let Some(game) = &game else {
            body(ui, "Looking for WoW Forever...");
            return;
        };
        widgets::fact(ui, "Folder:", &game.shown_path);
        widgets::fact(
            ui,
            "Version:",
            game.install.version.as_deref().unwrap_or("Couldn't tell"),
        );
        widgets::fact(ui, "Found:", game.how.describe());
        if let Some(problem) = &problem {
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    RichText::new(problem)
                        .font(theme::strong(14.5))
                        .color(theme::INK_BAD),
                )
                .wrap(),
            );
        }
    });
}

fn backups_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let has_game = app.game.is_some();
    let busy = app.queue.busy_exclusive();
    let buttons = vec![BarButton::main(
        "Back up now",
        has_game && !busy,
        Action::BackupNow,
    )];
    let backups = app.backups.clone();
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        heading(ui, "Backups");
        body(
            ui,
            "Your addons' saved settings live in the WTF folder. A backup is a zip of that folder.",
        );
        ui.add_space(8.0);
        let on = app.state.settings.backup_wtf;
        if widgets::ink_check(ui, on, "Back up settings before updating, once a day", true)
            .clicked()
        {
            app.actions.push(Action::SetBackupWtf(!on));
        }
        let keep = app.state.settings.keep_previous_version;
        if widgets::ink_check(ui, keep, "Keep the previous version of each addon", true).clicked() {
            app.actions.push(Action::SetKeepPrevious(!keep));
        }
        ui.add_space(10.0);
        widgets::section(ui, "Saved backups");
        match &backups {
            None => {
                widgets::ink_note(ui, "Reading the backup folder...");
            }
            Some(list) if list.is_empty() => {
                widgets::ink_note(ui, "None yet.");
            }
            Some(list) => {
                for entry in list {
                    backup_row(app, ui, entry, busy);
                }
            }
        }
    });
}

fn backup_row(app: &mut App, ui: &mut Ui, entry: &BackupEntry, busy: bool) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        let when = model::date_text(entry.created);
        let size = model::size_text(entry.size);
        let text_width = (ui.available_width() - 100.0).max(40.0);
        ui.allocate_ui(vec2(text_width, 28.0), |ui| {
            ui.set_width(text_width);
            ui.horizontal_centered(|ui| {
                widgets::ink(ui, format!("{when}, {size}"), theme::strong(14.5), theme::INK);
            });
        });
        let restore = widgets::button(ui, "Restore", ButtonKind::Quiet, !busy);
        if restore.clicked() {
            app.actions.push(Action::AskRestore(entry.clone()));
        }
    });
}

fn lists_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let has_managed = app.managed_map().is_some_and(|m| !m.is_empty());
    let buttons = vec![
        BarButton::main(
            "Install these",
            !app.list_text.trim().is_empty() && app.bundle.is_some(),
            Action::PreviewList,
        ),
        BarButton::quiet("Copy my list", has_managed, Action::CopyList),
    ];
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        heading(ui, "Addon lists");
        body(
            ui,
            "Copy your list to share your setup. Paste someone's list here to see what it would add before anything happens.",
        );
        ui.add_space(8.0);
        let height = (ui.available_height() - 8.0).clamp(80.0, 260.0);
        let width = ui.available_width();
        let edit = TextEdit::multiline(&mut app.list_text)
            .id_salt("list-text")
            .hint_text(
                RichText::new("Paste a list here")
                    .color(theme::HINT)
                    .font(theme::body(14.0)),
            )
            .char_limit(64 * 1024);
        widgets::well_edit(ui, edit, vec2(width, height));
    });
}

fn display_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    framed(app, ui, rect, Vec::new(), Vec::new(), |app, ui| {
        heading(ui, "Display");
        widgets::section(ui, "UI scale");
        let percent = (app.scale_draft * 100.0).round() as i32;
        ui.horizontal(|ui| {
            ui.style_mut().spacing.slider_width = (ui.available_width() - 70.0).clamp(80.0, 320.0);
            ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_rgb(60, 40, 16);
            let slider = egui::Slider::new(&mut app.scale_draft, 0.8..=2.0)
                .step_by(0.05)
                .show_value(false)
                .text("");
            let response = ui.add(slider);
            response.widget_info(|| {
                egui::WidgetInfo::slider(true, f64::from(app.scale_draft), "UI scale")
            });
            widgets::ink(ui, format!("{percent}%"), theme::strong(15.0), theme::INK);
            // Changing the scale moves the slider under the pointer, so it waits for the
            // drag to end. Keyboard and accessibility changes apply at once.
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                app.actions.push(Action::SetScale(app.scale_draft));
            }
        });
        ui.add_space(8.0);
        widgets::ink_note(ui, "Makes everything larger or smaller. It's remembered for next time.");
    });
}

fn updates_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let buttons = vec![BarButton::main("Check now", true, Action::CheckUpdateNow)];
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        heading(ui, "App updates");
        widgets::fact(ui, "Version:", VERSION);
        ui.add_space(8.0);
        let on = app.state.settings.check_app_updates;
        if widgets::ink_check(ui, on, "Check for a new version at launch", true).clicked() {
            app.actions.push(Action::SetCheckUpdates(!on));
        }
        ui.add_space(8.0);
        widgets::ink_note(
            ui,
            "Updates are signed, and nothing installs until you say so. The app only checks once per launch.",
        );
    });
}

fn about_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let buttons = vec![BarButton::quiet(
        "Copy diagnostics",
        true,
        Action::CopyDiagnostics,
    )];
    let logo = app.skin.logo.id();
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::hover());
            ui.painter().image(
                logo,
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.label(
                RichText::new("WoW is a Feeling")
                    .font(theme::display(28.0))
                    .color(theme::INK_HEAD),
            );
            widgets::ink(
                ui,
                format!("Version {VERSION}"),
                theme::strong(14.5),
                theme::INK_SOFT,
            );
        });
        ui.add_space(4.0);
        widgets::page_rule(ui);
        ui.add_space(8.0);
        credit(ui);
        let watch = widgets::ink_link(ui, "Watch the video", theme::strong(15.0));
        if watch.clicked() {
            app.actions
                .push(Action::OpenLink(easter::VIDEO_URL.to_owned()));
        }
        ui.add_space(8.0);
        body(
            ui,
            "World of Warcraft is a trademark of Blizzard Entertainment, Inc. This project is not affiliated with or endorsed by Blizzard Entertainment.",
        );
        ui.add_space(8.0);
        body(
            ui,
            "Free software under the GPL-3.0. Fonts are under the SIL Open Font License.",
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if widgets::ink_link(ui, "Source code", theme::strong(15.0)).clicked() {
                app.actions.push(Action::OpenLink(SOURCE_URL.to_owned()));
            }
            if widgets::ink_link(ui, "Website", theme::strong(15.0)).clicked() {
                app.actions.push(Action::OpenLink(HOMEPAGE_URL.to_owned()));
            }
        });
    });
}

/// The credit line, with the show's title in italics.
fn credit(ui: &mut Ui) {
    let plain = |text: &str| {
        RichText::new(text.to_owned())
            .font(theme::body(15.0))
            .color(theme::INK)
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(plain("Named after the song 'World of Warcraft is a Feeling' from "));
        ui.label(plain("Pure Pwnage").italics());
        ui.label(plain("."));
    });
}
