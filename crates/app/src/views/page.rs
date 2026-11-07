//! The parchment at right: whatever the list has selected, with its buttons along the
//! bottom where they're always in reach.

use crate::actions::Action;
use crate::app::{App, PageKind};
use crate::details_cache::Fetch;
use crate::model::{self, CatalogStatus, GameState, MainButton, Selection, Tag, clean_line};
use crate::ops::folder_set;
use crate::theme;
use crate::widgets::{self, ButtonKind};
use eframe::egui::{
    self, Color32, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2,
};
use feeling_core::catalog::{Addon, Channel};
use feeling_core::deps;
use feeling_core::state::{ManagedAddon, Origin};

const BUTTON_HEIGHT: f32 = 26.0;
const ROW_GAP: f32 = 6.0;
const CHECK_HEIGHT: f32 = 20.0;
/// Between why the last try failed and the checkboxes under it.
const NOTE_GAP: f32 = 6.0;
const NOTE_SIZE: f32 = 14.5;

/// Beside a required addon that an install leaves out, because nobody has reviewed it or it's
/// a big pack.
pub const INSTALL_IT_YOURSELF: &str = "(install it yourself)";

pub fn show(app: &mut App, ui: &mut Ui, rect: Rect) {
    paper(app, ui, rect);
    match app.page_kind() {
        PageKind::Addon(id) => addon_page(app, ui, rect, &id),
        PageKind::Other(primary) => other_page(app, ui, rect, &primary),
        PageKind::Kit(index) => kit_page(app, ui, rect, index),
        PageKind::Gap(index) => gap_page(app, ui, rect, index),
        PageKind::NoGame => no_game_page(app, ui, rect),
        PageKind::Empty => empty_page(app, ui, rect),
        PageKind::NoCatalog => no_catalog_page(app, ui, rect),
        PageKind::Loading => loading_page(app, ui, rect),
        PageKind::Nothing => note_page(app, ui, rect, "Pick something from the list."),
    }
}

/// The sheet itself, which the Settings pages share.
pub fn paper(app: &App, ui: &Ui, rect: Rect) {
    let painter = ui.painter();
    app.skin.parchment(painter, rect);
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
}

pub struct BarButton {
    pub label: String,
    pub kind: ButtonKind,
    pub enabled: bool,
    pub action: Action,
}

impl BarButton {
    pub fn main(label: impl Into<String>, enabled: bool, action: Action) -> Self {
        Self {
            label: label.into(),
            kind: ButtonKind::Main,
            enabled,
            action,
        }
    }

    pub fn quiet(label: impl Into<String>, enabled: bool, action: Action) -> Self {
        Self {
            label: label.into(),
            kind: ButtonKind::Quiet,
            enabled,
            action,
        }
    }
}

pub struct BarCheck {
    pub label: &'static str,
    pub on: bool,
    pub enabled: bool,
    pub action: Action,
}

/// Which row each width lands in, wrapping when one won't fit.
fn flow(widths: &[f32], room: f32, gap: f32) -> Vec<usize> {
    let mut rows = Vec::with_capacity(widths.len());
    let (mut row, mut used) = (0, 0.0);
    for &width in widths {
        if used > 0.0 && used + gap + width > room {
            row += 1;
            used = 0.0;
        }
        if used > 0.0 {
            used += gap;
        }
        used += width;
        rows.push(row);
    }
    rows
}

/// Lay a page out: its scrolling content on top, the bar pinned below it.
pub fn framed(
    app: &mut App,
    ui: &mut Ui,
    rect: Rect,
    buttons: Vec<BarButton>,
    checks: Vec<BarCheck>,
    content: impl FnOnce(&mut App, &mut Ui),
) {
    framed_with_note(app, ui, rect, buttons, checks, None, content);
}

