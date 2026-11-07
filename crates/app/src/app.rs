//! The window: frame, toolbar, quest log and parchment.

use crate::skin::{self, Skin, shadow_text};
use crate::widgets::{self, ButtonKind, Glyph};
use crate::{Launch, demo, theme};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Frame, Rect, ResizeDirection, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextEdit, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};
use feeling_core::catalog::{CATEGORIES, Catalog};
use std::collections::HashSet;

const ROW_HEIGHT: f32 = 23.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    MyAddons,
    Browse,
    Settings,
}

enum Row {
    Header {
        id: &'static str,
        name: &'static str,
        count: usize,
        open: bool,
    },
    Addon(usize),
}

pub struct App {
    skin: Skin,
    launch: Launch,
    frames: u32,
    tab: Tab,
    query: String,
    catalog: Catalog,
    collapsed: HashSet<&'static str>,
    rows: Vec<Row>,
    selected: Option<usize>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        theme::install(&cc.egui_ctx);
        let catalog = demo::catalog();
        let mut app = Self {
            skin: Skin::load(&cc.egui_ctx),
            launch,
            frames: 0,
            tab: Tab::Browse,
            query: String::new(),
            catalog,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: Some(0),
        };
        app.rebuild_rows();
        app
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (id, name) in CATEGORIES {
            let members: Vec<usize> = self
                .catalog
                .addons
                .iter()
                .enumerate()
                .filter(|(_, a)| a.category == id)
                .map(|(i, _)| i)
                .collect();
            if members.is_empty() {
                continue;
            }
            let open = !self.collapsed.contains(id);
            self.rows.push(Row::Header {
                id,
                name,
                count: members.len(),
                open,
            });
            if open {
                self.rows.extend(members.into_iter().map(Row::Addon));
            }
        }
    }

    fn window_chrome(&mut self, ui: &mut Ui, full: Rect) {
        let ctx = ui.ctx().clone();
        let painter = ui.painter().clone();
        self.skin.stone(&painter, full);

        // A darker band behind the title, so the gold reads against the stone.
        let band = Rect::from_min_max(full.min, pos2(full.max.x, full.min.y + 46.0));
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

        let close =
            Rect::from_min_size(pos2(full.max.x - 36.0, full.min.y + 14.0), vec2(20.0, 20.0));
        if widgets::chrome_button(ui, close, "close", Glyph::Close, "Close").clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        let minimize = close.translate(vec2(-25.0, 0.0));
        if widgets::chrome_button(ui, minimize, "minimize", Glyph::Minimize, "Minimize").clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
    }

    fn resize_grips(&self, ui: &mut Ui, full: Rect) {
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

    fn toolbar(&mut self, ui: &mut Ui, full: Rect) {
        let y = full.min.y + 56.0;
        let h = 28.0;
        let left = full.min.x + 102.0;
        let update = Rect::from_min_size(pos2(full.max.x - 136.0, y + 1.0), vec2(116.0, 26.0));
        let sort = Rect::from_min_size(pos2(update.min.x - 150.0, y + 1.0), vec2(140.0, 26.0));
        let search = Rect::from_min_max(pos2(left, y), pos2(sort.min.x - 10.0, y + h));

        skin::well(ui.painter(), search, Color32::BLACK);
        let edit = TextEdit::singleline(&mut self.query)
            .id_salt("search")
            .hint_text(
                RichText::new("Search addons, or type /help")
                    .color(theme::HINT)
                    .font(theme::body(15.0)),
            )
            .font(theme::body(15.0))
            .text_color(Color32::WHITE)
            .frame(Frame::NONE)
            .margin(vec2(8.0, 4.0))
            .min_size(search.size())
            .desired_width(search.width())
            .align(Align2::LEFT_CENTER);
        ui.put(search, edit);

        skin::well(ui.painter(), sort, Color32::BLACK);
        shadow_text(
            ui.painter(),
            pos2(sort.min.x + 10.0, sort.center().y),
            Align2::LEFT_CENTER,
            "Most popular",
            theme::label(13.5),
            theme::GOLD,
        );
        let arrow = pos2(sort.max.x - 14.0, sort.center().y);
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                arrow + vec2(-5.0, -3.0),
                arrow + vec2(5.0, -3.0),
                arrow + vec2(0.0, 4.0),
            ],
            theme::GOLD_DIM,
            Stroke::NONE,
        ));

        widgets::button_at(
            ui,
            update,
            "update-all",
            "Update all (3)",
            ButtonKind::Main,
            true,
        );
    }

    fn quest_log(&mut self, ui: &mut Ui, rect: Rect) {
        skin::well(ui.painter(), rect, theme::WELL);
        let inner = rect.shrink(3.0);
        let mut toggled = None;
        let mut clicked = None;
        let rows = &self.rows;
        let catalog = &self.catalog;
        let selected = self.selected;
        ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            ScrollArea::vertical()
                .id_salt("quest-log")
                .auto_shrink(false)
                .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for row in &rows[range] {
                        let width = ui.available_width();
                        let (r, resp) =
                            ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
                        let p = ui.painter_at(r);
                        match row {
                            Row::Header {
                                id,
                                name,
                                count,
                                open,
                            } => {
                                if resp.hovered() {
                                    p.rect_filled(r, 0.0, Color32::from_white_alpha(8));
                                }
                                let b = Rect::from_center_size(
                                    pos2(r.min.x + 13.0, r.center().y),
                                    vec2(13.0, 13.0),
                                );
                                p.rect_filled(b, 1.0, Color32::from_rgb(40, 30, 16));
                                p.rect_stroke(
                                    b,
                                    1.0,
                                    Stroke::new(1.0, theme::GOLD_DIM),
                                    StrokeKind::Inside,
                                );
                                let s = Stroke::new(1.5, theme::GOLD);
                                p.line_segment(
                                    [b.center() - vec2(3.5, 0.0), b.center() + vec2(3.5, 0.0)],
                                    s,
                                );
                                if !open {
                                    p.line_segment(
                                        [b.center() - vec2(0.0, 3.5), b.center() + vec2(0.0, 3.5)],
                                        s,
                                    );
                                }
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
                                    theme::body(13.0),
                                    theme::GOLD_DIM,
                                );
                                if resp.clicked() {
                                    toggled = Some(*id);
                                }
                            }
                            Row::Addon(i) => {
                                let addon = &catalog.addons[*i];
                                let is_selected = selected == Some(*i);
                                if is_selected {
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
                                shadow_text(
                                    &p,
                                    pos2(r.min.x + 26.0, r.center().y),
                                    Align2::LEFT_CENTER,
                                    &addon.name,
                                    theme::body(15.0),
                                    theme::tier_color(addon.tier),
                                );
                                let version =
                                    addon.stable.as_ref().map_or("", |s| s.version.as_str());
                                let (text, color) = if addon.tier == 0 {
                                    ("out of date", theme::ERROR_RED)
                                } else {
                                    (version, theme::TEXT_DIM)
                                };
                                shadow_text(
                                    &p,
                                    pos2(r.max.x - 8.0, r.center().y),
                                    Align2::RIGHT_CENTER,
                                    text,
                                    theme::body(13.0),
                                    color,
                                );
                                let resp = resp.on_hover_ui(|ui| addon_tooltip(ui, addon));
                                if resp.clicked() {
                                    clicked = Some(*i);
                                }
                            }
                        }
                    }
                });
        });
        if let Some(id) = toggled {
            if !self.collapsed.remove(id) {
                self.collapsed.insert(id);
            }
            self.rebuild_rows();
        }
        if clicked.is_some() {
            self.selected = clicked;
        }
    }

    fn page(&mut self, ui: &mut Ui, rect: Rect) {
        let painter = ui.painter();
        self.skin.parchment(painter, rect);
        painter.rect_stroke(
            rect,
            1.0,
            Stroke::new(1.0, Color32::from_black_alpha(220)),
            StrokeKind::Outside,
        );
        painter.rect_stroke(
            rect,
            1.0,
            Stroke::new(1.0, theme::GOLD_LINE),
            StrokeKind::Inside,
        );
        let Some(addon) = self.selected.and_then(|i| self.catalog.addons.get(i)) else {
            return;
        };

        let button_row = Rect::from_min_max(
            pos2(rect.min.x + 18.0, rect.max.y - 42.0),
            pos2(rect.max.x - 18.0, rect.max.y - 12.0),
        );
        let text_area = Rect::from_min_max(
            rect.min + vec2(18.0, 16.0),
            pos2(rect.max.x - 10.0, button_row.min.y - 8.0),
        );

        ui.scope_builder(UiBuilder::new().max_rect(text_area), |ui| {
            ScrollArea::vertical()
                .id_salt("page")
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.set_max_width(text_area.width() - 16.0);
                    ui.label(
                        RichText::new(&addon.name)
                            .font(theme::display(27.0))
                            .color(theme::INK_HEAD),
                    );
                    ui.label(
                        RichText::new(format!("by {}", addon.author))
                            .font(theme::label(14.0))
                            .color(theme::INK_SOFT),
                    );
                    ui.add_space(6.0);
                    page_rule(ui);
                    ui.add_space(8.0);
                    section(ui, "Description");
                    ui.label(
                        RichText::new(&addon.summary)
                            .font(theme::label(15.5))
                            .color(theme::INK),
                    );
                    ui.add_space(12.0);
                    section(ui, "Details");
                    let release = addon.stable.as_ref();
                    let fact = |ui: &mut Ui, k: &str, v: String| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(k)
                                    .font(theme::label(14.5))
                                    .color(theme::INK_SOFT),
                            );
                            ui.label(RichText::new(v).font(theme::strong(15.0)).color(theme::INK));
                        });
                    };
                    if let Some(r) = release {
                        fact(ui, "Version:", r.version.clone());
                        fact(ui, "Size:", megabytes(r.size));
                    }
                    fact(ui, "Folders:", addon.folders.join(", "));
                    fact(ui, "Requires:", "Interface 16001".to_owned());
                });
        });

        ui.scope_builder(UiBuilder::new().max_rect(button_row), |ui| {
            ui.horizontal(|ui| {
                widgets::button(ui, "Install", ButtonKind::Main, true);
                widgets::button(ui, "Share", ButtonKind::Quiet, true);
            });
        });
    }

    fn bottom(&mut self, ui: &mut Ui, full: Rect) {
        let strip_y = full.max.y - 40.0;
        let bar = Rect::from_center_size(pos2(full.center().x, strip_y - 19.0), vec2(360.0, 22.0));
        self.skin
            .casting_bar(ui.painter(), bar, 0.64, "Installing Questie");

        let tabs = [
            (Tab::MyAddons, "My Addons"),
            (Tab::Browse, "Browse"),
            (Tab::Settings, "Settings"),
        ];
        let mut x = full.min.x + 20.0;
        for (tab, text) in tabs {
            let r = Rect::from_min_size(pos2(x, strip_y), vec2(112.0, 28.0));
            if widgets::tab(ui, r, text, self.tab == tab).clicked() {
                self.tab = tab;
            }
            x += 116.0;
        }
        shadow_text(
            ui.painter(),
            pos2(full.max.x - 20.0, strip_y + 14.0),
            Align2::RIGHT_CENTER,
            "Forever 1.60.1  |  18 addons  |  updated 2 hours ago",
            theme::body(13.0),
            theme::TEXT_DIM,
        );
    }

    /// `--screenshot`: render a few frames so everything has settled, save a PNG, quit.
    fn take_screenshot_if_asked(&mut self, ui: &Ui) {
        let Some(path) = self.launch.screenshot.clone() else {
            return;
        };
        let ctx = ui.ctx();
        self.frames += 1;
        ctx.request_repaint();
        if self.frames == 5 {
            let closer = ctx.clone();
            ctx.request_screenshot(move |image| {
                let bytes: Vec<u8> = image
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_srgba_unmultiplied())
                    .collect();
                let saved = image::save_buffer(
                    &path,
                    &bytes,
                    image.size[0] as u32,
                    image.size[1] as u32,
                    image::ColorType::Rgba8,
                );
                if let Err(e) = saved {
                    eprintln!("could not save screenshot: {e}");
                }
                closer.send_viewport_cmd(ViewportCommand::Close);
            });
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.05, 0.04, 0.03, 1.0]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        self.window_chrome(ui, full);
        self.toolbar(ui, full);

        let top = full.min.y + 94.0;
        let bottom = full.max.y - 74.0;
        let split = full.min.x + (full.width() * 0.42).clamp(320.0, 460.0);
        let log = Rect::from_min_max(pos2(full.min.x + 20.0, top), pos2(split - 4.0, bottom));
        let page = Rect::from_min_max(pos2(split + 4.0, top), pos2(full.max.x - 20.0, bottom));
        self.quest_log(ui, log);
        self.page(ui, page);
        self.bottom(ui, full);

        self.skin.gold_frame(ui.painter(), full);
        self.skin.portrait(
            ui.painter(),
            pos2(full.min.x + 50.0, full.min.y + 50.0),
            44.0,
            false,
        );
        self.resize_grips(ui, full);
        self.take_screenshot_if_asked(ui);
    }
}

