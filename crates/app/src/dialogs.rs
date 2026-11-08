//! Static popups: dark fill, gold border, near the top, yellow text, red buttons. One at
//! a time, and Escape cancels.

use crate::actions::Action;
use crate::app::App;
use crate::model::size_text;
use crate::ops::{self, FolderConflict, Verb};
use crate::theme;
use crate::widgets::{self, ButtonKind};
use crate::worker::{DirListing, FoundGame};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Key, Margin, Modal, Modifiers, RichText,
    ScrollArea, Shadow, Stroke, TextEdit, Ui, UiBuilder, vec2,
};
use feeling_core::catalog::Channel;
use feeling_core::selfupdate::UpdateInfo;
use std::path::{Path, PathBuf};

/// What a removal will touch, worked out when the popup opens. What it really deletes is
/// worked out again when it starts, in case an install queued ahead of it shares a folder.
#[derive(Clone, Debug)]
pub struct RemovePlan {
    /// The managed record that goes away with the folders, if there is one.
    pub addon_id: Option<String>,
    pub name: String,
    pub folders: Vec<String>,
    pub dependents: Vec<String>,
}

/// What an install would take from something else, worked out when the popup opens.
#[derive(Clone, Debug)]
pub struct ReplacePlan {
    pub addon_id: String,
    pub name: String,
    /// Who it's from, because a display name isn't unique and this is.
    pub source: String,
    pub verb: Verb,
    pub conflicts: Vec<FolderConflict>,
    /// Packs the player already chose to install with it, which follow it into the queue.
    pub packs: Vec<String>,
}

impl ReplacePlan {
    /// Saying yes to this.
    pub fn confirmed(&self) -> Action {
        Action::ReplaceAndInstall {
            id: self.addon_id.clone(),
            folders: self.conflicts.iter().map(|c| c.folder.clone()).collect(),
            packs: self.packs.clone(),
        }
    }
}

/// What a first install would leave without, worked out when the popup opens: the packs an
/// addon needs, such as Spoken's quest voices, when nothing here or on its way provides one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackPlan {
    pub addon_id: String,
    pub name: String,
    /// Each needed group that nothing meets, by its name, such as "Quest voices".
    pub needs: Vec<String>,
    /// The first pack of each of those groups, as `(id, name)`, without repeats.
    pub packs: Vec<(String, String)>,
    /// Their downloads together, in bytes, or `None` when one of them doesn't say.
    pub size: Option<u64>,
}

impl PackPlan {
    /// "Spoken needs quest voices to work."
    pub fn title(&self) -> String {
        let needs: Vec<String> = self.needs.iter().map(|need| in_sentence(need)).collect();
        format!("{} needs {} to work.", self.name, ops::short_list(&needs))
    }

    /// "Spoken Audio: Quests (English) is a 1.5 GB download. Install it too?"
    pub fn question(&self) -> String {
        let names: Vec<&str> = self.packs.iter().map(|(_, name)| name.as_str()).collect();
        let packs = ops::short_list(&names);
        match (names.len() > 1, self.size) {
            (false, Some(size)) => {
                format!("{packs} is a {} download. Install it too?", size_text(size))
            }
            (false, None) => format!("{packs} is a separate download. Install it too?"),
            (true, Some(size)) => {
                format!(
                    "{packs} are a {} download. Install them too?",
                    size_text(size)
                )
            }
            (true, None) => format!("{packs} are separate downloads. Install them too?"),
        }
    }

    /// The main button, which installs the addon and every pack offered. With several, it
    /// answers the question in its own words.
    pub fn yes_label(&self) -> &'static str {
        if self.packs.len() > 1 {
            "Install them too"
        } else {
            "Install both"
        }
    }

    /// "Just Spoken"
    pub fn no_label(&self) -> String {
        format!("Just {}", self.name)
    }

    pub fn with_packs(&self) -> Action {
        Action::InstallWithPacks {
            id: self.addon_id.clone(),
            packs: self.packs.iter().map(|(id, _)| id.clone()).collect(),
        }
    }

    pub fn without_packs(&self) -> Action {
        Action::InstallWithPacks {
            id: self.addon_id.clone(),
            packs: Vec::new(),
        }
    }
}