/// [`framed`], with `note` pinned above the bar: why the last try at this addon failed, in
/// red ink, so it's beside the buttons that try again and never scrolls out of sight.
fn framed_with_note(
    app: &mut App,
    ui: &mut Ui,
    rect: Rect,
    buttons: Vec<BarButton>,
    checks: Vec<BarCheck>,
    note: Option<&str>,
    content: impl FnOnce(&mut App, &mut Ui),
) {
    let room = (rect.width() - 36.0).max(40.0);
    let note_height = note.map_or(0.0, |text| {
        let font = theme::body(NOTE_SIZE);
        let galley = ui
            .painter()
            .layout(text.to_owned(), font, theme::INK_BAD, room);
        galley.size().y.ceil() + NOTE_GAP
    });
    let widths: Vec<f32> = buttons
        .iter()
        .map(|b| widgets::button_width(ui, &b.label))
        .collect();
    let rows = flow(&widths, room, 6.0);
    let button_rows = rows.last().map_or(0, |last| last + 1);
    let check_widths: Vec<f32> = checks
        .iter()
        .map(|c| widgets::check_width(ui, c.label))
        .collect();
    let check_flow = flow(&check_widths, room, 14.0);
    let check_rows = check_flow.last().map_or(0, |last| last + 1);

    let bar_height = note_height
        + button_rows as f32 * (BUTTON_HEIGHT + ROW_GAP)
        + check_rows as f32 * (CHECK_HEIGHT + 2.0);
    let bar_bottom = rect.max.y - 10.0;
    let bar_top = bar_bottom - bar_height;
    let text_bottom = (bar_top - 4.0).max(rect.min.y + 36.0);
    let text = Rect::from_min_max(
        rect.min + vec2(18.0, 14.0),
        pos2(rect.max.x - 8.0, text_bottom),
    );

    ui.scope_builder(UiBuilder::new().max_rect(text), |ui| {
        // A scrollbar in the paper's own browns: egui's black groove would shout here.
        let visuals = ui.visuals_mut();
        visuals.extreme_bg_color = Color32::from_rgb(120, 92, 54);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(92, 66, 32);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(112, 80, 38);
        visuals.widgets.active.bg_fill = Color32::from_rgb(70, 48, 22);
        let mut area = ScrollArea::vertical().id_salt("page").auto_shrink(false);
        if std::mem::take(&mut app.page_to_top) {
            area = area.vertical_scroll_offset(0.0);
        }
        area.show(ui, |ui| {
            ui.set_max_width((text.width() - 18.0).max(40.0));
            content(app, ui);
        });
    });

    let mut y = bar_top;
    if let Some(text) = note {
        let at = Rect::from_min_size(
            pos2(rect.min.x + 18.0, y),
            vec2(room, note_height - NOTE_GAP),
        );
        ui.scope_builder(UiBuilder::new().max_rect(at), |ui| {
            widgets::ink(ui, text, theme::body(NOTE_SIZE), theme::INK_BAD);
        });
        y += note_height;
    }
    // Checkboxes above the buttons, so the main button always sits in the same corner.
    for row in 0..check_rows {
        let mut x = rect.min.x + 18.0;
        for (i, check) in checks.iter().enumerate() {
            if check_flow[i] != row {
                continue;
            }
            let at = Rect::from_min_size(pos2(x, y), vec2(check_widths[i], CHECK_HEIGHT));
            if widgets::ink_check_at(ui, at, check.on, check.label, check.enabled).clicked() {
                app.actions.push(check.action.clone());
            }
            x += check_widths[i] + 14.0;
        }
        y += CHECK_HEIGHT + 2.0;
    }
    for row in 0..button_rows {
        let mut x = rect.min.x + 18.0;
        for (i, button) in buttons.iter().enumerate() {
            if rows[i] != row {
                continue;
            }
            let at = Rect::from_min_size(pos2(x, y + 3.0), vec2(widths[i], BUTTON_HEIGHT));
            let response = widgets::button_at(
                ui,
                at,
                &button.label,
                &button.label,
                button.kind,
                button.enabled,
            );
            if response.clicked() {
                app.actions.push(button.action.clone());
            }
            x += widths[i] + 6.0;
        }
        y += BUTTON_HEIGHT + ROW_GAP;
    }
}

