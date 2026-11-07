//! Pure state and row building. Nothing here touches a window, a disk or a clock, so
//! all of it can be tested without opening anything.

use feeling_core::catalog::{Addon, CATEGORIES, Catalog, Channel, Gap, Kit, Release, Source};
use feeling_core::deps::FolderIndex;
use feeling_core::scan::AddonGroup;
use feeling_core::search::{SearchIndex, SortBy};
use feeling_core::state::{ManagedAddon, Origin};
use std::collections::{BTreeMap, HashMap, HashSet};

/// How many results a search shows. Nobody reads further than this.
const SEARCH_LIMIT: usize = 500;

/// The interface number the tooltips quote.
pub const FOREVER_INTERFACE: u32 = feeling_core::toc::FOREVER_INTERFACE_MIN + 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    MyAddons,
    Browse,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Game,
    Backups,
    Lists,
    Display,
    Updates,
    About,
}

/// The Settings tab's list, in display order.
pub const SECTIONS: [(Section, &str); 6] = [
    (Section::Game, "Game folder"),
    (Section::Backups, "Backups"),
    (Section::Lists, "Addon lists"),
    (Section::Display, "Display"),
    (Section::Updates, "App updates"),
    (Section::About, "About"),
];

/// The section the arrow keys land on. They stop at the ends, as the lists do.
pub fn next_section(current: Section, down: bool) -> Section {
    let at = SECTIONS
        .iter()
        .position(|(section, _)| *section == current)
        .unwrap_or(0);
    let next = if down {
        (at + 1).min(SECTIONS.len() - 1)
    } else {
        at.saturating_sub(1)
    };
    SECTIONS[next].0
}

/// What the parchment shows. Holds ids rather than positions, so it survives a rebuild.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Selection {
    /// A catalog addon, or a managed addon that isn't in the catalog.
    Addon(String),
    /// An unmanaged group in AddOns, by its primary folder.
    Other(String),
    Kit(usize),
    Gap(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HeaderId {
    Kits,
    Recommended,
    Category(usize),
    Updates,
    Installed,
    Others,
    NotAvailable,
}

impl HeaderId {
    /// Categories start closed. Everything else starts open.
    fn open_by_default(self) -> bool {
        !matches!(self, HeaderId::Category(_))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Row {
    Header {
        id: HeaderId,
        name: &'static str,
        count: u32,
        open: bool,
    },
    /// A catalog addon, by its position in the catalog.
    Addon(u32),
    /// A managed addon, by id.
    Managed(String),
    /// An unmanaged group, by its position in the group list.
    Other(u32),
    Kit(u32),
    Gap(u32),
}

/// A catalog with everything prepared for the UI thread. Built on a worker.
pub struct CatalogBundle {
    pub catalog: Catalog,
    pub index: SearchIndex,
    pub folders: FolderIndex,
    pub by_id: HashMap<String, u32>,
    /// Each addon's place in [`CATEGORIES`].
    category_of: Vec<u8>,
}

impl CatalogBundle {
    pub fn new(mut catalog: Catalog) -> Self {
        sanitize(&mut catalog);
        let mut by_id = HashMap::with_capacity(catalog.addons.len());
        for (i, addon) in catalog.addons.iter().enumerate() {
            by_id.entry(addon.id.clone()).or_insert(i as u32);
        }
        let misc = CATEGORIES.len() - 1;
        let category_of = catalog
            .addons
            .iter()
            .map(|addon| {
                CATEGORIES
                    .iter()
                    .position(|(id, _)| *id == addon.category)
                    .unwrap_or(misc) as u8
            })
            .collect();
        Self {
            index: SearchIndex::build(&catalog),
            folders: FolderIndex::build(&catalog),
            by_id,
            category_of,
            catalog,
        }
    }

    pub fn addon(&self, id: &str) -> Option<&Addon> {
        let index = *self.by_id.get(id)? as usize;
        self.catalog.addons.get(index)
    }

    pub fn position(&self, id: &str) -> Option<u32> {
        self.by_id.get(id).copied()
    }
}

/// Catalog text comes from strangers. It only ever reaches the screen as plain text, but
/// a newline or a right-to-left override in a name would still wreck a row.
pub fn clean_line(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 2));
    let mut last_was_space = true;
    let mut kept = 0;
    for c in text.chars() {
        if kept == max_chars {
            break;
        }
        if is_format_control(c) {
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            if !last_was_space {
                out.push(' ');
                kept += 1;
                last_was_space = true;
            }
            continue;
        }
        out.push(c);
        kept += 1;
        last_was_space = false;
    }
    out.truncate(out.trim_end().len());
    out
}

/// The same treatment for text that's allowed to have paragraphs.
pub fn clean_paragraphs(text: &str) -> String {
    text.lines()
        .map(|line| clean_line(line, 2000))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_format_control(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn sanitize(catalog: &mut Catalog) {
    catalog.addons.retain(|addon| !addon.id.is_empty());
    for addon in &mut catalog.addons {
        addon.name = clean_line(&addon.name, 80);
        addon.author = clean_line(&addon.author, 80);
        addon.summary = clean_line(&addon.summary, 600);
        if addon.name.is_empty() {
            addon.name = addon
                .folders
                .first()
                .map_or_else(|| clean_line(&addon.id, 80), |f| clean_line(f, 80));
        }
        for release in [&mut addon.stable, &mut addon.beta].into_iter().flatten() {
            release.version = clean_line(&release.version, 40);
        }
        for group in &mut addon.packs {
            group.name = clean_line(&group.name, 60);
        }
    }
    for kit in &mut catalog.kits {
        kit.name = clean_line(&kit.name, 80);
        kit.description = clean_line(&kit.description, 600);
    }
    for gap in &mut catalog.gaps {
        gap.name = clean_line(&gap.name, 80);
        gap.reason = clean_line(&gap.reason, 400);
    }
}

/// Is this the same version, give or take a leading `v`? TOCs often say `12.6.2` where
/// the release is tagged `v12.6.2`.
pub fn same_version(a: &str, b: &str) -> bool {
    fn bare(v: &str) -> &str {
        let v = v.trim();
        v.strip_prefix(['v', 'V'])
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or(v)
    }
    bare(a).eq_ignore_ascii_case(bare(b))
}

/// Is this catalog entry still the addon the record installed? It is while it lists the
/// record's main folder, or names it in `formerly`, where the catalog vouches for a rename.
/// One that does neither has become another addon, such as one of its repository's voice
/// packs, and updating to it would swap this addon's folders for that one's.
pub fn same_addon(record: &ManagedAddon, addon: &Addon) -> bool {
    record.folders.first().is_none_or(|main| {
        addon
            .folders
            .iter()
            .chain(&addon.formerly)
            .any(|folder| crate::ops::same_folder(folder, main))
    })
}

/// What the page of an installed addon with no catalog entry to go by says about updates.
/// `listed` is what the catalog has under its id now, which is a different addon.
pub fn no_updates_note(record: &ManagedAddon, listed: Option<&Addon>) -> String {
    if record.origin == Origin::LocalZip {
        return "Installed from a zip file, so it doesn't get updates.".to_owned();
    }
    match listed {
        Some(addon) => format!(
            "The catalog now lists a different addon here, {}. Yours doesn't get updates from \
             it. To switch, remove yours, then install that one.",
            addon.name
        ),
        None => "This addon isn't in the catalog anymore, so it doesn't get updates.".to_owned(),
    }
}

/// The same, in the few words a row's tooltip has room for.
pub fn no_updates_tooltip(record: &ManagedAddon, listed: Option<&Addon>) -> &'static str {
    match (record.origin, listed) {
        (Origin::LocalZip, _) => "Installed from a zip file",
        (_, Some(_)) => "The catalog now lists a different addon here",
        (_, None) => "Not in the catalog anymore",
    }
}

/// The catalog entry an addon's page, row and updates go by. An installed addon whose entry
/// has become another addon has none, so nothing offers to turn it into that one.
pub fn entry_for<'a>(
    bundle: &'a CatalogBundle,
    id: &str,
    record: Option<&ManagedAddon>,
) -> Option<&'a Addon> {
    let addon = bundle.addon(id)?;
    record
        .is_none_or(|record| same_addon(record, addon))
        .then_some(addon)
}

