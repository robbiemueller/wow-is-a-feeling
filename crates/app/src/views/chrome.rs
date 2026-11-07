//! The frame around everything: title, window buttons, casting strip, tabs, status text.

use crate::actions::Action;
use crate::app::App;
use crate::messages::strip_line;
use crate::model::Tab;
use crate::skin::{self, shadow_text, shadow_text_fit};
use crate::theme;
use crate::widgets::{self, Glyph};
use eframe::egui::{
    Align2, Color32, CursorIcon, Id, LayerId, Order, Rect, ResizeDirection, Sense, Ui,
    ViewportCommand, pos2, vec2,
};
use std::time::Instant;

const TITLE_BAND: f32 = 46.0;
const PORTRAIT_RADIUS: f32 = 44.0;

pub fn window(app: &mut App, ui: &mut Ui, full: Rect) {
    let ctx = ui.ctx().clone();
    let painter = ui.painter().clone();
    app.skin.stone(&painter, full);

    // A darker band behind the title, so the gold reads against the stone.
    let band = Rect::from_min_max(full.min, pos2(full.max.x, full.min.y + TITLE_BAND));
    skin::v_gradient(
        &painter,
        band,
        Color32::from_black_alpha(150),
        Color32::from_black_alpha(20),
    );
    skin::rule(
        &painter,
        pos2(full.min.x + 8.0, band.max.y),
        pos2(full.max.x - 8.0, band.max.y),
    );

    // Drag the title bar to move the window; double click to maximize.
    let drag_rect = Rect::from_min_max(
        pos2(full.min.x + 88.0, full.min.y),
        pos2(full.max.x - 70.0, band.max.y),
    );
    let drag = ui.interact(
        drag_rect,
        ui.scope_id().with("title-drag"),
        Sense::click_and_drag(),
    );
    if drag.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if drag.double_clicked() {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }

    shadow_text(
        &painter,
        pos2(full.min.x + 94.0, band.center().y - 1.0),
        Align2::LEFT_CENTER,
        "WoW is a Feeling",
        theme::display(23.0),
        theme::GOLD,
    );

    let close = Rect::from_min_size(pos2(full.max.x - 36.0, full.min.y + 14.0), vec2(20.0, 20.0));
    if widgets::chrome_button(ui, close, "close", Glyph::Close, "Close").clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
    let minimize = close.translate(vec2(-25.0, 0.0));
    if widgets::chrome_button(ui, minimize, "minimize", Glyph::Minimize, "Minimize").clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
}