fn title(ui: &mut Ui, name: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(name)
                .font(theme::display(27.0))
                .color(theme::INK_HEAD),
        )
        .wrap(),
    );
}

fn byline(ui: &mut Ui, author: &str) {
    if author.is_empty() {
        return;
    }
    widgets::mixed_label(ui, &format!("by {author}"), 14.0, theme::INK_SOFT);
}

fn rule(ui: &mut Ui) {
    ui.add_space(6.0);
    widgets::page_rule(ui);
    ui.add_space(8.0);
}

/// Running text on the parchment.
pub fn body(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::body(15.0))
                .color(theme::INK),
        )
        .wrap(),
    );
}

fn tag_ink(tag: &Tag<'_>) -> Color32 {
    match tag {
        Tag::Update => theme::INK_GOLD,
        Tag::OutOfDate => theme::INK_BAD,
        Tag::Installed => theme::INK_GOOD,
        Tag::Version(_) | Tag::Nothing => theme::INK_SOFT,
    }
}

/// The color a tier's name takes on the parchment, where the list's bright colors wouldn't
/// read. The name of each tier is the same: only the shade is darker.
fn tier_ink(tier: u8) -> Color32 {
    match tier.min(5) {
        0 => Color32::from_rgb(110, 104, 94),
        1 => theme::INK,
        2 => Color32::from_rgb(24, 110, 24),
        3 => Color32::from_rgb(20, 72, 160),
        4 => Color32::from_rgb(110, 40, 160),
        _ => Color32::from_rgb(170, 78, 0),
    }
}

fn addon_page(app: &mut App, ui: &mut Ui, rect: Rect, id: &str) {
    let bundle = app.bundle.clone();
    let record = app.managed(id).cloned();
    // An installed addon whose entry has become another addon gets a page of its own, which
    // says so, with no Install, Update or Reinstall that would turn it into that addon.
    let addon = bundle
        .as_deref()
        .and_then(|b| model::entry_for(b, id, record.as_ref()));
    let Some(addon) = addon else {
        match record {
            Some(record) => {
                let listed = bundle.as_deref().and_then(|b| b.addon(id));
                let note = model::no_updates_note(&record, listed);
                managed_only_page(app, ui, rect, &record, &note);
            }
            None => note_page(app, ui, rect, "This addon isn't in the catalog anymore."),
        }
        return;
    };

    let busy = app.queue.busy_with(id) || app.queue.busy_exclusive();
    let adoptable = app.adoption_for(id).is_some();
    let main = model::main_button(addon, record.as_ref(), adoptable);
    let channel = model::channel_for(
        record.as_ref(),
        app.channel_choice.get(id).copied(),
        app.state.settings.default_channel,
    );
    let has_release = addon.release(channel).is_some();
    let main_enabled =
        !busy && main != MainButton::Pinned && (has_release || main == MainButton::Manage);

    let mut buttons = vec![
        BarButton::main(main.label(), main_enabled, app.main_action_for(id, main)),
        BarButton::quiet(
            "Remove",
            record.is_some() && !busy,
            Action::AskRemove(Selection::Addon(id.to_owned())),
        ),
    ];
    let url = model::addon_page_url(addon);
    buttons.push(BarButton::quiet(
        "Open page",
        url.is_some(),
        Action::OpenLink(url.clone().unwrap_or_default()),
    ));
    buttons.push(BarButton::quiet(
        "Share",
        true,
        Action::Share(id.to_owned()),
    ));
    if let Some(button) = rollback_button(record.as_ref(), id, busy) {
        buttons.push(button);
    }

    let pinned = record.as_ref().is_some_and(|m| m.pinned);
    let checks = vec![
        BarCheck {
            label: "Use beta versions",
            on: channel == Channel::Beta,
            enabled: addon.beta.is_some() || channel == Channel::Beta,
            action: Action::SetBeta(id.to_owned(), channel != Channel::Beta),
        },
        BarCheck {
            label: "Don't update this addon",
            on: pinned,
            enabled: record
                .as_ref()
                .is_some_and(|m| m.origin != Origin::LocalZip),
            action: Action::SetPin(id.to_owned(), !pinned),
        },
    ];

    let failure = app.failures.get(id).cloned();
    framed_with_note(
        app,
        ui,
        rect,
        buttons,
        checks,
        failure.as_deref(),
        |app, ui| {
            title(ui, &addon.name);
            byline(ui, &addon.author);
            rule(ui);
            addon_body(app, ui, addon, record.as_ref(), channel);
        },
    );
}