/// A group's name as it reads inside a sentence: "Quest voices" becomes "quest voices". One
/// that starts with an acronym, such as "UI skins", keeps it.
fn in_sentence(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    if chars.next().is_some_and(char::is_uppercase) {
        return name.to_owned();
    }
    first
        .to_lowercase()
        .chain(name[first.len_utf8()..].chars())
        .collect()
}

/// What a pasted list would do, as installing it would work it out.
#[derive(Clone, Debug, Default)]
pub struct ListPreview {
    /// The listed addons installing it adds, which is what Install these passes on.
    pub add: Vec<ListAddon>,
    /// What those require that the list doesn't name and that isn't here or on its way.
    /// Installing them brings these along by itself, so they aren't passed on.
    pub brings: Vec<ListDep>,
    /// Addons the player has that installing those updates, or reinstalls, by itself, because
    /// they require a folder only the catalog's build has.
    pub updates: Vec<ListUpdate>,
    pub have: Vec<String>,
    /// Listed addons already on their way, which installing it doesn't add again.
    pub coming: Vec<String>,
    /// Installed addons it names whose entries have become other addons, each as the name
    /// the player knows it by and the addon the catalog lists now. Installing the list leaves
    /// these alone.
    pub left_alone: Vec<(String, String)>,
    /// Listed addons that a list of several leaves out because they would replace a folder,
    /// each with the folders it would replace. A list of several never asks about that.
    pub sitting_out: Vec<(String, String)>,
    /// What installing it would say about what those addons require and don't get, and why.
    pub notes: Vec<String>,
    pub unknown: Vec<String>,
}

impl ListPreview {
    /// How many addons installing the list adds, with what they bring along. An update of
    /// one the player has adds nothing.
    pub fn count(&self) -> usize {
        self.add.len() + self.brings.len()
    }

    /// What installing the list downloads, updates included, for under the list: "1.6 GB to
    /// download in all." One whose size the catalog doesn't give makes it "At least". `None`
    /// when no size is known.
    pub fn total(&self) -> Option<String> {
        let addons = || {
            self.add
                .iter()
                .chain(self.brings.iter().map(|dep| &dep.addon))
                .chain(self.updates.iter().map(|update| &update.addon))
        };
        // Sizes come from the catalog, so a silly one mustn't overflow the sum.
        let total = addons().fold(0u64, |total, addon| total.saturating_add(addon.size));
        if total == 0 {
            return None;
        }
        let total = size_text(total);
        Some(if addons().all(|addon| addon.size > 0) {
            format!("{total} to download in all.")
        } else {
            format!("At least {total} to download in all.")
        })
    }
}

/// An addon a pasted list brings along, because one of the addons it adds requires it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListDep {
    pub addon: ListAddon,
    /// The listed addon it comes with, by name.
    pub needed_by: String,
}

impl ListDep {
    /// Its line under the list: "Also installs Musician (101.8 MB), which Musician List
    /// requires."
    pub fn line(&self) -> String {
        format!(
            "Also installs {}, which {} requires.",
            self.addon.line(),
            self.needed_by
        )
    }
}

/// An addon the player has that installing a pasted list updates, or reinstalls, because one
/// of the addons it adds requires a folder only the catalog's build of it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListUpdate {
    /// Its download, from its own channel.
    pub addon: ListAddon,
    /// The version it moves to, or `None` when that's the one the player has, which
    /// reinstalls it.
    pub to: Option<String>,
    /// The listed addon it comes with, by name.
    pub needed_by: String,
}

impl ListUpdate {
    /// Its line under the list: "Also updates Musician to 2.0 (101.8 MB), which Musician
    /// List requires."
    pub fn line(&self) -> String {
        let what = match &self.to {
            Some(version) => format!(
                "updates {}",
                self.addon
                    .with_size(&format!("{} to {version}", self.addon.name))
            ),
            None => format!("reinstalls {}", self.addon.line()),
        };
        format!("Also {what}, which {} requires.", self.needed_by)
    }
}

/// The note for an installed addon a list names whose entry has become another addon, in the
/// words its page uses: "Left alone: ShortKings, since the catalog now lists a different addon
/// in its place, ShortRoyalty."
pub fn left_alone_line(yours: &str, listed: &str) -> String {
    format!(
        "Left alone: {yours}, since the catalog now lists a different addon in its place, \
         {listed}."
    )
}

