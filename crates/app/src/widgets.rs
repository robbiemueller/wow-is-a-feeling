//! Hand-painted widgets. Labels stay literal; the charm is in how they look.

use crate::skin::{self, shadow_text};
use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Rect, Response, Sense, Stroke, StrokeKind, TextFormat, Ui,
    Vec2, pos2, text::LayoutJob, vec2,
};

/// What kind of red button this is. `Quiet` is the brown one beside a main action.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Main,
    Quiet,
}

/// Text in the label face, with any numbers in the heavier one, because the label face's
/// numerals read badly: its `0` looks like an `O`.
pub fn mixed_job(text: &str, size: f32, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut append = |part: &str, number: bool| {
        if part.is_empty() {
            return;
        }
        let font = if number {
            theme::strong(size)
        } else {
            theme::label(size)
        };
        job.append(part, 0.0, TextFormat::simple(font, color));
    };
    let mut start = 0;
    let mut in_number = false;
    for (at, c) in text.char_indices() {
        let number = c.is_ascii_digit() || (in_number && matches!(c, '.' | ',' | '-' | '+'));
        if number != in_number {
            append(&text[start..at], in_number);
            start = at;
            in_number = number;
        }
    }
    append(&text[start..], in_number);
    job
}

/// Running text in the label face that may hold numbers, wrapped to fit.
pub fn mixed_label(ui: &mut Ui, text: &str, size: f32, color: Color32) -> Response {
    ui.add(egui::Label::new(mixed_job(text, size, color)).wrap())
}

/// The classic red button with gold lettering.
pub fn button(ui: &mut Ui, text: &str, kind: ButtonKind, enabled: bool) -> Response {
    let size = vec2(button_width(ui, text), 26.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        paint_button(ui, rect, text, kind, &response, enabled);
    }
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// The same button painted at a fixed spot, for the toolbar.
pub fn button_at(
    ui: &mut Ui,
    rect: Rect,
    id: &str,
    text: &str,
    kind: ButtonKind,
    enabled: bool,
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.scope_id().with(id), sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, text));
    paint_button(ui, rect, text, kind, &response, enabled);
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

fn paint_button(
    ui: &Ui,
    rect: Rect,
    text: &str,
    kind: ButtonKind,
    response: &Response,
    enabled: bool,
) {
    let painter = ui.painter_at(rect.expand(2.0));
    let pressed = response.is_pointer_button_down_on() && response.hovered();
    let hovered = response.hovered() && enabled;
    let (top, bottom) = match (kind, enabled, pressed, hovered) {
        (_, false, _, _) => (Color32::from_rgb(58, 46, 40), Color32::from_rgb(38, 30, 26)),
        (ButtonKind::Main, _, true, _) => {
            (Color32::from_rgb(92, 14, 8), Color32::from_rgb(126, 22, 14))
        }
        (ButtonKind::Main, _, _, true) => (
            Color32::from_rgb(190, 44, 28),
            Color32::from_rgb(120, 20, 12),
        ),
        (ButtonKind::Main, _, _, _) => {
            (Color32::from_rgb(160, 30, 20), Color32::from_rgb(92, 14, 8))
        }
        (ButtonKind::Quiet, _, true, _) => {
            (Color32::from_rgb(40, 30, 20), Color32::from_rgb(60, 46, 30))
        }
        (ButtonKind::Quiet, _, _, true) => {
            (Color32::from_rgb(96, 74, 46), Color32::from_rgb(60, 44, 26))
        }
        (ButtonKind::Quiet, _, _, _) => {
            (Color32::from_rgb(78, 58, 36), Color32::from_rgb(46, 34, 20))
        }
    };
    painter.rect_filled(rect.expand(1.0), 3.0, Color32::from_black_alpha(230));
    skin::v_gradient(&painter, rect.shrink(1.0), top, bottom);
    // A bright lip along the top sells the bevel.
    painter.line_segment(
        [
            rect.left_top() + vec2(2.0, 2.0),
            rect.right_top() + vec2(-2.0, 2.0),
        ],
        Stroke::new(1.0, Color32::from_white_alpha(if pressed { 8 } else { 46 })),
    );
    let edge = if !enabled {
        Color32::from_rgb(92, 82, 62)
    } else if hovered {
        theme::GOLD
    } else {
        theme::GOLD_DIM
    };
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, edge), StrokeKind::Inside);
    let color = if enabled {
        theme::GOLD
    } else {
        Color32::from_rgb(130, 120, 104)
    };
    let at = rect.center() + if pressed { vec2(1.0, 1.0) } else { Vec2::ZERO };
    let galley = painter.layout_job(mixed_job(text, 14.0, color));
    let top_left = at - galley.size() / 2.0;
    painter.galley_with_override_text_color(
        top_left + vec2(1.0, 1.0),
        galley.clone(),
        theme::SHADOW,
    );
    painter.galley(top_left, galley, color);
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            3.0,
            Stroke::new(1.0, Color32::WHITE),
            StrokeKind::Outside,
        );
    }
}