fn rollback_button(record: Option<&ManagedAddon>, id: &str, busy: bool) -> Option<BarButton> {
    let record = record?;
    let version = record.previous_version.as_ref()?;
    record.previous_zip.as_ref()?;
    Some(BarButton::quiet(
        format!("Roll back to {}", clean_line(version, 24)),
        !busy,
        Action::Rollback(id.to_owned()),
    ))
}

fn addon_body(
    app: &mut App,
    ui: &mut Ui,
    addon: &Addon,
    record: Option<&ManagedAddon>,
    channel: Channel,
) {
    if !addon.summary.is_empty() {
        widgets::section(ui, "Description");
        widgets::mixed_label(ui, &addon.summary, 15.5, theme::INK);
        ui.add_space(10.0);
    }
    widgets::section(ui, "Details");
    // First in the section, so it's on screen without scrolling even in the smallest window.
    if !addon.trusted {
        widgets::ink(ui, model::UNTRUSTED_NOTE, theme::body(14.5), theme::INK);
        ui.add_space(3.0);
    }
    let release = addon.release(channel);
    match (record, release) {
        (Some(record), Some(release)) if model::update_for(record, addon).is_some() => {
            widgets::fact(
                ui,
                "Version:",
                &format!(
                    "{} installed, {} available",
                    record.version, release.version
                ),
            );
        }
        (Some(record), _) => widgets::fact(ui, "Version:", &record.version),
        (None, Some(release)) => widgets::fact(ui, "Version:", &release.version),
        (None, None) => widgets::fact(ui, "Version:", "No Forever build yet"),
    }
    if let Some(release) = release {
        if release.date > 0 {
            widgets::fact(ui, "Updated:", &model::date_text(release.date));
        }
        if release.size > 0 {
            widgets::fact(ui, "Size:", &model::size_text(release.size));
        }
    }
    let source = model::source_text(addon);
    match model::source_url(addon) {
        Some(url) => widgets::fact_with(ui, "Source:", |ui| {
            let link = widgets::ink_link_fit(ui, &source, theme::strong(15.0)).on_hover_text(&url);
            if link.clicked() {
                app.actions.push(Action::OpenLink(url.clone()));
            }
        }),
        None => widgets::fact(ui, "Source:", &source),
    }
    if !addon.folders.is_empty() {
        widgets::fact(ui, "Folders:", &clean_line(&addon.folders.join(", "), 600));
    }
    requires(app, ui, addon);
    packs(app, ui, addon);
    ui.add_space(10.0);
    details_sections(app, ui, addon);
}