/// The note for the listed addons already on their way: "Already on its way: Musician."
pub fn coming_line(names: &[String]) -> String {
    let way = if names.len() > 1 {
        "their way"
    } else {
        "its way"
    };
    format!("Already on {way}: {}.", names.join(", "))
}

/// The note for an addon a list of several leaves out, as it would replace a folder:
/// "Left alone: Musician, which would replace Musician. Install it from its own page to
/// choose."
pub fn sitting_out_line(name: &str, folders: &str) -> String {
    format!(
        "Left alone: {name}, which would replace {folders}. Install it from its own page to choose."
    )
}

/// One addon a pasted list would add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListAddon {
    pub id: String,
    pub name: String,
    /// The channel it installs from: the one the list asks for, or for an addon the list
    /// brings along, its own.
    pub channel: Channel,
    /// What that channel's build downloads, in bytes. 0 when the catalog doesn't say.
    pub size: u64,
}

impl ListAddon {
    /// Its line in the list: "Spoken Audio: Quests (English) (1.5 GB)", or "(beta, 1.5 GB)"
    /// from the beta channel.
    pub fn line(&self) -> String {
        self.with_size(&self.name)
    }

    /// `what`, followed by its channel and download size the way its line gives them.
    fn with_size(&self, what: &str) -> String {
        let size = (self.size > 0).then(|| size_text(self.size));
        match (self.channel, size) {
            (Channel::Stable, None) => what.to_owned(),
            (Channel::Stable, Some(size)) => format!("{what} ({size})"),
            (Channel::Beta, None) => format!("{what} (beta)"),
            (Channel::Beta, Some(size)) => format!("{what} (beta, {size})"),
        }
    }
}

pub enum Popup {
    AlreadyRunning,
    Remove(RemovePlan),
    Replace(ReplacePlan),
    Packs(PackPlan),
    AppUpdate(Box<UpdateInfo>),
    Restore { zip: PathBuf, when: String },
    Folder(Box<FolderBrowser>),
    List(ListPreview),
}

/// The built-in folder browser. Everything it knows came from a worker.
#[derive(Default)]
pub struct FolderBrowser {
    /// What's in the path line, which the player can type over.
    pub typed: String,
    pub listing: Option<DirListing>,
    pub waiting_for: Option<PathBuf>,
}

impl FolderBrowser {
    pub fn usable(&self) -> Option<&FoundGame> {
        self.listing.as_ref()?.verdict.as_ref()?.as_ref().ok()
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(popup) = app.popup.take() else {
        return;
    };
    // There's nothing behind this one to go back to.
    let escape = !matches!(popup, Popup::AlreadyRunning)
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let mut keep = !escape;
    let width: f32 = match &popup {
        Popup::Folder(_) => 560.0,
        Popup::AppUpdate(_) | Popup::List(_) | Popup::Replace(_) | Popup::Packs(_) => 460.0,
        _ => 400.0,
    };
    let mut popup = popup;
    let frame = Frame::new()
        .fill(Color32::from_rgba_premultiplied(12, 10, 8, 250))
        .stroke(Stroke::new(2.0, theme::GOLD_DIM))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(16))
        .shadow(Shadow {
            offset: [0, 4],
            blur: 18,
            spread: 2,
            color: Color32::from_black_alpha(180),
        });
    let id = Id::unique("static-popup");
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, 92.0));
    Modal::new(id)
        .area(area)
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(110))
        .show(ctx, |ui| {
            let screen = ui.ctx().content_rect();
            ui.set_width(width.min(screen.width() - 72.0));
            match &mut popup {
                Popup::AlreadyRunning => already_running(ui, ctx),
                Popup::Remove(plan) => remove(ui, app, plan, &mut keep),
                Popup::Replace(plan) => replace(ui, app, plan, &mut keep),
                Popup::Packs(plan) => packs(ui, app, plan, &mut keep),
                Popup::AppUpdate(info) => app_update(ui, app, info, &mut keep),
                Popup::Restore { zip, when } => restore(ui, app, zip, when, &mut keep),
                Popup::Folder(browser) => folder(ui, app, browser, &mut keep),
                Popup::List(preview) => list(ui, app, preview, &mut keep),
            }
        });
    if escape && matches!(popup, Popup::Folder(_)) {
        app.actions.push(Action::CancelFolder);
    }
    // A button may have opened another one while this one was out of its slot.
    if keep {
        if let Some(opened) = app.popup.replace(popup) {
            app.show_popup(opened);
        }
    } else if app.popup.is_none() {
        app.popup = app.next_popup();
    }
    if !keep {
        ctx.request_repaint();
    }
}