/// A tab hanging from the bottom of the frame.
pub fn tab(ui: &mut Ui, rect: Rect, text: &str, selected: bool) -> Response {
    let response = ui
        .interact(rect, ui.scope_id().with(("tab", text)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, text));
    let painter = ui.painter_at(rect.expand(1.0));
    let hovered = response.hovered();
    let (top, bottom, edge) = if selected {
        (
            Color32::from_rgb(58, 44, 24),
            Color32::from_rgb(30, 24, 16),
            theme::GOLD_DIM,
        )
    } else if hovered {
        (
            Color32::from_rgb(30, 24, 16),
            Color32::from_rgb(40, 32, 20),
            theme::GOLD_DIM,
        )
    } else {
        (
            Color32::from_rgb(14, 11, 8),
            Color32::from_rgb(26, 20, 14),
            theme::GOLD_LINE,
        )
    };
    skin::v_gradient(&painter, rect, top, bottom);
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let stroke = Stroke::new(1.0, edge);
    painter.line_segment([pos2(l, b), pos2(l, t + 3.0)], stroke);
    painter.line_segment([pos2(l, t + 3.0), pos2(l + 3.0, t)], stroke);
    painter.line_segment([pos2(l + 3.0, t), pos2(r - 3.0, t)], stroke);
    painter.line_segment([pos2(r - 3.0, t), pos2(r, t + 3.0)], stroke);
    painter.line_segment([pos2(r, t + 3.0), pos2(r, b)], stroke);
    if !selected {
        painter.line_segment([pos2(l, b), pos2(r, b)], stroke);
    }
    let color = if selected || hovered {
        theme::GOLD
    } else {
        theme::GOLD_DIM
    };
    shadow_text(
        &painter,
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        theme::label(14.0),
        color,
    );
    response
}

/// A small square chrome button: minimize and close.
pub fn chrome_button(ui: &mut Ui, rect: Rect, id: &str, glyph: Glyph, label: &str) -> Response {
    let response = ui
        .interact(rect, ui.scope_id().with(id), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let painter = ui.painter_at(rect.expand(2.0));
    let hovered = response.hovered();
    let (top, bottom) = if hovered {
        (
            Color32::from_rgb(190, 44, 28),
            Color32::from_rgb(120, 20, 12),
        )
    } else {
        (Color32::from_rgb(138, 26, 16), Color32::from_rgb(82, 12, 6))
    };
    painter.rect_filled(rect.expand(1.0), 2.0, Color32::from_black_alpha(230));
    skin::v_gradient(&painter, rect.shrink(1.0), top, bottom);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(
            1.0,
            if hovered {
                theme::GOLD
            } else {
                theme::GOLD_DIM
            },
        ),
        StrokeKind::Inside,
    );
    let c = rect.center();
    let stroke = Stroke::new(1.6, theme::GOLD);
    match glyph {
        Glyph::Close => {
            painter.line_segment([c + vec2(-4.0, -4.0), c + vec2(4.0, 4.0)], stroke);
            painter.line_segment([c + vec2(4.0, -4.0), c + vec2(-4.0, 4.0)], stroke);
        }
        Glyph::Minimize => {
            painter.line_segment([c + vec2(-4.5, 4.0), c + vec2(4.5, 4.0)], stroke);
        }
    }
    response
}

#[derive(Clone, Copy)]
pub enum Glyph {
    Close,
    Minimize,
}

/// Item-tooltip title line, colored by tier.
pub fn tooltip_title(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::strong(15.0))
            .color(color),
    );
}

/// A plain white item-tooltip line.
pub fn tooltip_line(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::body(13.5))
            .color(Color32::WHITE),
    );
}