/// Required addons by name, each a click away when the catalog has it.
fn requires(app: &mut App, ui: &mut Ui, addon: &Addon) {
    if addon.deps.is_empty() {
        return;
    }
    let bundle = app.bundle.clone();
    // What an install would leave for the player to add, decided the same way it decides:
    // nobody reviewed it, or it's a pack with a big download.
    let yourself: Vec<String> = bundle.as_deref().map_or_else(Vec::new, |b| {
        let installed = app.installed_folders();
        let resolved = deps::resolve(&addon.id, &b.catalog, &b.folders, installed.iter(), |id| {
            app.channel_of(id)
        });
        let unreviewed = resolved
            .unreviewed
            .into_iter()
            .map(|left| (left.needed_by, left.provider, left.folder));
        let too_big = resolved
            .too_big
            .into_iter()
            .map(|left| (left.needed_by, left.provider, left.folder));
        unreviewed
            .chain(too_big)
            .filter(|(needed_by, provider, _)| {
                *needed_by == addon.id && app.managed(provider).is_none()
            })
            .map(|(_, _, folder)| folder.to_lowercase())
            .collect()
    });
    widgets::fact_with(ui, "Requires:", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for dep in &addon.deps {
                // The same pick an install would make, so a stranger's entry never gets to
                // stand here as what this addon requires.
                let provider = bundle.as_deref().and_then(|b| {
                    let best = deps::best_provider(dep, &b.catalog, &b.folders)?;
                    b.catalog.addons.get(best)
                });
                match provider {
                    Some(provider) => {
                        let link = widgets::ink_link(ui, &provider.name, theme::strong(15.0));
                        if link.clicked() {
                            app.actions
                                .push(Action::Select(Selection::Addon(provider.id.clone())));
                        }
                        if yourself.contains(&dep.to_lowercase()) {
                            widgets::ink(
                                ui,
                                INSTALL_IT_YOURSELF,
                                theme::body(14.0),
                                theme::INK_SOFT,
                            );
                        }
                    }
                    None => {
                        widgets::ink(ui, clean_line(dep, 60), theme::strong(15.0), theme::INK);
                    }
                }
            }
        });
    });
}

/// Packs that go with this addon, such as voice packs, one line per group. Each is a link to
/// its own page, where installing it is a deliberate press with its size and source in view.
/// A needed group with nothing installed says so in red, because the addon does nothing
/// useful until it's met.
fn packs(app: &mut App, ui: &mut Ui, addon: &Addon) {
    // Most addons have none, so they don't pay for a look at every folder each frame.
    if !addon.reviewed || addon.packs.is_empty() {
        return;
    }
    let Some(bundle) = app.bundle.clone() else {
        return;
    };
    let installed = folder_set(&app.installed_folders());
    let lines = model::pack_lines(addon, &bundle, &installed, |id| app.channel_of(id));
    for line in &lines {
        widgets::fact_with(ui, &format!("{}:", line.name), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (i, choice) in line.choices.iter().enumerate() {
                    if i > 0 {
                        aside(ui, "or", theme::INK_SOFT);
                    }
                    let link = widgets::ink_link(ui, &choice.name, theme::strong(15.0));
                    if link.clicked() {
                        app.actions
                            .push(Action::Select(Selection::Addon(choice.id.clone())));
                    }
                    if choice.installed {
                        aside(ui, "(installed)", theme::INK_GOOD);
                    } else if choice.size > 0 {
                        aside(
                            ui,
                            format!("({})", model::size_text(choice.size)),
                            theme::INK_SOFT,
                        );
                    }
                }
            });
            if line.needed && !line.met() {
                widgets::ink(
                    ui,
                    format!("{} needs one of these to work.", addon.name),
                    theme::body(14.5),
                    theme::INK_BAD,
                );
            }
        });
    }
}

/// A few words beside a link, such as "(1.5 GB)". Like the link, they go to the next line
/// whole rather than break in two.
fn aside(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    ui.add(
        egui::Label::new(
            RichText::new(text.into())
                .font(theme::body(14.0))
                .color(color),
        )
        .extend(),
    );
}

