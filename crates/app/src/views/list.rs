//! The quest log: a flat list of same-height rows, of which only the visible ones cost
//! anything.

use crate::actions::Action;
use crate::app::App;
use crate::easter;
use crate::model::{self, FOREVER_INTERFACE, Row, Tab, Tag};
use crate::skin::{self, shadow_text, shadow_text_fit};
use crate::theme;
use crate::widgets;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Rect, Response, ScrollArea, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, pos2, vec2,
};
use feeling_core::catalog::Addon;

pub const ROW_HEIGHT: f32 = 23.0;
const INDENT: f32 = 26.0;
const FLAT_INDENT: f32 = 10.0;

pub fn show(app: &mut App, ui: &mut Ui, rect: Rect) {
    skin::well(ui.painter(), rect, theme::WELL);
    let inner = rect.shrink(3.0);
    if app.rows.is_empty() {
        empty_note(app, ui, inner);
        return;
    }
    ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
        // This has to be set before `show_rows`, which works out where each row lands from
        // the spacing it finds here.
        ui.spacing_mut().item_spacing.y = 0.0;
        let tab = app.tab;
        let mut area = ScrollArea::vertical()
            .id_salt(("quest-log", tab == Tab::Browse))
            .auto_shrink(false);
        if let Some(row) = app.pending_scroll.take() {
            let top = row as f32 * ROW_HEIGHT;
            let height = inner.height();
            let offset = if top < app.list_offset {
                top
            } else if top + ROW_HEIGHT > app.list_offset + height {
                top + ROW_HEIGHT - height
            } else {
                app.list_offset
            };
            area = area.vertical_scroll_offset(offset.max(0.0));
        }
        if std::mem::take(&mut app.list_to_top) {
            area = area.vertical_scroll_offset(0.0);
        }
        let count = app.rows.len();
        let output = area.show_rows(ui, ROW_HEIGHT, count, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for index in range {
                draw_row(app, ui, index);
            }
        });
        app.list_offset = output.state.clamped_offset().y;
    });
}

fn empty_note(app: &App, ui: &mut Ui, inner: Rect) {
    let text = app.empty_list_note();
    if text.is_empty() {
        return;
    }
    let area = inner.shrink(14.0);
    let painter = ui.painter_at(inner);
    let galley = painter.layout(text.to_owned(), theme::body(14.5), theme::TEXT_DIM, area.width());
    painter.galley(area.min, galley, theme::TEXT_DIM);
}

fn draw_row(app: &mut App, ui: &mut Ui, index: usize) {
    let row = app.rows[index].clone();
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
    match row {
        Row::Header {
            id,
            name,
            count,
            open,
        } => {
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::Role::DisclosureTriangle,
                    true,
                    open,
                    format!("{name} ({count})"),
                )
            });
            paint_header(ui, rect, &response, name, count, open);
            if response.clicked() {
                app.actions.push(Action::ToggleHeader(id));
            }
        }
        row => {
            let selected = app.is_selected_row(&row);
            let indent = if app.rows_are_flat() { FLAT_INDENT } else { INDENT };
            let view = row_view(app, &row);
            let label = if view.tag.is_empty() {
                view.name.clone()
            } else {
                format!("{}, {}", view.name, view.tag)
            };
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::Role::Button, true, selected, &label)
            });
            paint_row(ui, rect, &response, &view, selected, indent);
            let response = response
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_ui(|ui| tooltip(app, ui, &row));
            if response.clicked() {
                if let Some(selection) =
                    model::row_selection(&row, app.bundle.as_deref(), &app.groups)
                {
                    app.actions.push(Action::Select(selection));
                }
            }
        }
    }
}

fn paint_header(ui: &Ui, r: Rect, resp: &Response, name: &str, count: u32, open: bool) {
    let p = ui.painter_at(r);
    if resp.hovered() {
        p.rect_filled(r, 0.0, Color32::from_white_alpha(8));
    }
    widgets::toggle_box(&p, pos2(r.min.x + 13.0, r.center().y), open);
    shadow_text(
        &p,
        pos2(r.min.x + 26.0, r.center().y),
        Align2::LEFT_CENTER,
        name,
        theme::label(14.5),
        theme::GOLD,
    );
    shadow_text(
        &p,
        pos2(r.max.x - 8.0, r.center().y),
        Align2::RIGHT_CENTER,
        &count.to_string(),
        theme::strong(13.0),
        theme::GOLD_DIM,
    );
}

/// What a row says, worked out only for the rows on screen.
struct RowView {
    name: String,
    color: Color32,
    tag: String,
    tag_color: Color32,
}

fn tag_color(tag: &Tag<'_>) -> Color32 {
    match tag {
        Tag::Update => theme::GOLD,
        Tag::OutOfDate => theme::ERROR_RED,
        Tag::Installed => theme::INSTALLED,
        Tag::Version(_) | Tag::Nothing => theme::TEXT_DIM,
    }
}