/// The release a managed addon would move to, if it isn't on it already.
pub fn update_for<'a>(managed: &ManagedAddon, addon: &'a Addon) -> Option<&'a Release> {
    if managed.origin == Origin::LocalZip || !same_addon(managed, addon) {
        return None;
    }
    let release = addon.release(managed.channel)?;
    (!same_version(&managed.version, &release.version)).then_some(release)
}

/// Ids of managed addons with an update waiting, in name order. Pinned ones sit it out.
pub fn updates_available(
    managed: &BTreeMap<String, ManagedAddon>,
    bundle: Option<&CatalogBundle>,
) -> Vec<String> {
    let Some(bundle) = bundle else {
        return Vec::new();
    };
    let mut ids: Vec<&ManagedAddon> = managed
        .values()
        .filter(|m| !m.pinned)
        .filter(|m| {
            bundle
                .addon(&m.id)
                .is_some_and(|a| update_for(m, a).is_some())
        })
        .collect();
    ids.sort_by_cached_key(|m| m.name.to_lowercase());
    ids.into_iter().map(|m| m.id.clone()).collect()
}

fn header(id: HeaderId, name: &'static str, count: usize, toggled: &HashSet<HeaderId>) -> Row {
    Row::Header {
        id,
        name,
        count: count as u32,
        open: id.open_by_default() != toggled.contains(&id),
    }
}

fn is_open(row: &Row) -> bool {
    matches!(row, Row::Header { open: true, .. })
}

/// The text to search for. A leading `/` is a command being typed, not a search.
pub fn effective_query(text: &str) -> &str {
    let text = text.trim();
    if text.starts_with('/') { "" } else { text }
}

/// The Browse list.
pub fn browse_rows(
    bundle: &CatalogBundle,
    query: &str,
    sort: SortBy,
    toggled: &HashSet<HeaderId>,
) -> Vec<Row> {
    let query = effective_query(query);
    let catalog = &bundle.catalog;
    if !query.is_empty() {
        let mut rows: Vec<Row> = bundle
            .index
            .query(query, SEARCH_LIMIT)
            .into_iter()
            .map(|i| Row::Addon(i as u32))
            .collect();
        let lower = query.to_lowercase();
        let gaps: Vec<usize> = catalog
            .gaps
            .iter()
            .enumerate()
            .filter(|(_, gap)| gap.name.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect();
        if !gaps.is_empty() {
            let head = header(
                HeaderId::NotAvailable,
                "Not available here",
                gaps.len(),
                toggled,
            );
            let open = is_open(&head);
            rows.push(head);
            if open {
                rows.extend(gaps.into_iter().map(|i| Row::Gap(i as u32)));
            }
        }
        return rows;
    }

    let mut rows = Vec::new();
    if !catalog.kits.is_empty() {
        let head = header(HeaderId::Kits, "Starter kits", catalog.kits.len(), toggled);
        let open = is_open(&head);
        rows.push(head);
        if open {
            rows.extend((0..catalog.kits.len()).map(|i| Row::Kit(i as u32)));
        }
    }
    let featured: Vec<u32> = catalog
        .featured
        .iter()
        .filter_map(|id| bundle.position(id))
        .collect();
    if !featured.is_empty() {
        let head = header(
            HeaderId::Recommended,
            "Recommended",
            featured.len(),
            toggled,
        );
        let open = is_open(&head);
        rows.push(head);
        if open {
            rows.extend(featured.into_iter().map(Row::Addon));
        }
    }

    // One pass over the sorted catalog fills every bucket, in sorted order.
    let mut counts = [0usize; CATEGORIES.len()];
    for &category in &bundle.category_of {
        counts[usize::from(category)] += 1;
    }
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); CATEGORIES.len()];
    let open_category = |i: usize| {
        let id = HeaderId::Category(i);
        id.open_by_default() != toggled.contains(&id)
    };
    for addon in bundle.index.sorted(sort) {
        let category = usize::from(bundle.category_of[addon]);
        if open_category(category) {
            members[category].push(addon as u32);
        }
    }
    for (i, (_, name)) in CATEGORIES.iter().enumerate() {
        if counts[i] == 0 {
            continue;
        }
        rows.push(header(HeaderId::Category(i), name, counts[i], toggled));
        rows.extend(std::mem::take(&mut members[i]).into_iter().map(Row::Addon));
    }
    rows
}

/// The My Addons list.
pub fn my_rows(
    managed: &BTreeMap<String, ManagedAddon>,
    updates: &[String],
    groups: &[AddonGroup],
    query: &str,
    toggled: &HashSet<HeaderId>,
) -> Vec<Row> {
    let lower = effective_query(query).to_lowercase();
    let wanted = |name: &str| lower.is_empty() || name.to_lowercase().contains(&lower);
    let waiting: HashSet<&str> = updates.iter().map(String::as_str).collect();

    let mut sorted: Vec<&ManagedAddon> = managed.values().filter(|m| wanted(&m.name)).collect();
    sorted.sort_by_cached_key(|m| m.name.to_lowercase());
    let (update_rows, plain_rows): (Vec<&ManagedAddon>, Vec<&ManagedAddon>) = sorted
        .into_iter()
        .partition(|m| waiting.contains(m.id.as_str()));

    let mut rows = Vec::new();
    let push_managed = |id, name, list: Vec<&ManagedAddon>, rows: &mut Vec<Row>| {
        if list.is_empty() {
            return;
        }
        let head = header(id, name, list.len(), toggled);
        let open = is_open(&head);
        rows.push(head);
        if open {
            rows.extend(list.into_iter().map(|m| Row::Managed(m.id.clone())));
        }
    };
    push_managed(
        HeaderId::Updates,
        "Updates available",
        update_rows,
        &mut rows,
    );
    push_managed(HeaderId::Installed, "Installed", plain_rows, &mut rows);

    let others: Vec<usize> = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| wanted(&group.title) || wanted(&group.primary))
        .map(|(i, _)| i)
        .collect();
    if !others.is_empty() {
        let head = header(HeaderId::Others, "Other folders", others.len(), toggled);
        let open = is_open(&head);
        rows.push(head);
        if open {
            rows.extend(others.into_iter().map(|i| Row::Other(i as u32)));
        }
    }
    rows
}

/// The selection a row stands for. Headers have none.
pub fn row_selection(
    row: &Row,
    bundle: Option<&CatalogBundle>,
    groups: &[AddonGroup],
) -> Option<Selection> {
    match row {
        Row::Header { .. } => None,
        Row::Addon(i) => Some(Selection::Addon(
            bundle?.catalog.addons.get(*i as usize)?.id.clone(),
        )),
        Row::Managed(id) => Some(Selection::Addon(id.clone())),
        Row::Other(i) => Some(Selection::Other(groups.get(*i as usize)?.primary.clone())),
        Row::Kit(i) => Some(Selection::Kit(*i as usize)),
        Row::Gap(i) => Some(Selection::Gap(*i as usize)),
    }
}