/// A hand-ruled line across the parchment, with a small diamond at its center.
fn page_rule(ui: &mut Ui) {
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

fn section(ui: &mut Ui, title: &str) {
    ui.label(
        RichText::new(title)
            .font(theme::display(17.0))
            .color(theme::INK_HEAD),
    );
    ui.add_space(1.0);
}

fn megabytes(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} KB", (bytes / 1000).max(1))
    }
}

/// Addon sizes, priced the way a vendor would.
fn coins(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    let gold = mb.floor() as u64;
    let silver = ((mb - mb.floor()) * 100.0).round() as u64;
    if gold == 0 {
        format!("{silver} silver")
    } else {
        format!("{gold} gold {silver} silver")
    }
}

fn addon_tooltip(ui: &mut Ui, addon: &feeling_core::catalog::Addon) {
    ui.set_max_width(270.0);
    widgets::tooltip_title(ui, &addon.name, theme::tier_color(addon.tier));
    widgets::tooltip_line(ui, "Binds when installed");
    if let Some(r) = &addon.stable {
        widgets::tooltip_line(ui, &format!("Size: {}", coins(r.size)));
    }
    if !addon.summary.is_empty() {
        widgets::tooltip_colored(ui, &format!("Use: {}", addon.summary), theme::USE_GREEN);
    }
}