/// Screenshots, About and Changelog, whatever has arrived so far. A readme can run long,
/// so the pictures go first, where they get seen.
fn details_sections(app: &mut App, ui: &mut Ui, addon: &Addon) {
    let Some(entry) = app.details.get(&addon.id) else {
        return;
    };
    match &entry.fetch {
        Fetch::Waiting => {
            widgets::ink_note(ui, "Looking for this addon's readme...");
            ui.add_space(10.0);
        }
        Fetch::Failed { .. } => {
            widgets::ink_note(
                ui,
                "Couldn't fetch this addon's readme and screenshots. Everything else here still works.",
            );
            ui.add_space(10.0);
        }
        Fetch::Ready(details) => {
            if !entry.textures.is_empty() {
                widgets::section(ui, "Screenshots");
                for texture in &entry.textures {
                    let size = texture.size_vec2();
                    let width = size.x.min(ui.available_width()).min(640.0);
                    let shown = vec2(width, size.y * width / size.x.max(1.0));
                    let (rect, _) = ui.allocate_exact_size(shown, Sense::hover());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    ui.add_space(6.0);
                }
                ui.add_space(4.0);
            }
            if !details.about.is_empty() && details.about != addon.summary {
                widgets::section(ui, "About");
                body(ui, &details.about);
                ui.add_space(10.0);
            }
            if let Some(changelog) = details.changelog.clone() {
                let open = app.changelog_open;
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::Role::DisclosureTriangle,
                        true,
                        open,
                        "Changelog",
                    )
                });
                widgets::toggle_box(ui.painter(), pos2(rect.min.x + 8.0, rect.center().y), open);
                ui.painter().text(
                    pos2(rect.min.x + 22.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    "Changelog",
                    theme::display(17.0),
                    theme::INK_HEAD,
                );
                if response.clicked() {
                    app.changelog_open = !open;
                }
                if open {
                    body(ui, &changelog);
                }
                ui.add_space(10.0);
            }
        }
    }
}

/// A managed addon with no catalog entry to go by: a dropped zip, one that left the catalog,
/// or one whose entry now lists a different addon. `note` says which.
fn managed_only_page(app: &mut App, ui: &mut Ui, rect: Rect, record: &ManagedAddon, note: &str) {
    let busy = app.queue.busy_with(&record.id) || app.queue.busy_exclusive();
    let mut buttons = vec![BarButton::main(
        "Remove",
        !busy,
        Action::AskRemove(Selection::Addon(record.id.clone())),
    )];
    if let Some(button) = rollback_button(Some(record), &record.id, busy) {
        buttons.push(button);
    }
    let name = record.name.clone();
    let version = record.version.clone();
    let folders = record.folders.join(", ");
    let failure = app.failures.get(&record.id).cloned();
    framed_with_note(
        app,
        ui,
        rect,
        buttons,
        Vec::new(),
        failure.as_deref(),
        |_, ui| {
            title(ui, &name);
            rule(ui);
            body(ui, note);
            ui.add_space(10.0);
            widgets::section(ui, "Details");
            if !version.is_empty() {
                widgets::fact(ui, "Version:", &version);
            }
            widgets::fact(ui, "Folders:", &clean_line(&folders, 600));
        },
    );
}