pub fn resize_grips(ui: &mut Ui, full: Rect) {
    let ctx = ui.ctx().clone();
    let t = 5.0;
    let c = 14.0;
    let (l, r, top, b) = (full.min.x, full.max.x, full.min.y, full.max.y);
    let grips = [
        (
            Rect::from_min_max(pos2(l, top), pos2(l + c, top + t)),
            ResizeDirection::NorthWest,
            CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_max(pos2(r - c, top), pos2(r, top + t)),
            ResizeDirection::NorthEast,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(pos2(l, b - c), pos2(l + c, b)),
            ResizeDirection::SouthWest,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(pos2(r - c, b - c), pos2(r, b)),
            ResizeDirection::SouthEast,
            CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_max(pos2(l + c, top), pos2(r - c, top + t)),
            ResizeDirection::North,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(pos2(l + c, b - t), pos2(r - c, b)),
            ResizeDirection::South,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(pos2(l, top + c), pos2(l + t, b - c)),
            ResizeDirection::West,
            CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(pos2(r - t, top + c), pos2(r, b - c)),
            ResizeDirection::East,
            CursorIcon::ResizeHorizontal,
        ),
    ];
    for (i, (rect, dir, cursor)) in grips.into_iter().enumerate() {
        let resp = ui
            .interact(rect, ui.scope_id().with(("resize", i)), Sense::drag())
            .on_hover_cursor(cursor);
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// The casting strip, the tabs and the status text.
pub fn bottom(app: &mut App, ui: &mut Ui, full: Rect, now: Instant) {
    let strip_y = full.max.y - 40.0;
    let bar_center = pos2(full.center().x, strip_y - 19.0);
    if let Some(casting) = app.casting() {
        let width = 360.0_f32.min(full.width() - 120.0);
        let bar = Rect::from_center_size(bar_center, vec2(width, 22.0));
        app.skin
            .casting_bar(ui.painter(), bar, casting.fraction, &casting.text);
        widgets::read_aloud(ui, bar, "casting", &casting.text);
        let cancel = Rect::from_min_size(
            pos2(bar.max.x + 7.0, bar.center().y - 9.0),
            vec2(18.0, 18.0),
        );
        if widgets::chrome_button(ui, cancel, "cancel-ops", Glyph::Close, "Cancel").clicked() {
            app.actions.push(Action::CancelOps);
        }
    } else if let Some((line, until)) = &app.loot
        && now < *until
    {
        strip_line(ui.painter(), bar_center, line, theme::USE_GREEN);
        let spot = Rect::from_center_size(bar_center, vec2(320.0, 20.0));
        widgets::read_aloud(ui, spot, "loot", line);
    }

    let tabs = [
        (Tab::MyAddons, "My Addons"),
        (Tab::Browse, "Browse"),
        (Tab::Settings, "Settings"),
    ];
    let mut x = full.min.x + 20.0;
    for (tab, text) in tabs {
        let r = Rect::from_min_size(pos2(x, strip_y), vec2(112.0, 28.0));
        if widgets::tab(ui, r, text, app.tab == tab).clicked() {
            app.actions.push(Action::SetTab(tab));
        }
        x += 116.0;
    }
    let status = app.status_text();
    let room = (full.max.x - 20.0 - x - 12.0).max(40.0);
    let shown = shadow_text_fit(
        ui.painter(),
        pos2(full.max.x - 20.0, strip_y + 14.0),
        Align2::RIGHT_CENTER,
        &status,
        theme::body(13.0),
        theme::TEXT_DIM,
        room,
    );
    widgets::read_aloud(ui, shown, "status", &status);
}

pub fn frame_and_portrait(app: &mut App, ui: &mut Ui, full: Rect, now: Instant) {
    app.skin.gold_frame(ui.painter(), full);
    let center = pos2(full.min.x + 50.0, full.min.y + 50.0);
    let response = app
        .fun
        .portrait(ui, &app.skin, center, PORTRAIT_RADIUS, now);
    if response.clicked() {
        app.actions.push(Action::PortraitClicked);
    }
}

/// Things painted over the whole window: fading messages, the ding, the block letters, and
/// the hint while a file hovers over the window.
pub fn overlays(app: &mut App, ui: &mut Ui, full: Rect, now: Instant) {
    let ctx = ui.ctx().clone();
    app.fun.overlays(&ctx, full, now);
    let popup_bottom = app
        .popup
        .as_ref()
        .and_then(|_| ctx.memory(|m| m.area_rect(Id::unique("static-popup"))))
        .map(|popup| popup.max.y + 4.0);
    app.messages.show(&ctx, full, popup_bottom, now);

    let hovering = app.launch.drop_overlay || ctx.input(|i| !i.raw.hovered_files.is_empty());
    if hovering {
        let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("drop")));
        let inner = full.shrink(10.0);
        painter.rect_filled(inner, 4.0, Color32::from_black_alpha(150));
        painter.rect_stroke(
            inner,
            4.0,
            eframe::egui::Stroke::new(2.0, theme::GOLD_DIM),
            eframe::egui::StrokeKind::Inside,
        );
        shadow_text(
            &painter,
            full.center(),
            Align2::CENTER_CENTER,
            "Drop a zip to install it",
            theme::display(26.0),
            theme::GOLD,
        );
    }
}