/// Where a selection sits in the rows, if it's showing.
pub fn row_of(
    rows: &[Row],
    selection: &Selection,
    bundle: Option<&CatalogBundle>,
    groups: &[AddonGroup],
) -> Option<usize> {
    rows.iter().position(|row| match (row, selection) {
        (Row::Addon(i), Selection::Addon(id)) => bundle.is_some_and(|b| b.position(id) == Some(*i)),
        (Row::Managed(a), Selection::Addon(b)) => a == b,
        (Row::Other(i), Selection::Other(primary)) => groups
            .get(*i as usize)
            .is_some_and(|g| g.primary == *primary),
        (Row::Kit(a), Selection::Kit(b)) | (Row::Gap(a), Selection::Gap(b)) => *a as usize == *b,
        _ => false,
    })
}

/// The next row to land on with the arrow keys, skipping headers. `None` when there's
/// nowhere to go.
pub fn step_selection(rows: &[Row], from: Option<usize>, down: bool) -> Option<usize> {
    let selectable = |i: usize| !matches!(rows.get(i), Some(Row::Header { .. }) | None);
    match (from, down) {
        (None, true) => (0..rows.len()).find(|&i| selectable(i)),
        (None, false) => (0..rows.len()).rev().find(|&i| selectable(i)),
        (Some(at), true) => (at + 1..rows.len()).find(|&i| selectable(i)),
        (Some(at), false) => (0..at).rev().find(|&i| selectable(i)),
    }
}

/// Which row a tab selects when nothing else says otherwise: the first addon, because a
/// kit is a poor first impression, or failing that whatever comes first.
pub fn first_row(rows: &[Row]) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Addon(_) | Row::Managed(_)))
        .or_else(|| step_selection(rows, None, true))
}

/// What a list row says at its right edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag<'a> {
    Version(&'a str),
    Update,
    OutOfDate,
    Installed,
    Nothing,
}

impl Tag<'_> {
    /// The same words a screen reader gets, so color is never the only signal.
    pub fn words(&self) -> &str {
        match self {
            Tag::Version(version) => version,
            Tag::Update => "update",
            Tag::OutOfDate => "out of date",
            Tag::Installed => "installed",
            Tag::Nothing => "",
        }
    }
}

/// A row in Browse: an installed addon says so, grey ones have fallen behind Forever. An
/// entry that has become another addon isn't what the record installed, so it's shown like
/// any addon that isn't installed.
pub fn browse_tag<'a>(addon: &'a Addon, managed: Option<&ManagedAddon>) -> Tag<'a> {
    if let Some(managed) = managed.filter(|managed| same_addon(managed, addon)) {
        return if !managed.pinned && update_for(managed, addon).is_some() {
            Tag::Update
        } else {
            Tag::Installed
        };
    }
    if addon.tier == 0 {
        return Tag::OutOfDate;
    }
    addon
        .stable
        .as_ref()
        .or(addon.beta.as_ref())
        .map_or(Tag::Nothing, |release| Tag::Version(&release.version))
}

/// A row in My Addons. An entry that has become another addon says nothing about this one.
pub fn managed_tag<'a>(
    managed: &'a ManagedAddon,
    addon: Option<&Addon>,
    targets_forever: bool,
) -> Tag<'a> {
    if let Some(addon) = addon.filter(|addon| same_addon(managed, addon)) {
        if !managed.pinned && update_for(managed, addon).is_some() {
            return Tag::Update;
        }
        if addon.tier == 0 {
            return Tag::OutOfDate;
        }
    }
    if !targets_forever {
        return Tag::OutOfDate;
    }
    if managed.version.is_empty() {
        Tag::Nothing
    } else {
        Tag::Version(&managed.version)
    }
}

/// A row for an unmanaged group.
pub fn other_tag(group: &AddonGroup) -> Tag<'_> {
    if !group.targets_forever {
        return Tag::OutOfDate;
    }
    group.version.as_deref().map_or(Tag::Nothing, Tag::Version)
}

/// What the big button on an addon's page does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MainButton {
    Install,
    Update,
    Reinstall,
    /// Already on disk from another tool, and it matches this catalog entry.
    Manage,
    /// A pin stops updates, so there's nothing to press until it's lifted.
    Pinned,
}

impl MainButton {
    pub fn label(self) -> &'static str {
        match self {
            MainButton::Install => "Install",
            MainButton::Update | MainButton::Pinned => "Update",
            MainButton::Reinstall => "Reinstall",
            MainButton::Manage => "Manage with this app",
        }
    }
}

pub fn main_button(addon: &Addon, managed: Option<&ManagedAddon>, adoptable: bool) -> MainButton {
    match managed {
        None if adoptable => MainButton::Manage,
        None => MainButton::Install,
        Some(m) if update_for(m, addon).is_some() => {
            if m.pinned {
                MainButton::Pinned
            } else {
                MainButton::Update
            }
        }
        Some(_) => MainButton::Reinstall,
    }
}

/// The channel an addon installs from: its record's, the page's checkbox, or the default.
pub fn channel_for(
    managed: Option<&ManagedAddon>,
    choice: Option<Channel>,
    default: Channel,
) -> Channel {
    managed.map(|m| m.channel).or(choice).unwrap_or(default)
}

/// A kit's addons, as `(addon, its row tag)`. Ids the catalog doesn't have are left out,
/// because a kit is a suggestion, not a promise.
pub fn kit_addons<'a>(
    kit: &'a Kit,
    bundle: &'a CatalogBundle,
    managed: Option<&BTreeMap<String, ManagedAddon>>,
) -> Vec<(&'a Addon, Tag<'a>)> {
    kit.addons
        .iter()
        .filter_map(|id| {
            let addon = bundle.addon(id)?;
            let record = managed.and_then(|m| m.get(id));
            Some((addon, browse_tag(addon, record)))
        })
        .collect()
}

/// Ids in a kit that aren't installed yet. One whose entry has become another addon counts, as
/// its row shows it, so installing the kit says why that one is left alone.
pub fn kit_missing(
    kit: &Kit,
    bundle: &CatalogBundle,
    managed: Option<&BTreeMap<String, ManagedAddon>>,
) -> Vec<String> {
    kit.addons
        .iter()
        .filter(|id| {
            bundle.addon(id).is_some_and(|addon| {
                managed
                    .and_then(|m| m.get(*id))
                    .is_none_or(|record| !same_addon(record, addon))
            })
        })
        .cloned()
        .collect()
}

/// A gap's alternatives that really are in the catalog.
pub fn gap_alternatives<'a>(gap: &'a Gap, bundle: &'a CatalogBundle) -> Vec<&'a Addon> {
    gap.alternatives
        .iter()
        .filter_map(|id| bundle.addon(id))
        .collect()
}

/// What a group of packs is called when its catalog entry left the name out.
const UNNAMED_PACKS: &str = "Packs";

/// One pack an addon's page offers, such as a voice pack.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PackChoice {
    pub id: String,
    pub name: String,
    /// The download, in bytes, of the build it would install. 0 when unknown.
    pub size: u64,
    /// Some folder of it is in AddOns or in a record, whoever put it there.
    pub installed: bool,
}

/// One group of packs as an addon's page shows it: any one of `choices` will do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PackLine {
    pub name: String,
    pub needed: bool,
    /// In the order to offer them. The first is the one offered by default.
    pub choices: Vec<PackChoice>,
    /// Folder name prefixes that meet the group too, lowercased: packs the catalog doesn't
    /// list, such as another language's or another tool's.
    pub met_by: Vec<String>,
    /// Some folder in AddOns or in a record starts with one of `met_by`.
    pub met_elsewhere: bool,
}

impl PackLine {
    pub fn met(&self) -> bool {
        self.met_elsewhere || self.choices.iter().any(|choice| choice.installed)
    }

    /// Would this folder, lowercased, meet the group although it's none of its packs'?
    pub fn met_by_folder(&self, lower_folder: &str) -> bool {
        self.met_by
            .iter()
            .any(|prefix| lower_folder.starts_with(prefix.as_str()))
    }
}

