//! Colors, fonts and egui style. Everything visual that isn't a texture.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    TextStyle, style::ScrollStyle,
};
use std::sync::Arc;

pub const GOLD: Color32 = Color32::from_rgb(255, 209, 0);
pub const GOLD_DIM: Color32 = Color32::from_rgb(201, 162, 39);
pub const GOLD_LINE: Color32 = Color32::from_rgb(138, 106, 40);
pub const TEXT: Color32 = Color32::from_rgb(232, 224, 204);
pub const TEXT_DIM: Color32 = Color32::from_rgb(158, 150, 132);
pub const HINT: Color32 = Color32::from_rgb(196, 188, 166);
pub const SHADOW: Color32 = Color32::from_black_alpha(220);
pub const ERROR_RED: Color32 = Color32::from_rgb(255, 48, 32);
pub const SYSTEM_YELLOW: Color32 = Color32::from_rgb(255, 255, 0);
pub const USE_GREEN: Color32 = Color32::from_rgb(30, 255, 0);
pub const FLAVOR: Color32 = Color32::from_rgb(255, 209, 0);
pub const INSTALLED: Color32 = Color32::from_rgb(140, 200, 120);

pub const INK: Color32 = Color32::from_rgb(46, 30, 12);
pub const INK_SOFT: Color32 = Color32::from_rgb(92, 68, 38);
pub const INK_HEAD: Color32 = Color32::from_rgb(74, 42, 8);
pub const INK_LINK: Color32 = Color32::from_rgb(20, 58, 120);
pub const INK_LINK_HOVER: Color32 = Color32::from_rgb(120, 30, 12);
pub const INK_GOOD: Color32 = Color32::from_rgb(28, 96, 28);
pub const INK_BAD: Color32 = Color32::from_rgb(150, 24, 12);
pub const INK_GOLD: Color32 = Color32::from_rgb(132, 86, 6);

pub const WELL: Color32 = Color32::from_rgb(10, 8, 6);
pub const TOOLTIP_FILL: Color32 = Color32::from_rgba_premultiplied(8, 9, 24, 244);
pub const TOOLTIP_EDGE: Color32 = Color32::from_rgb(150, 150, 165);

/// The six popularity colors, poor to legendary.
pub const TIERS: [Color32; 6] = [
    Color32::from_rgb(157, 157, 157),
    Color32::from_rgb(255, 255, 255),
    Color32::from_rgb(30, 255, 0),
    Color32::from_rgb(64, 150, 255),
    Color32::from_rgb(184, 92, 255),
    Color32::from_rgb(255, 128, 0),
];

pub fn tier_color(tier: u8) -> Color32 {
    TIERS[usize::from(tier.min(5))]
}

/// Ornate display face, for the window title and parchment headings.
pub fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("display".into()))
}

/// Flared serif, for buttons, tabs, headers and parchment body text.
pub fn label(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("label".into()))
}

/// Condensed sans, for the addon list and anything dense.
pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// A slightly heavier cut of the body face.
pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("strong".into()))
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        "almendra",
        include_bytes!("../../../assets/fonts/Almendra-Bold.ttf"),
    );
    add(
        "marcellus",
        include_bytes!("../../../assets/fonts/Marcellus-Regular.ttf"),
    );
    add(
        "fira",
        include_bytes!("../../../assets/fonts/FiraSansCondensed-Regular.ttf"),
    );
    add(
        "fira-medium",
        include_bytes!("../../../assets/fonts/FiraSansCondensed-Medium.ttf"),
    );

    let chain = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    fonts
        .families
        .insert(FontFamily::Proportional, chain(&["fira", "marcellus"]));
    fonts
        .families
        .insert(FontFamily::Monospace, chain(&["fira", "marcellus"]));
    fonts.families.insert(
        FontFamily::Name("display".into()),
        chain(&["almendra", "marcellus", "fira"]),
    );
    fonts.families.insert(
        FontFamily::Name("label".into()),
        chain(&["marcellus", "fira"]),
    );
    fonts.families.insert(
        FontFamily::Name("strong".into()),
        chain(&["fira-medium", "fira", "marcellus"]),
    );
    ctx.set_fonts(fonts);

    // One look, whatever the system's theme is. Without this a light-mode system picks
    // egui's light style, and none of what follows applies.
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, body(12.0)),
            (TextStyle::Body, body(15.0)),
            (TextStyle::Monospace, body(13.0)),
            (TextStyle::Button, label(14.0)),
            (TextStyle::Heading, display(20.0)),
        ]
        .into();

        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.scroll = ScrollStyle {
            bar_width: 10.0,
            floating: false,
            handle_min_length: 28.0,
            bar_inner_margin: 2.0,
            bar_outer_margin: 1.0,
            foreground_color: false,
            // Always visible, so a page that goes on says so before anyone hovers over it.
            dormant_background_opacity: 0.25,
            active_background_opacity: 0.35,
            interact_background_opacity: 0.5,
            dormant_handle_opacity: 0.8,
            active_handle_opacity: 0.9,
            interact_handle_opacity: 1.0,
            ..ScrollStyle::solid()
        };
        style.interaction.tooltip_delay = 0.25;
        style.interaction.show_tooltips_only_when_still = false;
        // Popups and hovers snap into place, which is both the 2004 way and fewer frames.
        style.animation_time = 0.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.weak_text_color = Some(HINT);
        v.panel_fill = Color32::TRANSPARENT;
        v.extreme_bg_color = Color32::BLACK;
        v.faint_bg_color = Color32::from_rgb(20, 16, 12);
        v.selection.bg_fill = Color32::from_rgb(92, 72, 24);
        v.selection.stroke = Stroke::new(1.0, GOLD);
        v.hyperlink_color = Color32::from_rgb(120, 180, 255);
        v.window_fill = TOOLTIP_FILL;
        v.window_stroke = Stroke::new(1.5, TOOLTIP_EDGE);
        v.window_corner_radius = CornerRadius::same(3);
        v.menu_corner_radius = CornerRadius::same(3);
        v.window_shadow = Shadow {
            offset: [0, 3],
            blur: 10,
            spread: 0,
            color: Color32::from_black_alpha(140),
        };
        v.popup_shadow = v.window_shadow;

        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, GOLD_LINE);
        w.inactive.bg_fill = Color32::from_rgb(92, 70, 28);
        w.inactive.weak_bg_fill = Color32::from_rgb(36, 28, 18);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.bg_stroke = Stroke::new(1.0, GOLD_LINE);
        w.hovered.bg_fill = Color32::from_rgb(138, 106, 40);
        w.hovered.weak_bg_fill = Color32::from_rgb(52, 40, 24);
        w.hovered.fg_stroke = Stroke::new(1.0, GOLD);
        w.hovered.bg_stroke = Stroke::new(1.0, GOLD_DIM);
        w.active.bg_fill = Color32::from_rgb(201, 162, 39);
        w.active.weak_bg_fill = Color32::from_rgb(64, 48, 26);
        w.active.fg_stroke = Stroke::new(1.0, GOLD);
        w.active.bg_stroke = Stroke::new(1.0, GOLD);
    });
}