fn yellow(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::strong(15.5))
                .color(theme::SYSTEM_YELLOW),
        )
        .wrap(),
    );
}

fn plain(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::body(14.0))
                .color(theme::TEXT),
        )
        .wrap(),
    );
}

/// A row of buttons, centered, the way static popups always had them.
fn button_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
            add(ui);
        });
    });
}

fn already_running(ui: &mut Ui, ctx: &egui::Context) {
    yellow(ui, "WoW is a Feeling is already running.");
    ui.add_space(4.0);
    plain(
        ui,
        "Look for its window. Only one can run at a time, so nothing gets installed twice.",
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Close", ButtonKind::Main, true).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

fn remove(ui: &mut Ui, app: &mut App, plan: &RemovePlan, keep: &mut bool) {
    yellow(
        ui,
        &format!(
            "Remove {}? Its folders are deleted. Your saved settings in WTF stay.",
            plan.name
        ),
    );
    if !plan.dependents.is_empty() {
        ui.add_space(6.0);
        plain(
            ui,
            &format!(
                "These installed addons require it and may stop working: {}.",
                plan.dependents.join(", ")
            ),
        );
    }
    if !plan.folders.is_empty() {
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!("Folders: {}", plan.folders.join(", ")))
                    .font(theme::body(13.0))
                    .color(theme::TEXT_DIM),
            )
            .wrap(),
        );
    }
    button_row(ui, |ui| {
        if widgets::button(ui, "Remove", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::ConfirmRemove(plan.clone()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

fn replace(ui: &mut Ui, app: &mut App, plan: &ReplacePlan, keep: &mut bool) {
    yellow(
        ui,
        &format!(
            "{} {} would replace these folders already in your AddOns:",
            plan.verb.gerund(),
            plan.name
        ),
    );
    ui.add_space(6.0);
    ScrollArea::vertical()
        .id_salt("replace-list")
        .max_height(160.0)
        .show(ui, |ui| {
            // A real addon never gets near this. A strange catalog entry could.
            const MOST_SHOWN: usize = 40;
            for conflict in plan.conflicts.iter().take(MOST_SHOWN) {
                plain(ui, &conflict.describe());
            }
            if plan.conflicts.len() > MOST_SHOWN {
                plain(
                    ui,
                    &format!("and {} more", plan.conflicts.len() - MOST_SHOWN),
                );
            }
        });
    ui.add_space(6.0);
    plain(
        ui,
        "What's in them now gets deleted, and anything that needs them may stop working until you install it again. Your saved settings in WTF stay.",
    );
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new(format!("Source: {}", plan.source))
                .font(theme::body(13.0))
                .color(theme::TEXT_DIM),
        )
        .wrap(),
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Replace them", ButtonKind::Main, true).clicked() {
            app.actions.push(plan.confirmed());
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

/// Asked once per click, before a first install of an addon that needs a pack. Escape is
/// Cancel, as on every popup, so nothing at all gets installed.
fn packs(ui: &mut Ui, app: &mut App, plan: &PackPlan, keep: &mut bool) {
    yellow(ui, &plan.title());
    ui.add_space(4.0);
    plain(ui, &plan.question());
    button_row(ui, |ui| {
        if widgets::button(ui, plan.yes_label(), ButtonKind::Main, true).clicked() {
            app.actions.push(plan.with_packs());
            *keep = false;
        }
        if widgets::button(ui, &plan.no_label(), ButtonKind::Quiet, true).clicked() {
            app.actions.push(plan.without_packs());
            *keep = false;
        }
    });
}

fn app_update(ui: &mut Ui, app: &mut App, info: &UpdateInfo, keep: &mut bool) {
    yellow(ui, &format!("Version {} is available.", info.version));
    plain(
        ui,
        &format!("(You have {}.)", crate::views::settings::VERSION),
    );
    if !info.notes.is_empty() {
        ui.add_space(8.0);
        let height = (ui.ctx().content_rect().height() - 320.0).clamp(60.0, 150.0);
        sunken_list(ui, "update-notes", height, Color32::BLACK, |ui| {
            plain(ui, &crate::model::clean_paragraphs(&info.notes));
        });
    }
    button_row(ui, |ui| {
        if widgets::button(ui, "Update now", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::UpdateNow(Box::new(info.clone())));
            *keep = false;
        }
        if widgets::button(ui, "Later", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
        if widgets::button(ui, "Skip this version", ButtonKind::Quiet, true).clicked() {
            app.actions.push(Action::SkipVersion(info.version.clone()));
            *keep = false;
        }
    });
}

fn restore(ui: &mut Ui, app: &mut App, zip: &Path, when: &str, keep: &mut bool) {
    yellow(ui, &format!("Restore your settings from {when}?"));
    ui.add_space(4.0);
    plain(
        ui,
        "Your current WTF folder is kept beside it under a new name. This can't run while the game is open.",
    );
    button_row(ui, |ui| {
        if widgets::button(ui, "Restore", ButtonKind::Main, true).clicked() {
            app.actions.push(Action::ConfirmRestore(zip.to_path_buf()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

fn folder(ui: &mut Ui, app: &mut App, browser: &mut FolderBrowser, keep: &mut bool) {
    yellow(ui, "Choose the World of Warcraft folder");
    ui.add_space(6.0);

    let parent = browser.listing.as_ref().and_then(|l| l.parent.clone());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let up = widgets::button(ui, "Up", ButtonKind::Quiet, parent.is_some());
        if up.clicked()
            && let Some(parent) = &parent
        {
            app.actions.push(Action::BrowseTo(parent.clone()));
        }
        let room = (ui.available_width() - 2.0).max(60.0);
        let edit = TextEdit::singleline(&mut browser.typed)
            .id_salt("folder-path")
            .font(theme::body(14.0))
            .text_color(Color32::WHITE)
            .frame(Frame::NONE);
        let response = widgets::well_edit(ui, edit, vec2(room, 26.0));
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            app.actions
                .push(Action::BrowseTo(PathBuf::from(browser.typed.trim())));
        }
    });

    let drives = browser
        .listing
        .as_ref()
        .map(|l| l.drives.clone())
        .unwrap_or_default();
    if !drives.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for drive in drives {
                let label = drive.display().to_string();
                if drive_button(ui, &label).clicked() {
                    app.actions.push(Action::BrowseTo(drive));
                }
            }
        });
    }
    ui.add_space(6.0);

    // Room for the list without running off a small window.
    let screen = ui.ctx().content_rect().height();
    let list_height = (screen - 340.0).clamp(90.0, 280.0);
    sunken_list(ui, "folder-list", list_height, theme::WELL, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        match &browser.listing {
            None => plain(ui, "Reading..."),
            Some(listing) => {
                if let Some(error) = &listing.error {
                    plain(ui, error);
                } else if listing.folders.is_empty() {
                    plain(ui, "No folders in here.");
                }
                for name in &listing.folders {
                    if folder_row(ui, name).clicked() {
                        app.actions.push(Action::BrowseTo(listing.path.join(name)));
                    }
                }
            }
        }
    });
    ui.add_space(8.0);

    let usable = browser.usable().cloned();
    if let Some(game) = &usable {
        let version = game.install.version.as_deref().unwrap_or("unknown version");
        ui.add(
            egui::Label::new(
                RichText::new(format!("WoW Forever found here ({version})."))
                    .font(theme::strong(14.0))
                    .color(theme::USE_GREEN),
            )
            .wrap(),
        );
    } else if let Some(Some(Err(why))) = browser.listing.as_ref().map(|l| l.verdict.as_ref()) {
        ui.add(
            egui::Label::new(
                RichText::new(why.as_str())
                    .font(theme::body(13.0))
                    .color(theme::TEXT_DIM),
            )
            .wrap(),
        );
    }
    button_row(ui, |ui| {
        let use_it = widgets::button(ui, "Use this folder", ButtonKind::Main, usable.is_some());
        if use_it.clicked()
            && let Some(game) = usable
        {
            app.actions.push(Action::UseFolder(Box::new(game)));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            app.actions.push(Action::CancelFolder);
            *keep = false;
        }
    });
}

/// A sunken box of exactly this height, which scrolls whatever doesn't fit in it.
fn sunken_list(ui: &mut Ui, id: &str, height: f32, fill: Color32, content: impl FnOnce(&mut Ui)) {
    let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), height));
    crate::skin::well(ui.painter(), rect, fill);
    ui.scope_builder(
        UiBuilder::new().max_rect(rect.shrink2(vec2(6.0, 4.0))),
        |ui| {
            ScrollArea::vertical()
                .id_salt(id)
                .auto_shrink(false)
                .show(ui, content);
        },
    );
    // A scope only reserves what its content used, and this box is always the same size.
    ui.advance_cursor_after_rect(rect);
}

