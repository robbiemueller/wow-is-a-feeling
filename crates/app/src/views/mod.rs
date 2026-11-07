//! The window, laid out in the order it's painted.

pub mod chrome;
pub mod list;
pub mod page;
pub mod settings;
pub mod toolbar;

use crate::app::App;
use crate::model::Tab;
use eframe::egui::{Rect, Ui, pos2};
use std::time::Instant;

pub fn draw(app: &mut App, ui: &mut Ui) {
    let full = ui.max_rect();
    let now = Instant::now();
    chrome::window(app, ui, full);
    toolbar::show(app, ui, full);

    let top = full.min.y + 94.0;
    let bottom = (full.max.y - 74.0).max(top + 40.0);
    let split = full.min.x + (full.width() * 0.42).clamp(220.0, 460.0);
    let log = Rect::from_min_max(pos2(full.min.x + 20.0, top), pos2(split - 4.0, bottom));
    let parchment = Rect::from_min_max(pos2(split + 4.0, top), pos2(full.max.x - 20.0, bottom));
    if app.tab == Tab::Settings {
        settings::list(app, ui, log);
        settings::page(app, ui, parchment);
    } else {
        list::show(app, ui, log);
        page::show(app, ui, parchment);
    }
    chrome::bottom(app, ui, full, now);
    chrome::frame_and_portrait(app, ui, full, now);
    chrome::resize_grips(ui, full);
    chrome::overlays(app, ui, full, now);
    crate::dialogs::show(app, ui.ctx());
}
