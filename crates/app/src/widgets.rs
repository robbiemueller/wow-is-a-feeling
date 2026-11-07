//! Hand-painted widgets. Labels stay literal; the charm is in how they look.

use crate::skin::{self, shadow_text};
use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2,
    vec2,
};

/// What kind of red button this is. `Quiet` is the brown one beside a main action.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Main,
    Quiet,
}

/// The classic red button with gold lettering.
pub fn button(ui: &mut Ui, text: &str, kind: ButtonKind, enabled: bool) -> Response {
    let font = theme::label(14.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), theme::GOLD);
    let size = vec2((galley.size().x + 28.0).max(82.0), 26.0);
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
    shadow_text(
        &painter,
        at,
        Align2::CENTER_CENTER,
        text,
        theme::label(14.0),
        color,
    );
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