fn other_page(app: &mut App, ui: &mut Ui, rect: Rect, primary: &str) {
    let Some(group) = app.group(primary).cloned() else {
        note_page(app, ui, rect, "That folder is gone.");
        return;
    };
    let busy = app.queue.busy_exclusive() || app.queue.busy_with(&format!("other:{primary}"));
    let adoption = app.adoption_for_folder(primary).cloned();
    let bundle = app.bundle.clone();
    let matched = adoption.as_ref().and_then(|a| {
        bundle
            .as_deref()
            .and_then(|b| b.addon(&a.addon_id))
            .map(|addon| format!("{} ({})", addon.name, model::source_text(addon)))
    });

    let mut buttons = Vec::new();
    if let Some(adoption) = &adoption {
        buttons.push(BarButton::main(
            "Manage with this app",
            !busy,
            Action::Adopt(adoption.addon_id.clone()),
        ));
    }
    buttons.push(BarButton::quiet(
        "Remove",
        !busy,
        Action::AskRemove(Selection::Other(primary.to_owned())),
    ));

    let notes = app.toc_notes(primary).map(|n| clean_line(&n, 600));
    framed(app, ui, rect, buttons, Vec::new(), |_, ui| {
        title(ui, &clean_line(&group.title, 80));
        rule(ui);
        if let Some(notes) = &notes {
            body(ui, notes);
            ui.add_space(8.0);
        }
        body(ui, "This addon wasn't installed by this app.");
        if let Some(name) = &matched {
            ui.add_space(4.0);
            body(
                ui,
                &format!(
                    "It matches {name} in the catalog. Let this app manage it and it gets updates."
                ),
            );
        }
        ui.add_space(10.0);
        widgets::section(ui, "Details");
        if let Some(version) = &group.version {
            widgets::fact(ui, "Version:", &clean_line(version, 40));
        }
        widgets::fact(
            ui,
            "Forever:",
            if group.targets_forever {
                "Says it supports WoW Forever"
            } else {
                "Doesn't say it supports WoW Forever, so the game may call it out of date"
            },
        );
        widgets::fact(ui, "Folders:", &clean_line(&group.folders.join(", "), 600));
    });
}

fn kit_page(app: &mut App, ui: &mut Ui, rect: Rect, index: usize) {
    let bundle = app.bundle.clone();
    let Some(bundle) = bundle else {
        return;
    };
    let Some(kit) = bundle.catalog.kits.get(index) else {
        note_page(app, ui, rect, "That kit is gone.");
        return;
    };
    let managed = app.managed_map().cloned();
    let missing = model::kit_missing(kit, &bundle, managed.as_ref());
    let label = if missing.is_empty() {
        "Install all".to_owned()
    } else {
        format!("Install all ({})", missing.len())
    };
    let busy = app.queue.busy_exclusive() || missing.iter().all(|id| app.queue.busy_with(id));
    let buttons = vec![BarButton::main(
        label,
        !missing.is_empty() && !busy,
        Action::Install(missing),
    )];
    let rows = model::kit_addons(kit, &bundle, managed.as_ref());
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        title(ui, &kit.name);
        rule(ui);
        body(ui, &kit.description);
        ui.add_space(10.0);
        widgets::section(ui, "In this kit");
        for (addon, tag) in &rows {
            let row = widgets::ink_row(
                ui,
                &addon.name,
                tier_ink(addon.tier),
                tag.words(),
                tag_ink(tag),
            );
            if row.clicked() {
                app.actions
                    .push(Action::Select(Selection::Addon(addon.id.clone())));
            }
        }
    });
}

fn gap_page(app: &mut App, ui: &mut Ui, rect: Rect, index: usize) {
    let bundle = app.bundle.clone();
    let Some(bundle) = bundle else {
        return;
    };
    let Some(gap) = bundle.catalog.gaps.get(index) else {
        note_page(app, ui, rect, "That entry is gone.");
        return;
    };
    let url = gap.website.as_deref().and_then(model::safe_url);
    let buttons = vec![BarButton::main(
        "Open its page",
        url.is_some(),
        Action::OpenLink(url.unwrap_or_default().to_owned()),
    )];
    let alternatives = model::gap_alternatives(gap, &bundle);
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        title(ui, &gap.name);
        rule(ui);
        widgets::section(ui, "Not available here");
        body(ui, &gap.reason);
        ui.add_space(10.0);
        if !alternatives.is_empty() {
            widgets::section(ui, "Try one of these");
            for addon in &alternatives {
                let record = app.managed(&addon.id);
                let tag = model::browse_tag(addon, record);
                let row = widgets::ink_row(
                    ui,
                    &addon.name,
                    tier_ink(addon.tier),
                    tag.words(),
                    tag_ink(&tag),
                );
                if row.clicked() {
                    app.actions
                        .push(Action::Select(Selection::Addon(addon.id.clone())));
                }
            }
            ui.add_space(10.0);
        }
        widgets::ink_note(
            ui,
            "Downloaded it yourself? Drop the zip on this window and it installs the same safe way.",
        );
    });
}