fn drive_button(ui: &mut Ui, label: &str) -> egui::Response {
    let size = vec2(44.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Drive {label}"))
    });
    let fill = if response.hovered() {
        Color32::from_rgb(60, 46, 30)
    } else {
        Color32::from_rgb(36, 28, 18)
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    ui.painter().rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, theme::GOLD_LINE),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::strong(13.0),
        theme::TEXT,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn folder_row(ui: &mut Ui, name: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 22.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter()
                .rect_filled(rect, 0.0, Color32::from_white_alpha(14));
        }
        crate::skin::shadow_text_fit(
            ui.painter(),
            egui::pos2(rect.min.x + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            theme::body(14.5),
            theme::TEXT,
            width - 12.0,
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn list(ui: &mut Ui, app: &mut App, preview: &ListPreview, keep: &mut bool) {
    yellow(
        ui,
        &match preview.count() {
            0 => "This list wouldn't add anything.".to_owned(),
            1 => "This list adds 1 addon:".to_owned(),
            n => format!("This list adds {n} addons:"),
        },
    );
    ui.add_space(6.0);
    ScrollArea::vertical()
        .id_salt("list-preview")
        .max_height(200.0)
        .show(ui, |ui| {
            for addon in &preview.add {
                plain(ui, &addon.line());
            }
            if !preview.brings.is_empty() || !preview.updates.is_empty() {
                ui.add_space(6.0);
                for dep in &preview.brings {
                    plain(ui, &dep.line());
                }
                for update in &preview.updates {
                    plain(ui, &update.line());
                }
            }
            if !preview.notes.is_empty() {
                ui.add_space(6.0);
                for note in &preview.notes {
                    plain(ui, note);
                }
            }
            if !preview.have.is_empty() {
                ui.add_space(6.0);
                plain(
                    ui,
                    &format!("Already installed: {}.", preview.have.join(", ")),
                );
            }
            if !preview.coming.is_empty() {
                ui.add_space(6.0);
                plain(ui, &coming_line(&preview.coming));
            }
            if !preview.left_alone.is_empty() || !preview.sitting_out.is_empty() {
                ui.add_space(6.0);
                for (yours, listed) in &preview.left_alone {
                    plain(ui, &left_alone_line(yours, listed));
                }
                for (name, folders) in &preview.sitting_out {
                    plain(ui, &sitting_out_line(name, folders));
                }
            }
            if !preview.unknown.is_empty() {
                ui.add_space(6.0);
                plain(
                    ui,
                    &format!("Not in the catalog: {}.", preview.unknown.join(", ")),
                );
            }
        });
    // Outside the scrolling part, so a long list can't hide it.
    if let Some(total) = preview.total() {
        ui.add_space(6.0);
        plain(ui, &total);
    }
    button_row(ui, |ui| {
        let any = !preview.add.is_empty();
        if widgets::button(ui, "Install these", ButtonKind::Main, any).clicked() {
            // What the listed addons require comes along by itself, as for any install.
            app.actions.push(Action::ConfirmList(preview.add.clone()));
            *keep = false;
        }
        if widgets::button(ui, "Cancel", ButtonKind::Quiet, true).clicked() {
            *keep = false;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(needs: &[&str], packs: &[(&str, &str)], size: Option<u64>) -> PackPlan {
        PackPlan {
            addon_id: "gh:rusty-key/spoken-wow".to_owned(),
            name: "Spoken".to_owned(),
            needs: needs.iter().map(|n| (*n).to_owned()).collect(),
            packs: packs
                .iter()
                .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
                .collect(),
            size,
        }
    }

    const QUESTS: (&str, &str) = (
        "gh:rusty-key/spoken-wow/quests-audio",
        "Spoken Audio: Quests",
    );
    const BOSSES: (&str, &str) = (
        "gh:rusty-key/spoken-wow/bosses-audio",
        "Spoken Audio: Bosses",
    );

    #[test]
    fn one_missing_pack_is_asked_about_by_name_and_size() {
        let one = plan(&["Quest voices"], &[QUESTS], Some(1_479_966_451));
        assert_eq!(one.title(), "Spoken needs quest voices to work.");
        assert_eq!(
            one.question(),
            "Spoken Audio: Quests is a 1.5 GB download. Install it too?"
        );
        assert_eq!(one.yes_label(), "Install both");
        assert_eq!(one.no_label(), "Just Spoken");
        assert!(matches!(
            one.with_packs(),
            Action::InstallWithPacks { id, packs }
                if id == "gh:rusty-key/spoken-wow" && packs == [QUESTS.0]
        ));
        assert!(matches!(
            one.without_packs(),
            Action::InstallWithPacks { packs, .. } if packs.is_empty()
        ));
    }

    #[test]
    fn several_missing_packs_are_named_and_added_up() {
        let two = plan(
            &["Quest voices", "Boss voices"],
            &[QUESTS, BOSSES],
            Some(1_479_966_451 + 157_070_901),
        );
        assert_eq!(
            two.title(),
            "Spoken needs quest voices and boss voices to work."
        );
        assert_eq!(
            two.question(),
            "Spoken Audio: Quests and Spoken Audio: Bosses are a 1.6 GB download. \
             Install them too?"
        );
        assert_eq!(two.yes_label(), "Install them too");
        assert!(matches!(
            two.with_packs(),
            Action::InstallWithPacks { packs, .. } if packs == [QUESTS.0, BOSSES.0]
        ));
    }

    #[test]
    fn a_pack_of_unknown_size_is_still_asked_about_honestly() {
        let unknown = plan(&["Quest voices"], &[QUESTS], None);
        assert_eq!(
            unknown.question(),
            "Spoken Audio: Quests is a separate download. Install it too?"
        );
        let both = plan(&["Quest voices", "Boss voices"], &[QUESTS, BOSSES], None);
        assert_eq!(
            both.question(),
            "Spoken Audio: Quests and Spoken Audio: Bosses are separate downloads. \
             Install them too?"
        );
    }

    fn listed(name: &str, channel: Channel, size: u64) -> ListAddon {
        ListAddon {
            id: format!("gh:someone/{name}"),
            name: name.to_owned(),
            channel,
            size,
        }
    }

    #[test]
    fn a_list_shows_each_addons_download_after_its_name() {
        let pack = listed(
            "Spoken Audio: Quests (English)",
            Channel::Stable,
            1_479_966_451,
        );
        assert_eq!(pack.line(), "Spoken Audio: Quests (English) (1.5 GB)");
        let beta = listed("GatherMate2", Channel::Beta, 3_050_000);
        assert_eq!(beta.line(), "GatherMate2 (beta, 3.1 MB)");
        // A size the catalog doesn't give says nothing, rather than "1 KB".
        assert_eq!(listed("Foo", Channel::Stable, 0).line(), "Foo");
        assert_eq!(listed("Foo", Channel::Beta, 0).line(), "Foo (beta)");
    }

    #[test]
    fn a_list_adds_up_what_it_downloads_and_says_when_that_is_only_part_of_it() {
        let mut preview = ListPreview {
            add: vec![
                listed("Spoken", Channel::Stable, 72_000_000),
                listed(
                    "Spoken Audio: Quests (English)",
                    Channel::Stable,
                    1_479_966_451,
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            preview.total().as_deref(),
            Some("1.6 GB to download in all.")
        );
        preview.add.push(listed("Foo", Channel::Stable, 0));
        assert_eq!(
            preview.total().as_deref(),
            Some("At least 1.6 GB to download in all.")
        );
        preview.add.push(listed("Silly", Channel::Stable, u64::MAX));
        assert!(preview.total().is_some(), "a silly size can't overflow");
        assert_eq!(ListPreview::default().total(), None);
        let unknown = ListPreview {
            add: vec![listed("Foo", Channel::Stable, 0)],
            ..Default::default()
        };
        assert_eq!(unknown.total(), None, "nothing to add up");
    }

    fn brought(name: &str, size: u64, needed_by: &str) -> ListDep {
        ListDep {
            addon: listed(name, Channel::Stable, size),
            needed_by: needed_by.to_owned(),
        }
    }

    #[test]
    fn what_a_list_brings_along_gets_its_own_line_and_counts_in_the_total() {
        // The live catalog's sizes: Musician List requires Musician.
        let mut preview = ListPreview {
            add: vec![listed("Musician List", Channel::Stable, 87_310)],
            brings: vec![brought("Musician", 101_799_121, "Musician List")],
            ..Default::default()
        };
        assert_eq!(
            preview.brings[0].line(),
            "Also installs Musician (101.8 MB), which Musician List requires."
        );
        assert_eq!(preview.count(), 2);
        assert_eq!(
            preview.total().as_deref(),
            Some("101.9 MB to download in all.")
        );
        preview
            .brings
            .push(brought("Somebody's Library", 0, "Musician List"));
        assert_eq!(
            preview.brings[1].line(),
            "Also installs Somebody's Library, which Musician List requires."
        );
        assert_eq!(
            preview.total().as_deref(),
            Some("At least 101.9 MB to download in all."),
            "a size the catalog doesn't give counts here too"
        );
        preview.brings[1].addon.channel = Channel::Beta;
        assert_eq!(
            preview.brings[1].line(),
            "Also installs Somebody's Library (beta), which Musician List requires."
        );
    }

    #[test]
    fn an_update_a_list_brings_gets_its_own_line_and_counts_in_the_total_but_not_the_heading() {
        let update = |to: Option<&str>, channel: Channel, size: u64| ListUpdate {
            addon: listed("Musician", channel, size),
            to: to.map(str::to_owned),
            needed_by: "Musician List".to_owned(),
        };
        assert_eq!(
            update(Some("2.0"), Channel::Stable, 101_799_121).line(),
            "Also updates Musician to 2.0 (101.8 MB), which Musician List requires."
        );
        assert_eq!(
            update(Some("2.0-beta"), Channel::Beta, 101_799_121).line(),
            "Also updates Musician to 2.0-beta (beta, 101.8 MB), which Musician List requires."
        );
        assert_eq!(
            update(Some("2.0"), Channel::Stable, 0).line(),
            "Also updates Musician to 2.0, which Musician List requires."
        );
        assert_eq!(
            update(None, Channel::Stable, 101_799_121).line(),
            "Also reinstalls Musician (101.8 MB), which Musician List requires."
        );
        let preview = ListPreview {
            add: vec![listed("Musician List", Channel::Stable, 87_310)],
            updates: vec![update(Some("2.0"), Channel::Stable, 101_799_121)],
            ..Default::default()
        };
        assert_eq!(preview.count(), 1, "an update adds no addon");
        assert_eq!(
            preview.total().as_deref(),
            Some("101.9 MB to download in all.")
        );
    }

    #[test]
    fn an_entry_a_list_leaves_alone_says_why_in_its_own_note() {
        assert_eq!(
            left_alone_line("ShortKings", "ShortRoyalty"),
            "Left alone: ShortKings, since the catalog now lists a different addon in its \
             place, ShortRoyalty."
        );
        assert_eq!(
            sitting_out_line("Musician", "Musician"),
            "Left alone: Musician, which would replace Musician. Install it from its own page \
             to choose."
        );
        assert_eq!(
            coming_line(&["Musician".to_owned()]),
            "Already on its way: Musician."
        );
        assert_eq!(
            coming_line(&["Musician".to_owned(), "Foo".to_owned()]),
            "Already on their way: Musician, Foo."
        );
        let preview = ListPreview {
            left_alone: vec![("ShortKings".to_owned(), "ShortRoyalty".to_owned())],
            sitting_out: vec![("Musician".to_owned(), "Musician".to_owned())],
            coming: vec!["Foo".to_owned()],
            notes: vec!["Bar won't load without Baz, a 1.5 GB download.".to_owned()],
            ..Default::default()
        };
        assert_eq!(preview.count(), 0);
        assert_eq!(preview.total(), None);
    }

    #[test]
    fn a_group_name_reads_naturally_inside_a_sentence() {
        assert_eq!(in_sentence("Quest voices"), "quest voices");
        assert_eq!(in_sentence("UI skins"), "UI skins");
        assert_eq!(in_sentence("Ärger"), "ärger");
        assert_eq!(in_sentence("X"), "x");
        assert_eq!(in_sentence(""), "");
    }
}