/// An addon's pack groups, as its page shows them. Only a reviewed entry's count, because a
/// person wrote them down, and only trusted packs the catalog has, since a page that offers
/// one invites installing it. A pack that isn't here and has no build to install is left out,
/// and so is a group left with nothing to show. `installed` is every folder in AddOns or in a
/// record, lowercased, and `channel` says which build each pack would install. A folder there
/// that starts with one of a group's `met_by` prefixes meets it too, in any case.
pub fn pack_lines(
    addon: &Addon,
    bundle: &CatalogBundle,
    installed: &HashSet<String>,
    channel: impl Fn(&str) -> Channel,
) -> Vec<PackLine> {
    if !addon.reviewed {
        return Vec::new();
    }
    addon
        .packs
        .iter()
        .filter_map(|group| {
            let mut seen = HashSet::new();
            let choices: Vec<PackChoice> = group
                .ids
                .iter()
                .filter(|id| **id != addon.id && seen.insert(id.as_str()))
                .filter_map(|id| bundle.addon(id))
                .filter(|pack| pack.trusted)
                .filter_map(|pack| {
                    let installed = pack
                        .folders
                        .iter()
                        .any(|folder| installed.contains(&folder.to_lowercase()));
                    let release = pack.release(channel(&pack.id));
                    (installed || release.is_some()).then(|| PackChoice {
                        id: pack.id.clone(),
                        name: pack.name.clone(),
                        size: release.map_or(0, |release| release.size),
                        installed,
                    })
                })
                .collect();
            if choices.is_empty() {
                return None;
            }
            // A blank prefix would meet every group with anything at all installed.
            let met_by: Vec<String> = group
                .met_by
                .iter()
                .map(|prefix| prefix.trim().to_lowercase())
                .filter(|prefix| !prefix.is_empty())
                .collect();
            let mut line = PackLine {
                name: if group.name.is_empty() {
                    UNNAMED_PACKS.to_owned()
                } else {
                    group.name.clone()
                },
                needed: group.needed,
                choices,
                met_by,
                met_elsewhere: false,
            };
            line.met_elsewhere =
                !line.met_by.is_empty() && installed.iter().any(|f| line.met_by_folder(f));
            Some(line)
        })
        .collect()
}

/// An `http(s)` link and nothing else. The address came from a stranger, and it's about
/// to be handed to the OS to open.
pub fn safe_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_ok = !rest.is_empty() && !rest.starts_with(['/', '.', '@']);
    (host_ok && !url.chars().any(|c| c.is_whitespace() || c.is_control())).then_some(url)
}

/// Where an addon lives on the web, for "Open page" and "Share".
pub fn addon_page_url(addon: &Addon) -> Option<String> {
    if let Some(url) = addon.website.as_deref().and_then(safe_url) {
        return Some(url.to_owned());
    }
    source_url(addon)
}

/// Where the Source line goes: the very thing it names. An addon's `website` is whatever its
/// author wrote, so it can't stand in for the repo.
pub fn source_url(addon: &Addon) -> Option<String> {
    match &addon.source {
        Source::Github { repo } => {
            let plain = repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
            (plain && repo.contains('/')).then(|| format!("https://github.com/{repo}"))
        }
        Source::Wowi { id } => Some(format!("https://www.wowinterface.com/downloads/info{id}")),
        Source::Direct { url } => safe_url(url).map(str::to_owned),
    }
}

/// Who an addon really comes from, for the Details table. Display names aren't unique, so
/// the repo is the only real signal.
pub fn source_text(addon: &Addon) -> String {
    match &addon.source {
        Source::Github { repo } => format!("GitHub: {}", clean_line(repo, 140)),
        Source::Wowi { .. } => "WoWInterface".to_owned(),
        Source::Direct { .. } => "Direct link".to_owned(),
    }
}

/// What the page says about an entry the catalog doesn't vouch for.
pub const UNTRUSTED_NOTE: &str =
    "New in the catalog. Nobody has reviewed it, so check who made it before you install.";

/// Where the game is, as far as anyone knows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GameState {
    Searching,
    Found { version: Option<String> },
    NotFound,
}

/// Where the catalog came from, and whether fetching a newer one worked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatalogStatus {
    Loading,
    /// As fresh as the server has it.
    Fresh,
    /// A cached copy, because the fetch failed for some other reason than the network.
    Cached,
    /// A cached copy, because we're offline.
    Offline,
    /// Nothing to show. `offline` says whether that's the network's fault.
    Missing {
        offline: bool,
    },
}

/// The text at the bottom right.
pub fn status_line(
    game: &GameState,
    addons: usize,
    generated: i64,
    catalog: CatalogStatus,
    now: i64,
) -> String {
    let game = match game {
        GameState::Searching => "Looking for WoW Forever".to_owned(),
        GameState::NotFound => "WoW Forever not found".to_owned(),
        GameState::Found { version: Some(v) } => format!("Forever {}", short_version(v)),
        GameState::Found { version: None } => "Forever".to_owned(),
    };
    let catalog = match catalog {
        CatalogStatus::Loading => "Loading the catalog".to_owned(),
        CatalogStatus::Offline => "Offline, showing the last catalog".to_owned(),
        CatalogStatus::Missing { offline: true } => "Offline, no catalog yet".to_owned(),
        CatalogStatus::Missing { offline: false } => "No catalog yet".to_owned(),
        CatalogStatus::Fresh | CatalogStatus::Cached => {
            let count = plural(addons, "addon");
            if generated > 0 {
                format!("{count}  |  updated {}", age_text(now - generated))
            } else {
                count
            }
        }
    };
    format!("{game}  |  {catalog}")
}

/// `1.60.1` out of `1.60.1.12345`. The build number is for the diagnostics, not the status.
pub fn short_version(version: &str) -> &str {
    let end = version
        .match_indices('.')
        .nth(2)
        .map_or(version.len(), |(at, _)| at);
    &version[..end]
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `3 hours ago`, to the coarsest unit that fits.
pub fn age_text(seconds: i64) -> String {
    let seconds = seconds.max(0) as usize;
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} ago", plural(seconds / 60, "minute")),
        3600..86400 => format!("{} ago", plural(seconds / 3600, "hour")),
        _ => format!("{} ago", plural(seconds / 86400, "day")),
    }
}

/// `Nov 4, 2026`, in UTC. Fine for a release date, and free of a date dependency.
pub fn date_text(unix: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Howard Hinnant's civil_from_days, which every date library is built on.
    let z = unix.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{} {day}, {year}", MONTHS[(month - 1) as usize])
}

/// `412 KB`, `93.6 MB`, or from 1,000 MB up `1.5 GB`. An unknown size says nothing at all.
pub fn size_text(bytes: u64) -> String {
    // Rounded in whole numbers, so 999.96 MB can't come out as "1000.0 MB".
    let tenths = |unit: u64| bytes / unit + u64::from(bytes % unit >= unit / 2);
    if bytes < 1_000_000 {
        return format!("{} KB", (bytes / 1000).max(1));
    }
    let megabytes = tenths(100_000);
    if megabytes < 10_000 {
        return format!("{}.{} MB", megabytes / 10, megabytes % 10);
    }
    let gigabytes = tenths(100_000_000);
    format!("{}.{} GB", gigabytes / 10, gigabytes % 10)
}