fn row_view(app: &App, row: &Row) -> RowView {
    let tagged = |name: &str, color, tag: Tag<'_>| RowView {
        name: name.to_owned(),
        color,
        tag: tag.words().to_owned(),
        tag_color: tag_color(&tag),
    };
    let bundle = app.bundle.as_deref();
    match row {
        Row::Addon(i) => {
            let Some(addon) = bundle.and_then(|b| b.catalog.addons.get(*i as usize)) else {
                return tagged("", theme::TEXT, Tag::Nothing);
            };
            let record = app.managed(&addon.id);
            tagged(
                &addon.name,
                theme::tier_color(addon.tier),
                model::browse_tag(addon, record),
            )
        }
        Row::Managed(id) => {
            let Some(record) = app.managed(id) else {
                return tagged(id, theme::TEXT, Tag::Nothing);
            };
            let addon = bundle.and_then(|b| b.addon(id));
            let color = addon.map_or(theme::tier_color(1), |a| theme::tier_color(a.tier));
            tagged(
                &record.name,
                color,
                model::managed_tag(record, addon, app.targets_forever(record)),
            )
        }
        Row::Other(i) => {
            let Some(group) = app.groups.get(*i as usize) else {
                return tagged("", theme::TEXT, Tag::Nothing);
            };
            let color = if group.targets_forever {
                theme::tier_color(1)
            } else {
                theme::tier_color(0)
            };
            tagged(&group.title, color, model::other_tag(group))
        }
        Row::Kit(i) => {
            let kit = bundle.and_then(|b| b.catalog.kits.get(*i as usize));
            let count = kit.map_or(0, |k| k.addons.len());
            RowView {
                name: kit.map_or(String::new(), |k| k.name.clone()),
                color: theme::GOLD_DIM,
                tag: format!("{count} addons"),
                tag_color: theme::TEXT_DIM,
            }
        }
        Row::Gap(i) => {
            let gap = bundle.and_then(|b| b.catalog.gaps.get(*i as usize));
            RowView {
                name: gap.map_or(String::new(), |g| g.name.clone()),
                color: theme::tier_color(0),
                tag: "not here".to_owned(),
                tag_color: theme::TEXT_DIM,
            }
        }
        Row::Header { .. } => tagged("", theme::TEXT, Tag::Nothing),
    }
}

fn paint_row(ui: &Ui, r: Rect, resp: &Response, view: &RowView, selected: bool, indent: f32) {
    if !ui.is_rect_visible(r) {
        return;
    }
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
    } else if resp.hovered() {
        p.rect_filled(r, 0.0, Color32::from_white_alpha(14));
    }
    if resp.has_focus() {
        p.rect_stroke(
            r.shrink(1.0),
            0.0,
            Stroke::new(1.0, Color32::WHITE),
            StrokeKind::Inside,
        );
    }
    let tag_right = pos2(r.max.x - 8.0, r.center().y);
    let tag_rect = shadow_text_fit(
        &p,
        tag_right,
        Align2::RIGHT_CENTER,
        &view.tag,
        theme::strong(13.0),
        view.tag_color,
        r.width() * 0.45,
    );
    let name_room = tag_rect.min.x - r.min.x - indent - 8.0;
    shadow_text_fit(
        &p,
        pos2(r.min.x + indent, r.center().y),
        Align2::LEFT_CENTER,
        &view.name,
        theme::body(15.0),
        view.color,
        name_room,
    );
}

fn tooltip(app: &App, ui: &mut Ui, row: &Row) {
    ui.set_max_width(270.0);
    let bundle = app.bundle.as_deref();
    match row {
        Row::Addon(i) => {
            if let Some(addon) = bundle.and_then(|b| b.catalog.addons.get(*i as usize)) {
                addon_tooltip(ui, addon);
            }
        }
        Row::Managed(id) => match bundle.and_then(|b| b.addon(id)) {
            Some(addon) => addon_tooltip(ui, addon),
            None => {
                if let Some(record) = app.managed(id) {
                    widgets::tooltip_title(ui, &record.name, theme::tier_color(1));
                    widgets::tooltip_line(ui, "Installed from a zip file");
                }
            }
        },
        Row::Other(i) => {
            if let Some(group) = app.groups.get(*i as usize) {
                let color = theme::tier_color(u8::from(group.targets_forever));
                widgets::tooltip_title(ui, &group.title, color);
                widgets::tooltip_line(ui, "This addon wasn't installed by this app.");
                if let Some(notes) = app.toc_notes(&group.primary) {
                    widgets::tooltip_colored(ui, &format!("Use: {notes}"), theme::USE_GREEN);
                }
            }
        }
        Row::Kit(i) => {
            if let Some(kit) = bundle.and_then(|b| b.catalog.kits.get(*i as usize)) {
                widgets::tooltip_title(ui, &kit.name, theme::GOLD_DIM);
                widgets::tooltip_colored(ui, &kit.description, theme::USE_GREEN);
            }
        }
        Row::Gap(i) => {
            if let Some(gap) = bundle.and_then(|b| b.catalog.gaps.get(*i as usize)) {
                widgets::tooltip_title(ui, &gap.name, theme::tier_color(0));
                widgets::tooltip_line(ui, &gap.reason);
            }
        }
        Row::Header { .. } => {}
    }
}

fn addon_tooltip(ui: &mut Ui, addon: &Addon) {
    widgets::tooltip_title(ui, &addon.name, theme::tier_color(addon.tier));
    widgets::tooltip_line(ui, "Binds when installed");
    widgets::tooltip_line(ui, &format!("Requires Interface {FOREVER_INTERFACE}"));
    let size = addon
        .stable
        .as_ref()
        .or(addon.beta.as_ref())
        .map_or(0, |release| release.size);
    if size > 0 {
        widgets::tooltip_line(ui, &format!("Size: {}", easter::coins(size)));
    }
    if !addon.summary.is_empty() {
        widgets::tooltip_colored(ui, &format!("Use: {}", addon.summary), theme::USE_GREEN);
    }
    widgets::tooltip_colored(
        ui,
        &format!("\"{}\"", easter::flavor_line(&addon.id)),
        theme::FLAVOR,
    );
}