/// A colored item-tooltip line, for the green "Use:" and yellow flavor text.
pub fn tooltip_colored(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::body(13.5))
            .color(color),
    );
}

/// Tell a screen reader about text that's only painted.
pub fn read_aloud(ui: &mut Ui, rect: Rect, id: &str, text: &str) {
    let response = ui.interact(rect, ui.scope_id().with(("read", id)), Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
}

/// How wide [`button`] would make this label.
pub fn button_width(ui: &Ui, text: &str) -> f32 {
    let galley = ui.painter().layout_job(mixed_job(text, 14.0, theme::GOLD));
    (galley.size().x + 28.0).max(82.0)
}

/// The plus or minus box beside a collapsible header.
pub fn toggle_box(painter: &egui::Painter, center: egui::Pos2, open: bool) {
    let b = Rect::from_center_size(center, vec2(13.0, 13.0));
    painter.rect_filled(b, 1.0, Color32::from_rgb(40, 30, 16));
    painter.rect_stroke(
        b,
        1.0,
        Stroke::new(1.0, theme::GOLD_DIM),
        StrokeKind::Inside,
    );
    let s = Stroke::new(1.5, theme::GOLD);
    painter.line_segment(
        [b.center() - vec2(3.5, 0.0), b.center() + vec2(3.5, 0.0)],
        s,
    );
    if !open {
        painter.line_segment(
            [b.center() - vec2(0.0, 3.5), b.center() + vec2(0.0, 3.5)],
            s,
        );
    }
}

/// Dark text on the parchment.
pub fn ink(ui: &mut Ui, text: impl Into<String>, font: egui::FontId, color: Color32) -> Response {
    ui.label(egui::RichText::new(text.into()).font(font).color(color))
}

/// A small note on the parchment.
pub fn ink_note(ui: &mut Ui, text: impl Into<String>) -> Response {
    ink(ui, text, theme::body(13.5), theme::INK_SOFT)
}

/// A link on the parchment. The page's text color is dark, so egui's own light blue
/// wouldn't read. Hands back the response; opening is the caller's to decide.
pub fn ink_link(ui: &mut Ui, text: &str, font: egui::FontId) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, theme::INK_LINK);
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, text));
    let color = if response.hovered() {
        theme::INK_LINK_HOVER
    } else {
        theme::INK_LINK
    };
    if ui.is_rect_visible(rect) {
        ui.painter()
            .galley_with_override_text_color(rect.min, galley, color);
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, color),
        );
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A small checkbox on the parchment, at a fixed spot. The page's button bar wants these.
pub fn ink_check_at(ui: &mut Ui, rect: Rect, on: bool, text: &str, enabled: bool) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.scope_id().with(("check", text)), sense);
    paint_check(ui, rect, on, text, enabled, &response);
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, on, text));
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A small checkbox on the parchment, in the flow of the text.
pub fn ink_check(ui: &mut Ui, on: bool, text: &str, enabled: bool) -> Response {
    let size = vec2(check_width(ui, text), 20.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ink_check_at(ui, rect, on, text, enabled)
}

/// How wide a checkbox with this label is.
pub fn check_width(ui: &Ui, text: &str) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::body(14.0), theme::INK);
    14.0 + 6.0 + galley.size().x
}

