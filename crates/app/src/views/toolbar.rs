//! Search box, sort dropdown and **Update all**.

use crate::actions::Action;
use crate::app::App;
use crate::model::Tab;
use crate::skin::{self, shadow_text};
use crate::theme;
use crate::widgets::{self, ButtonKind};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Frame, Margin, Popup, Rect, RichText, Sense, Stroke,
    TextEdit, Ui, pos2, vec2,
};
use feeling_core::search::SortBy;

pub const SORTS: [(SortBy, &str); 3] = [
    (SortBy::Popular, "Most popular"),
    (SortBy::Updated, "Recently updated"),
    (SortBy::Name, "A to Z"),
];

pub fn sort_label(sort: SortBy) -> &'static str {
    SORTS
        .iter()
        .find(|(s, _)| *s == sort)
        .map_or(SORTS[0].1, |(_, label)| label)
}

pub fn search_id() -> egui::Id {
    egui::Id::unique("search-box")
}

pub fn show(app: &mut App, ui: &mut Ui, full: Rect) {
    let y = full.min.y + 56.0;
    let h = 28.0;
    let left = full.min.x + 102.0;
    let count = app.updates.len();
    let update_text = format!("Update all ({count})");
    let update_width = widgets::button_width(ui, &update_text).max(116.0);
    let update = Rect::from_min_size(
        pos2(full.max.x - 20.0 - update_width, y + 1.0),
        vec2(update_width, 26.0),
    );
    let mut search_right = update.min.x - 10.0;
    if app.tab == Tab::Browse {
        let sort_width = 150.0_f32.min((update.min.x - left) * 0.4);
        let sort = Rect::from_min_size(
            pos2(update.min.x - 10.0 - sort_width, y + 1.0),
            vec2(sort_width, 26.0),
        );
        sort_dropdown(app, ui, sort);
        search_right = sort.min.x - 10.0;
    }
    let search = Rect::from_min_max(pos2(left, y), pos2(search_right.max(left + 40.0), y + h));
    search_box(app, ui, search);

    let enabled = count > 0 && !app.queue.busy_exclusive();
    if widgets::button_at(
        ui,
        update,
        "update-all",
        &update_text,
        ButtonKind::Main,
        enabled,
    )
    .clicked()
    {
        app.actions.push(Action::UpdateAll);
    }
}

fn search_box(app: &mut App, ui: &mut Ui, rect: Rect) {
    skin::well(ui.painter(), rect, Color32::BLACK);
    let edit = TextEdit::singleline(&mut app.query)
        .id(search_id())
        .hint_text(
            RichText::new("Search addons, or type /help")
                .color(theme::HINT)
                .font(theme::body(15.0)),
        )
        .font(theme::body(15.0))
        .text_color(Color32::WHITE)
        .frame(Frame::NONE.inner_margin(Margin::symmetric(8, 4)))
        .min_size(rect.size())
        .desired_width(rect.width())
        .char_limit(200)
        .align(Align2::LEFT_CENTER);
    let response = ui.put(rect, edit);
    if std::mem::take(&mut app.focus_search) {
        response.request_focus();
    }
    if response.changed() {
        app.actions.push(Action::SearchChanged);
    }
}

fn sort_dropdown(app: &mut App, ui: &mut Ui, rect: Rect) {
    let label = sort_label(app.sort);
    let response = ui
        .interact(rect, ui.scope_id().with("sort"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::ComboBox, true, format!("Sort by: {label}"))
    });
    skin::well(ui.painter(), rect, Color32::BLACK);
    if response.hovered() {
        ui.painter()
            .rect_filled(rect.shrink(1.0), 1.0, Color32::from_white_alpha(10));
    }
    shadow_text(
        ui.painter(),
        pos2(rect.min.x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::strong(13.5),
        theme::GOLD,
    );
    let arrow = pos2(rect.max.x - 14.0, rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            arrow + vec2(-5.0, -3.0),
            arrow + vec2(5.0, -3.0),
            arrow + vec2(0.0, 4.0),
        ],
        theme::GOLD_DIM,
        Stroke::NONE,
    ));

    if std::mem::take(&mut app.open_sort) {
        Popup::open_id(ui.ctx(), Popup::default_response_id(&response));
    }
    Popup::menu(&response)
        .width(rect.width())
        .gap(2.0)
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (sort, text) in SORTS {
                let (row, resp) =
                    ui.allocate_exact_size(vec2(rect.width() - 14.0, 24.0), Sense::click());
                let selected = app.sort == sort;
                resp.widget_info(|| {
                    egui::WidgetInfo::selected(egui::Role::Button, true, selected, text)
                });
                if resp.hovered() {
                    ui.painter()
                        .rect_filled(row, 2.0, Color32::from_white_alpha(16));
                }
                ui.painter().text(
                    pos2(row.min.x + 6.0, row.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    theme::strong(13.5),
                    if selected { theme::GOLD } else { theme::TEXT },
                );
                if resp.clicked() {
                    app.actions.push(Action::SetSort(sort));
                    ui.close();
                }
            }
        });
}