/// What "Look again" actually looks at, so the no-game page can say.
pub fn places_checked() -> &'static [&'static str] {
    if cfg!(windows) {
        &[
            "Battle.net's list of installed games",
            "The game's entry in the Windows registry",
            "The usual folders on each local drive",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "Battle.net's list of installed games",
            "The Applications folder",
            "A Spotlight search for the game",
        ]
    } else {
        &[
            "Wine prefixes in your home folder",
            "Lutris, Bottles, Steam and Faugus game folders",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feeling_core::catalog::PackGroup;

    fn addon(id: &str, name: &str, category: &str, downloads: u64) -> Addon {
        Addon {
            id: id.to_owned(),
            name: name.to_owned(),
            category: category.to_owned(),
            downloads,
            tier: 3,
            folders: vec![name.to_owned()],
            stable: Some(Release {
                version: "2.0".to_owned(),
                url: "https://example.test/a.zip".to_owned(),
                date: 100,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn bundle() -> CatalogBundle {
        CatalogBundle::new(Catalog {
            schema: 1,
            generated: 1000,
            addons: vec![
                addon("gh:a/questie", "Questie", "quests", 900),
                addon("gh:a/rxp", "RestedXP", "quests", 100),
                addon("gh:a/dbm", "Deadly Boss Mods", "bosses", 800),
                addon("gh:a/odd", "Odd One", "brand-new-category", 1),
            ],
            featured: vec!["gh:a/dbm".to_owned(), "gh:a/nope".to_owned()],
            kits: vec![Kit {
                name: "Starter".to_owned(),
                description: "Things".to_owned(),
                addons: vec!["gh:a/questie".to_owned(), "gh:a/gone".to_owned()],
            }],
            gaps: vec![Gap {
                name: "WeakAuras".to_owned(),
                reason: "Not here".to_owned(),
                website: None,
                alternatives: vec!["gh:a/dbm".to_owned(), "gh:a/gone".to_owned()],
            }],
        })
    }

    fn managed(id: &str, name: &str, version: &str) -> ManagedAddon {
        ManagedAddon {
            id: id.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            folders: vec![name.to_owned()],
            ..Default::default()
        }
    }

    fn map(items: Vec<ManagedAddon>) -> BTreeMap<String, ManagedAddon> {
        items.into_iter().map(|m| (m.id.clone(), m)).collect()
    }

    fn headers(rows: &[Row]) -> Vec<(&'static str, u32, bool)> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Header {
                    name, count, open, ..
                } => Some((*name, *count, *open)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn browse_shows_kits_then_recommended_then_closed_categories() {
        let rows = browse_rows(&bundle(), "", SortBy::Popular, &HashSet::new());
        assert_eq!(
            headers(&rows),
            vec![
                ("Starter kits", 1, true),
                ("Recommended", 1, true),
                ("Quests and Leveling", 2, false),
                ("Bosses and Dungeons", 1, false),
                ("Everything Else", 1, false),
            ]
        );
        assert!(rows.contains(&Row::Kit(0)));
        assert_eq!(
            rows.iter().filter(|r| matches!(r, Row::Addon(_))).count(),
            1
        );
    }

    #[test]
    fn opening_a_category_lists_it_in_sort_order() {
        let b = bundle();
        let toggled = HashSet::from([HeaderId::Category(0)]);
        let rows = browse_rows(&b, "", SortBy::Name, &toggled);
        let at = rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    Row::Header {
                        id: HeaderId::Category(0),
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(rows[at + 1], Row::Addon(0));
        assert_eq!(rows[at + 2], Row::Addon(1));
        let sorted = browse_rows(&b, "", SortBy::Popular, &toggled);
        assert_eq!(sorted[at + 1], Row::Addon(0), "more downloads comes first");
    }

    #[test]
    fn search_is_flat_and_ends_with_gaps() {
        let rows = browse_rows(&bundle(), "questie", SortBy::Popular, &HashSet::new());
        assert_eq!(rows, vec![Row::Addon(0)]);
        let rows = browse_rows(&bundle(), "weak", SortBy::Popular, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                Row::Header {
                    id: HeaderId::NotAvailable,
                    name: "Not available here",
                    count: 1,
                    open: true
                },
                Row::Gap(0)
            ]
        );
    }

    #[test]
    fn a_command_being_typed_is_not_a_search() {
        let b = bundle();
        let plain = browse_rows(&b, "", SortBy::Popular, &HashSet::new());
        assert_eq!(
            browse_rows(&b, "/dan", SortBy::Popular, &HashSet::new()),
            plain
        );
    }

    #[test]
    fn my_addons_splits_updates_from_installed() {
        let list = map(vec![
            managed("gh:a/questie", "Questie", "1.0"),
            managed("gh:a/dbm", "Deadly Boss Mods", "2.0"),
        ]);
        let b = bundle();
        let updates = updates_available(&list, Some(&b));
        assert_eq!(updates, vec!["gh:a/questie".to_owned()]);
        let groups = vec![AddonGroup {
            primary: "Foo".to_owned(),
            folders: vec!["Foo".to_owned()],
            title: "Foo".to_owned(),
            ..Default::default()
        }];
        let rows = my_rows(&list, &updates, &groups, "", &HashSet::new());
        assert_eq!(
            headers(&rows),
            vec![
                ("Updates available", 1, true),
                ("Installed", 1, true),
                ("Other folders", 1, true)
            ]
        );
        let rows = my_rows(&list, &updates, &groups, "boss", &HashSet::new());
        assert_eq!(headers(&rows), vec![("Installed", 1, true)]);
    }

    #[test]
    fn pinned_addons_never_count_as_updates() {
        let mut m = managed("gh:a/questie", "Questie", "1.0");
        m.pinned = true;
        let list = map(vec![m]);
        assert!(updates_available(&list, Some(&bundle())).is_empty());
    }

    #[test]
    fn local_zips_never_get_updates() {
        let b = bundle();
        let mut m = managed("gh:a/questie", "Questie", "1.0");
        m.origin = Origin::LocalZip;
        assert!(update_for(&m, b.addon("gh:a/questie").unwrap()).is_none());
    }

    #[test]
    fn a_leading_v_is_not_an_update() {
        assert!(same_version("v12.6.2", "12.6.2"));
        assert!(same_version("12.6.2", "V12.6.2"));
        assert!(!same_version("12.6.2", "12.6.3"));
        assert!(!same_version("version", "ersion"), "only before a digit");
    }

    #[test]
    fn selection_skips_headers() {
        let rows = vec![
            Row::Header {
                id: HeaderId::Kits,
                name: "Kits",
                count: 1,
                open: true,
            },
            Row::Kit(0),
            Row::Header {
                id: HeaderId::Recommended,
                name: "Rec",
                count: 1,
                open: true,
            },
            Row::Addon(3),
        ];
        assert_eq!(step_selection(&rows, None, true), Some(1));
        assert_eq!(step_selection(&rows, Some(1), true), Some(3));
        assert_eq!(step_selection(&rows, Some(3), true), None);
        assert_eq!(step_selection(&rows, Some(3), false), Some(1));
        assert_eq!(step_selection(&rows, Some(1), false), None);
        assert_eq!(first_row(&rows), Some(3), "an addon beats a kit");
    }

    #[test]
    fn the_arrows_walk_the_settings_sections_and_stop_at_the_ends() {
        assert_eq!(next_section(Section::Game, true), Section::Backups);
        assert_eq!(next_section(Section::Backups, false), Section::Game);
        assert_eq!(next_section(Section::Game, false), Section::Game);
        assert_eq!(next_section(Section::About, true), Section::About);
    }

    #[test]
    fn tags_say_what_they_mean() {
        let b = bundle();
        let a = b.addon("gh:a/questie").unwrap();
        assert_eq!(browse_tag(a, None), Tag::Version("2.0"));
        let old = managed(&a.id, &a.name, "1.0");
        assert_eq!(browse_tag(a, Some(&old)), Tag::Update);
        assert_eq!(managed_tag(&old, Some(a), true), Tag::Update);
        let same = managed(&a.id, &a.name, "2.0");
        assert_eq!(browse_tag(a, Some(&same)), Tag::Installed);
        assert_eq!(managed_tag(&same, Some(a), true), Tag::Version("2.0"));
        assert_eq!(managed_tag(&same, Some(a), false), Tag::OutOfDate);

        let mut grey = a.clone();
        grey.tier = 0;
        assert_eq!(browse_tag(&grey, None), Tag::OutOfDate);
        assert_eq!(managed_tag(&same, Some(&grey), true), Tag::OutOfDate);
    }

    #[test]
    fn main_button_follows_the_record() {
        let b = bundle();
        let a = b.addon("gh:a/questie").unwrap();
        assert_eq!(main_button(a, None, false), MainButton::Install);
        assert_eq!(main_button(a, None, true), MainButton::Manage);
        let mut old = managed(&a.id, &a.name, "1.0");
        assert_eq!(main_button(a, Some(&old), false), MainButton::Update);
        old.pinned = true;
        assert_eq!(main_button(a, Some(&old), false), MainButton::Pinned);
        let same = managed(&a.id, &a.name, "2.0");
        assert_eq!(main_button(a, Some(&same), false), MainButton::Reinstall);
    }

    #[test]
    fn status_line_is_plain_about_what_it_knows() {
        let game = GameState::Found {
            version: Some("1.60.1".to_owned()),
        };
        assert_eq!(
            status_line(&game, 18, 1000, CatalogStatus::Fresh, 1000 + 7200),
            "Forever 1.60.1  |  18 addons  |  updated 2 hours ago"
        );
        assert_eq!(
            status_line(&game, 18, 1000, CatalogStatus::Offline, 2000),
            "Forever 1.60.1  |  Offline, showing the last catalog"
        );
        assert_eq!(
            status_line(
                &GameState::NotFound,
                0,
                0,
                CatalogStatus::Missing { offline: true },
                0
            ),
            "WoW Forever not found  |  Offline, no catalog yet"
        );
        assert_eq!(
            status_line(&game, 1, 0, CatalogStatus::Cached, 0),
            "Forever 1.60.1  |  1 addon"
        );
    }

    #[test]
    fn the_status_shows_the_game_version_without_its_build() {
        assert_eq!(short_version("1.60.1.12345"), "1.60.1");
        assert_eq!(short_version("1.60.1"), "1.60.1");
        assert_eq!(short_version("2"), "2");
    }

    #[test]
    fn dates_and_ages_read_naturally() {
        assert_eq!(date_text(1_794_000_000), "Nov 6, 2026");
        assert_eq!(date_text(0), "Jan 1, 1970");
        assert_eq!(age_text(5), "just now");
        assert_eq!(age_text(60), "1 minute ago");
        assert_eq!(age_text(7200), "2 hours ago");
        assert_eq!(age_text(90000), "1 day ago");
        assert_eq!(age_text(-50), "just now", "a clock that jumped back");
    }

    #[test]
    fn catalog_text_is_cleaned_of_anything_that_could_wreck_a_row() {
        assert_eq!(clean_line("  Quest\nie\t2 ", 80), "Quest ie 2");
        assert_eq!(clean_line("evil\u{202e}name", 80), "evilname");
        assert_eq!(clean_line("abcdef", 3), "abc");
        assert_eq!(clean_line("\u{200b}\u{feff}", 80), "");

        let mut nameless = addon("gh:a/x", "", "misc", 0);
        nameless.folders = vec!["Folder".to_owned()];
        let b = CatalogBundle::new(Catalog {
            addons: vec![nameless, addon("", "No id", "misc", 0)],
            ..Default::default()
        });
        assert_eq!(
            b.catalog.addons.len(),
            1,
            "an addon without an id can't be tracked"
        );
        assert_eq!(b.catalog.addons[0].name, "Folder");
    }

    #[test]
    fn only_web_links_get_opened() {
        assert_eq!(safe_url(" https://a.test/x "), Some("https://a.test/x"));
        assert!(safe_url("http://a.test").is_some());
        assert!(safe_url("file:///c:/windows").is_none());
        assert!(safe_url("javascript:alert(1)").is_none());
        assert!(safe_url("https://").is_none());
        assert!(safe_url("https://a b.test").is_none());
    }

    #[test]
    fn addon_pages_prefer_the_website_and_refuse_odd_repos() {
        let mut a = addon("gh:a/b", "A", "misc", 0);
        a.source = Source::Github {
            repo: "a/b".to_owned(),
        };
        assert_eq!(addon_page_url(&a).unwrap(), "https://github.com/a/b");
        a.source = Source::Github {
            repo: "a/b?x=1".to_owned(),
        };
        assert_eq!(addon_page_url(&a), None);
        a.source = Source::Wowi { id: 12 };
        assert!(addon_page_url(&a).unwrap().ends_with("info12"));
        a.website = Some("https://home.test".to_owned());
        assert_eq!(addon_page_url(&a).unwrap(), "https://home.test");
        a.website = Some("file:///etc/passwd".to_owned());
        assert!(addon_page_url(&a).unwrap().ends_with("info12"));
    }

    #[test]
    fn the_source_line_names_the_real_owner_and_goes_there() {
        let mut a = addon("gh:a/b", "A", "misc", 0);
        a.website = Some("https://looks-official.test".to_owned());
        a.source = Source::Github {
            repo: "stranger/details".to_owned(),
        };
        assert_eq!(source_text(&a), "GitHub: stranger/details");
        assert_eq!(
            source_url(&a).unwrap(),
            "https://github.com/stranger/details",
            "what its author wrote as a website doesn't change where it comes from"
        );
        a.source = Source::Wowi { id: 12 };
        assert_eq!(source_text(&a), "WoWInterface");
        assert!(source_url(&a).unwrap().ends_with("info12"));
        a.source = Source::Direct {
            url: "https://a.test/x.zip".to_owned(),
        };
        assert_eq!(source_text(&a), "Direct link");
    }

    #[test]
    fn kits_and_gaps_ignore_ids_the_catalog_lacks() {
        let b = bundle();
        let kit = &b.catalog.kits[0];
        assert_eq!(kit_addons(kit, &b, None).len(), 1);
        assert_eq!(kit_missing(kit, &b, None), vec!["gh:a/questie".to_owned()]);
        let list = map(vec![managed("gh:a/questie", "Questie", "2.0")]);
        assert!(kit_missing(kit, &b, Some(&list)).is_empty());
        assert_eq!(gap_alternatives(&b.catalog.gaps[0], &b).len(), 1);
    }

    #[test]
    fn a_kit_counts_an_entry_that_became_another_addon_as_not_installed() {
        let b = bundle();
        let kit = &b.catalog.kits[0];
        // The record's main folder isn't Questie's, so the entry is another addon now.
        let list = map(vec![managed("gh:a/questie", "QuestHelper", "1.0")]);
        assert_eq!(kit_addons(kit, &b, Some(&list))[0].1, Tag::Version("2.0"));
        assert_eq!(
            kit_missing(kit, &b, Some(&list)),
            vec!["gh:a/questie".to_owned()],
            "as its row shows it"
        );
    }

    #[test]
    fn row_lookup_finds_every_kind() {
        let b = bundle();
        let groups = vec![AddonGroup {
            primary: "Foo".to_owned(),
            ..Default::default()
        }];
        let rows = vec![
            Row::Addon(2),
            Row::Managed("x".to_owned()),
            Row::Other(0),
            Row::Kit(0),
            Row::Gap(0),
        ];
        for (i, row) in rows.iter().enumerate() {
            let selection = row_selection(row, Some(&b), &groups).unwrap();
            assert_eq!(row_of(&rows, &selection, Some(&b), &groups), Some(i));
        }
    }

    #[test]
    fn sizes_read_in_kb_mb_and_from_a_thousand_mb_in_gb() {
        assert_eq!(size_text(1), "1 KB");
        assert_eq!(size_text(820_400), "820 KB");
        assert_eq!(size_text(999_999), "999 KB");
        assert_eq!(size_text(1_000_000), "1.0 MB");
        assert_eq!(size_text(93_600_000), "93.6 MB");
        assert_eq!(size_text(456_216_109), "456.2 MB");
        assert_eq!(size_text(999_949_999), "999.9 MB");
        assert_eq!(size_text(999_950_000), "1.0 GB", "never 1000.0 MB");
        assert_eq!(size_text(1_479_966_451), "1.5 GB");
        assert_eq!(size_text(1_646_390_683), "1.6 GB");
        assert_eq!(size_text(12_000_000_000), "12.0 GB");
        assert!(
            size_text(u64::MAX).ends_with(" GB"),
            "a silly size can't overflow"
        );
    }

    const SPOKEN: &str = "gh:rusty-key/spoken-wow";
    const QUESTS: &str = "gh:rusty-key/spoken-wow/quests-audio";
    const GERMAN: &str = "gh:rusty-key/spoken-wow/quests-audio-dede";
    const PLACES: &str = "gh:rusty-key/spoken-wow/places-audio";

    fn pack(id: &str, name: &str, folders: &[&str], size: u64) -> Addon {
        let mut pack = addon(id, name, "quests", 10);
        pack.folders = folders.iter().map(|f| (*f).to_owned()).collect();
        pack.stable.as_mut().unwrap().size = size;
        pack.trusted = true;
        pack.reviewed = true;
        pack
    }

    fn spoken_catalog(packs: Vec<PackGroup>, more: Vec<Addon>) -> CatalogBundle {
        let mut spoken = addon(SPOKEN, "Spoken", "quests", 500);
        spoken.folders = vec!["Spoken".to_owned(), "Spoken_Quests".to_owned()];
        spoken.trusted = true;
        spoken.reviewed = true;
        spoken.packs = packs;
        let mut addons = vec![
            spoken,
            pack(
                QUESTS,
                "Spoken Audio: Quests",
                &["SpokenQuestsAudioAlliance", "SpokenQuestsAudioHorde"],
                1_479_966_451,
            ),
            pack(
                GERMAN,
                "Spoken Audio: Quests (German)",
                &["SpokenQuestsAudio_deDE"],
                1_646_390_683,
            ),
            pack(
                PLACES,
                "Spoken Audio: Places",
                &["SpokenPlacesAudio"],
                456_216_109,
            ),
        ];
        addons.extend(more);
        CatalogBundle::new(Catalog {
            addons,
            ..Default::default()
        })
    }

    fn group(name: &str, needed: bool, ids: &[&str]) -> PackGroup {
        PackGroup {
            name: name.to_owned(),
            needed,
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            met_by: Vec::new(),
        }
    }

    fn lines_for(bundle: &CatalogBundle, installed: &[&str]) -> Vec<PackLine> {
        let installed: HashSet<String> = installed.iter().map(|f| f.to_lowercase()).collect();
        pack_lines(bundle.addon(SPOKEN).unwrap(), bundle, &installed, |_| {
            Channel::Stable
        })
    }

    #[test]
    fn pack_lines_name_each_pack_with_its_size_in_the_order_given() {
        let b = spoken_catalog(
            vec![
                group("Quest voices", true, &[QUESTS, GERMAN]),
                group("Place voices", false, &[PLACES]),
            ],
            Vec::new(),
        );
        let lines = lines_for(&b, &[]);
        assert_eq!(
            lines,
            vec![
                PackLine {
                    name: "Quest voices".to_owned(),
                    needed: true,
                    choices: vec![
                        PackChoice {
                            id: QUESTS.to_owned(),
                            name: "Spoken Audio: Quests".to_owned(),
                            size: 1_479_966_451,
                            installed: false,
                        },
                        PackChoice {
                            id: GERMAN.to_owned(),
                            name: "Spoken Audio: Quests (German)".to_owned(),
                            size: 1_646_390_683,
                            installed: false,
                        },
                    ],
                    met_by: Vec::new(),
                    met_elsewhere: false,
                },
                PackLine {
                    name: "Place voices".to_owned(),
                    needed: false,
                    choices: vec![PackChoice {
                        id: PLACES.to_owned(),
                        name: "Spoken Audio: Places".to_owned(),
                        size: 456_216_109,
                        installed: false,
                    }],
                    met_by: Vec::new(),
                    met_elsewhere: false,
                },
            ]
        );
        assert!(!lines[0].met() && !lines[1].met());
    }

    #[test]
    fn a_pack_group_is_met_by_any_folder_starting_with_one_of_its_prefixes_in_any_case() {
        let mut quests = group("Quest voices", true, &[QUESTS]);
        quests.met_by = vec![
            "SpokenQuestsAudio".to_owned(),
            "SpokenCommunity_".to_owned(),
            String::new(),
            " ".to_owned(),
        ];
        let b = spoken_catalog(vec![quests], Vec::new());
        // German voices from another tool, and a community pack, neither of them listed.
        let german = lines_for(&b, &["Spoken", "SPOKENQUESTSAUDIO_deDE"]);
        assert!(german[0].met() && !german[0].choices[0].installed);
        assert!(german[0].met_elsewhere);
        assert!(lines_for(&b, &["spokencommunity_ivrit"])[0].met());
        assert!(
            !lines_for(&b, &["Spoken", "Spoken_Quests", "SpokenPlacesAudio"])[0].met(),
            "a blank prefix meets nothing"
        );
        assert!(!lines_for(&b, &["SpokenQuests"])[0].met(), "a whole prefix");
        assert!(german[0].met_by_folder("spokenquestsaudio_frfr"));
        assert!(!german[0].met_by_folder("spokenplacesaudio"));
    }

    #[test]
    fn a_pack_group_is_met_by_any_folder_of_any_of_its_packs_whoever_put_it_there() {
        let b = spoken_catalog(
            vec![group("Quest voices", true, &[QUESTS, GERMAN])],
            Vec::new(),
        );
        let lines = lines_for(&b, &["spokenquestsaudiohorde"]);
        assert!(
            lines[0].met(),
            "one of the quests pack's folders, in any case"
        );
        assert!(lines[0].choices[0].installed && !lines[0].choices[1].installed);
        assert!(lines_for(&b, &["SpokenQuestsAudio_deDE"])[0].met());
        assert!(!lines_for(&b, &["Spoken", "Spoken_Quests", "SpokenPlacesAudio"])[0].met());
    }

    #[test]
    fn only_a_reviewed_entrys_groups_and_only_trusted_packs_the_catalog_has_show() {
        let groups = vec![group(
            "Quest voices",
            true,
            &[QUESTS, "gh:someone/not-listed", GERMAN, QUESTS, SPOKEN],
        )];
        let mut b = spoken_catalog(groups.clone(), Vec::new());
        let choices: Vec<String> = lines_for(&b, &[])[0]
            .choices
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(
            choices,
            vec![QUESTS.to_owned(), GERMAN.to_owned()],
            "unknown ids, repeats and the addon itself are left out"
        );

        let stranger = b.position(GERMAN).unwrap() as usize;
        b.catalog.addons[stranger].trusted = false;
        assert_eq!(lines_for(&b, &[])[0].choices.len(), 1);
        let quests = b.position(QUESTS).unwrap() as usize;
        b.catalog.addons[quests].trusted = false;
        assert!(lines_for(&b, &[]).is_empty(), "nothing left to show");

        let mut unreviewed = spoken_catalog(groups, Vec::new());
        let spoken = unreviewed.position(SPOKEN).unwrap() as usize;
        unreviewed.catalog.addons[spoken].reviewed = false;
        assert!(lines_for(&unreviewed, &[]).is_empty());
    }

    #[test]
    fn a_pack_with_no_build_is_only_shown_once_it_is_installed() {
        let mut b = spoken_catalog(
            vec![group("Quest voices", true, &[QUESTS, GERMAN])],
            Vec::new(),
        );
        let german = b.position(GERMAN).unwrap() as usize;
        b.catalog.addons[german].stable = None;
        assert_eq!(lines_for(&b, &[])[0].choices.len(), 1);
        let lines = lines_for(&b, &["SpokenQuestsAudio_deDE"]);
        assert_eq!(lines[0].choices.len(), 2);
        assert_eq!(lines[0].choices[1].size, 0);
        assert!(lines[0].met());
    }

    #[test]
    fn a_pack_size_is_the_build_its_channel_would_install() {
        let mut b = spoken_catalog(vec![group("Quest voices", true, &[QUESTS])], Vec::new());
        let quests = b.position(QUESTS).unwrap() as usize;
        b.catalog.addons[quests].beta = Some(Release {
            version: "2.6.0-beta".to_owned(),
            size: 1_600_000_000,
            date: 200,
            ..Default::default()
        });
        let spoken = b.addon(SPOKEN).unwrap();
        let beta = |id: &str| {
            if id == QUESTS {
                Channel::Beta
            } else {
                Channel::Stable
            }
        };
        let lines = pack_lines(spoken, &b, &HashSet::new(), beta);
        assert_eq!(lines[0].choices[0].size, 1_600_000_000);
    }

    #[test]
    fn a_group_name_is_cleaned_and_never_left_blank() {
        let b = spoken_catalog(
            vec![
                group("Quest\nvoices\u{202e}", true, &[QUESTS]),
                group(" \u{200b} ", false, &[PLACES]),
            ],
            Vec::new(),
        );
        let names: Vec<String> = lines_for(&b, &[]).into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["Quest voices", "Packs"]);
    }

    #[test]
    fn an_entry_that_no_longer_lists_the_main_folder_is_another_addon() {
        let mut hijacked = addon(SPOKEN, "Spoken", "quests", 500);
        hijacked.folders = vec![
            "SpokenQuestsAudioAlliance".to_owned(),
            "SpokenQuestsAudioHorde".to_owned(),
        ];
        hijacked.stable.as_mut().unwrap().version = "v2.5.0".to_owned();
        let mut record = managed(SPOKEN, "Spoken", "v3.8.3");
        record.folders = vec!["Spoken".to_owned(), "Spoken_Quests".to_owned()];
        assert!(!same_addon(&record, &hijacked));
        assert_eq!(update_for(&record, &hijacked), None);
        assert_eq!(
            browse_tag(&hijacked, Some(&record)),
            Tag::Version("v2.5.0"),
            "that addon isn't installed"
        );
        assert_eq!(
            managed_tag(&record, Some(&hijacked), true),
            Tag::Version("v3.8.3")
        );
        hijacked.tier = 0;
        assert_eq!(
            managed_tag(&record, Some(&hijacked), true),
            Tag::Version("v3.8.3"),
            "that addon falling behind says nothing about this one"
        );

        let b = CatalogBundle::new(Catalog {
            addons: vec![hijacked.clone()],
            ..Default::default()
        });
        assert_eq!(entry_for(&b, SPOKEN, Some(&record)), None);
        assert!(
            entry_for(&b, SPOKEN, None).is_some(),
            "it's still there to browse"
        );
        let list = map(vec![record.clone()]);
        assert!(updates_available(&list, Some(&b)).is_empty());

        // Still listing the main folder, in any case and anywhere, it's the same addon.
        hijacked.folders.push("SPOKEN".to_owned());
        assert!(same_addon(&record, &hijacked));
        assert!(update_for(&record, &hijacked).is_some());
        // A record with no folders has nothing to tell it by.
        record.folders.clear();
        assert!(same_addon(&record, &hijacked));
    }

    #[test]
    fn a_main_folder_the_catalog_vouches_for_as_a_rename_is_still_the_same_addon() {
        let mut renamed = addon("wowi:424242", "ShortRoyalty", "misc", 10);
        renamed.stable.as_mut().unwrap().version = "v0.3.1".to_owned();
        let record = managed("wowi:424242", "ShortKings", "v0.3.0");
        assert!(
            !same_addon(&record, &renamed),
            "a rename nobody vouched for"
        );
        assert_eq!(browse_tag(&renamed, Some(&record)), Tag::Version("v0.3.1"));

        renamed.formerly = vec!["shortkings".to_owned()];
        assert!(same_addon(&record, &renamed), "in any case");
        assert!(update_for(&record, &renamed).is_some());
        assert_eq!(browse_tag(&renamed, Some(&record)), Tag::Update);
        assert_eq!(managed_tag(&record, Some(&renamed), true), Tag::Update);
        let b = CatalogBundle::new(Catalog {
            addons: vec![renamed],
            ..Default::default()
        });
        assert!(entry_for(&b, "wowi:424242", Some(&record)).is_some());
        let list = map(vec![record]);
        assert_eq!(updates_available(&list, Some(&b)), ["wowi:424242"]);

        // Only the record's main folder counts: another of its folders proves nothing.
        let mut other = managed("wowi:424242", "ShortKings", "v0.3.0");
        other.folders = vec!["SomethingElse".to_owned(), "ShortKings".to_owned()];
        assert!(!same_addon(&other, b.addon("wowi:424242").unwrap()));
    }

    #[test]
    fn a_page_without_an_entry_says_which_of_three_reasons_it_has() {
        let mut record = managed("gh:a/sixty", "Forever Sixty", "addon-v2.0.34");
        let data = addon("gh:a/sixty", "Forever Sixty Data", "misc", 1);
        assert_eq!(
            no_updates_note(&record, Some(&data)),
            "The catalog now lists a different addon here, Forever Sixty Data. Yours doesn't \
             get updates from it. To switch, remove yours, then install that one."
        );
        assert_eq!(
            no_updates_tooltip(&record, Some(&data)),
            "The catalog now lists a different addon here"
        );
        assert_eq!(
            no_updates_note(&record, None),
            "This addon isn't in the catalog anymore, so it doesn't get updates."
        );
        assert_eq!(
            no_updates_tooltip(&record, None),
            "Not in the catalog anymore"
        );
        record.origin = Origin::LocalZip;
        assert_eq!(
            no_updates_note(&record, None),
            "Installed from a zip file, so it doesn't get updates."
        );
        assert_eq!(
            no_updates_tooltip(&record, None),
            "Installed from a zip file"
        );
    }

    #[test]
    fn every_demo_addon_is_still_the_addon_its_entry_lists() {
        let bundle = CatalogBundle::new(crate::demo::catalog());
        let state = crate::demo::state(crate::args::Scene::Normal);
        let records = &state.current().unwrap().managed;
        assert!(!records.is_empty());
        for record in records.values() {
            if let Some(entry) = bundle.addon(&record.id) {
                assert!(same_addon(record, entry), "{}", record.id);
            }
        }
    }

    #[test]
    fn a_big_catalog_rebuilds_quickly() {
        let addons: Vec<Addon> = (0..8000)
            .map(|i| addon(&format!("gh:a/{i}"), &format!("Addon {i}"), "misc", i))
            .collect();
        let b = CatalogBundle::new(Catalog {
            addons,
            ..Default::default()
        });
        let toggled = HashSet::from([HeaderId::Category(12)]);
        let rows = browse_rows(&b, "", SortBy::Popular, &toggled);
        assert_eq!(rows.len(), 8001);
    }
}