fn paint_check(ui: &Ui, rect: Rect, on: bool, text: &str, enabled: bool, response: &Response) {
    let painter = ui.painter_at(rect.expand(2.0));
    let side = 14.0;
    let boxed = Rect::from_min_size(
        pos2(rect.min.x, rect.center().y - side / 2.0),
        vec2(side, side),
    );
    let alpha = if enabled { 200 } else { 90 };
    painter.rect_filled(
        boxed,
        2.0,
        Color32::from_rgba_unmultiplied(40, 26, 10, alpha),
    );
    let edge = if response.hovered() && enabled {
        theme::INK
    } else {
        theme::INK_SOFT
    };
    painter.rect_stroke(boxed, 2.0, Stroke::new(1.0, edge), StrokeKind::Inside);
    if on {
        let tick = Stroke::new(2.0, theme::GOLD);
        let c = boxed.center();
        painter.line_segment([c + vec2(-3.5, 0.0), c + vec2(-1.0, 3.0)], tick);
        painter.line_segment([c + vec2(-1.0, 3.0), c + vec2(4.0, -3.5)], tick);
    }
    let color = if enabled {
        theme::INK
    } else {
        Color32::from_rgba_unmultiplied(46, 30, 12, 130)
    };
    painter.text(
        pos2(boxed.max.x + 6.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        theme::body(14.0),
        color,
    );
}

/// A clickable row on the parchment: a name, and a little state at the right.
pub fn ink_row(
    ui: &mut Ui,
    name: &str,
    name_color: Color32,
    tag: &str,
    tag_color: Color32,
) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 22.0), Sense::click());
    let label = if tag.is_empty() {
        name.to_owned()
    } else {
        format!("{name}, {tag}")
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        if response.hovered() {
            painter.rect_filled(rect, 2.0, Color32::from_rgba_unmultiplied(92, 60, 20, 44));
        }
        let tag_rect = painter.text(
            pos2(rect.max.x - 6.0, rect.center().y),
            Align2::RIGHT_CENTER,
            tag,
            theme::strong(13.5),
            tag_color,
        );
        let mut job = egui::text::LayoutJob::simple_singleline(
            name.to_owned(),
            theme::strong(14.5),
            name_color,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(
            (tag_rect.min.x - rect.min.x - 18.0).max(20.0),
        );
        let galley = painter.layout_job(job);
        painter.galley(
            pos2(rect.min.x + 6.0, rect.center().y - galley.size().y / 2.0),
            galley,
            name_color,
        );
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A hand-ruled line across the parchment, with a small diamond at its center.
pub fn page_rule(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 7.0), Sense::hover());
    let p = ui.painter_at(r);
    let c = r.center();
    let line = Color32::from_rgba_unmultiplied(92, 60, 20, 170);
    let clear = Color32::from_rgba_unmultiplied(92, 60, 20, 0);
    let left = Rect::from_min_max(pos2(r.min.x, c.y), pos2(c.x - 8.0, c.y + 1.0));
    let right = Rect::from_min_max(pos2(c.x + 8.0, c.y), pos2(r.max.x, c.y + 1.0));
    skin::h_gradient(&p, left, clear, line);
    skin::h_gradient(&p, right, line, clear);
    p.add(egui::Shape::convex_polygon(
        vec![
            c - vec2(4.0, 0.0),
            c - vec2(0.0, 3.0),
            c + vec2(4.0, 0.0),
            c + vec2(0.0, 3.0),
        ],
        line,
        Stroke::NONE,
    ));
}

/// A heading on the parchment.
pub fn section(ui: &mut Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .font(theme::display(17.0))
            .color(theme::INK_HEAD),
    );
    ui.add_space(1.0);
}

/// A label and its value on the parchment. Values use the heavier face, because the label
/// face's numerals read badly.
pub fn fact(ui: &mut Ui, key: &str, value: &str) {
    fact_with(ui, key, |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(value)
                    .font(theme::strong(15.0))
                    .color(theme::INK),
            )
            .wrap(),
        );
    });
}

/// A label with anything at all beside it.
pub fn fact_with(ui: &mut Ui, key: &str, value: impl FnOnce(&mut Ui)) {
    const KEY_WIDTH: f32 = 74.0;
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        vec2(width, 0.0),
        egui::Layout::left_to_right(egui::Align::TOP),
        |ui| {
            ui.allocate_ui(vec2(KEY_WIDTH, 18.0), |ui| {
                ui.set_width(KEY_WIDTH);
                ink(ui, key, theme::label(14.5), theme::INK_SOFT);
            });
            ui.vertical(|ui| {
                ui.set_max_width((width - KEY_WIDTH - 8.0).max(40.0));
                value(ui);
            });
        },
    );
}

/// A black well to type in on the parchment.
pub fn well_edit(ui: &mut Ui, edit: egui::TextEdit<'_>, size: Vec2) -> Response {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    skin::well(ui.painter(), rect, Color32::BLACK);
    let inner = rect.shrink2(vec2(6.0, 4.0));
    let response = ui.put(
        inner,
        edit.frame(egui::Frame::NONE)
            .font(theme::body(14.0))
            .text_color(Color32::WHITE)
            .desired_width(inner.width()),
    );
    // Whatever comes next starts below the well, not below the text.
    ui.advance_cursor_after_rect(rect);
    response
}