fn empty_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let buttons = vec![BarButton::main(
        "Browse addons",
        true,
        Action::SetTab(model::Tab::Browse),
    )];
    let bundle = app.bundle.clone();
    framed(app, ui, rect, buttons, Vec::new(), |app, ui| {
        title(ui, "Your quest log is empty.");
        rule(ui);
        body(
            ui,
            "Nothing's installed yet. Browse the catalog, or start with a kit of addons that work well together.",
        );
        ui.add_space(10.0);
        let kits = bundle.as_deref().map_or(&[][..], |b| &b.catalog.kits[..]);
        if !kits.is_empty() {
            widgets::section(ui, "Starter kits");
            for (i, kit) in kits.iter().enumerate() {
                let tag = format!("{} addons", kit.addons.len());
                let row = widgets::ink_row(ui, &kit.name, theme::INK, &tag, theme::INK_SOFT);
                if row.clicked() {
                    app.actions.push(Action::GoToKit(i));
                }
            }
        }
    });
}

pub fn no_game_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let buttons = vec![
        BarButton::main("Choose folder", true, Action::ChooseFolder),
        BarButton::quiet("Look again", true, Action::LookAgain),
    ];
    framed(app, ui, rect, buttons, Vec::new(), |_, ui| {
        title(ui, "Couldn't find WoW Forever.");
        rule(ui);
        body(ui, "Here's what was checked:");
        ui.add_space(4.0);
        for place in model::places_checked() {
            body(ui, &format!("  -  {place}"));
        }
        ui.add_space(10.0);
        body(
            ui,
            "If the game is somewhere else, choose its folder. Browsing addons still works, and installing asks for the folder first.",
        );
    });
}

fn no_catalog_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let offline = matches!(app.catalog_status, CatalogStatus::Missing { offline: true });
    let buttons = vec![BarButton::main("Try again", true, Action::RetryCatalog)];
    framed(app, ui, rect, buttons, Vec::new(), |_, ui| {
        if offline {
            title(ui, "You're offline.");
            rule(ui);
            body(
                ui,
                "The list of addons comes from the internet and there's no saved copy yet, so there's nothing to browse for now. It only takes one connection: after that, the last list is kept for times like this.",
            );
        } else {
            title(ui, "No catalog yet.");
            rule(ui);
            body(
                ui,
                "The list of addons hasn't been published yet, or couldn't be read, so there's nothing to browse for now.",
            );
        }
        ui.add_space(10.0);
        body(
            ui,
            "Addons you already have still show under My Addons, and you can drop a downloaded zip on this window to install it.",
        );
    });
}

fn loading_page(app: &mut App, ui: &mut Ui, rect: Rect) {
    let message = match (app.tab, &app.game_state) {
        (model::Tab::MyAddons, GameState::Searching) => "Looking for WoW Forever...",
        (model::Tab::MyAddons, _) => "Reading your AddOns folder...",
        _ => "Fetching the list of addons...",
    };
    note_page(app, ui, rect, message);
}

fn note_page(app: &mut App, ui: &mut Ui, rect: Rect, text: &str) {
    framed(app, ui, rect, Vec::new(), Vec::new(), |_, ui| {
        ui.add_space(8.0);
        widgets::ink(ui, text, theme::label(16.0), theme::INK_SOFT);
    });
}

#[cfg(test)]
mod tests {
    use super::flow;

    #[test]
    fn buttons_wrap_only_when_they_must() {
        assert_eq!(flow(&[100.0, 100.0], 300.0, 6.0), vec![0, 0]);
        assert_eq!(flow(&[100.0, 100.0, 100.0], 300.0, 6.0), vec![0, 0, 1]);
        assert_eq!(flow(&[400.0, 100.0], 300.0, 6.0), vec![0, 1]);
        assert!(flow(&[], 300.0, 6.0).is_empty());
    }
}
