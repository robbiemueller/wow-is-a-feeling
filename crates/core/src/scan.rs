//! Reading `Interface/AddOns` into folders with parsed TOCs, grouping multi-folder
//! addons, and matching what's already there to the catalog.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, DirEntry, File, Metadata};
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::catalog::{Addon, Catalog, Source};
use crate::error::{Error, Result};
use crate::toc::{self, Toc};

const MAX_TOC_BYTES: u64 = 256 * 1024;

/// One folder in `Interface/AddOns`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalFolder {
    /// The folder name as spelled on disk.
    pub name: String,
    /// The TOC the Forever client would load. `None` when the client would ignore this folder.
    pub toc_file: Option<String>,
    /// Empty when there's no TOC, or it couldn't be read.
    pub toc: Toc,
    pub targets_forever: bool,
    /// Unix seconds. 0 when unknown.
    pub modified: i64,
}

/// Read `addons_dir`. A folder that doesn't exist yet just means no addons, and one
/// unreadable addon never fails the whole scan. Only looks one level inside each folder.
pub fn scan_addons(addons_dir: &Path) -> Result<Vec<LocalFolder>> {
    let entries = match fs::read_dir(addons_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(addons_dir, e)),
    };

    let mut folders = Vec::new();
    let mut toc_names = Vec::new();
    let mut buffer = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Some(metadata) = folder_metadata(&entry) else {
            continue;
        };
        folders.push(read_folder(
            name,
            &entry.path(),
            unix_modified(&metadata),
            &mut toc_names,
            &mut buffer,
        ));
    }
    folders.sort_by_cached_key(|f| (f.name.to_lowercase(), f.name.clone()));
    Ok(folders)
}

/// The metadata of a folder, following links, because developers often link addons
/// in from their working copies. `None` for anything that isn't a folder.
fn folder_metadata(entry: &DirEntry) -> Option<Metadata> {
    let kind = entry.file_type().ok()?;
    let metadata = if kind.is_symlink() {
        fs::metadata(entry.path()).ok()?
    } else {
        entry.metadata().ok()?
    };
    metadata.is_dir().then_some(metadata)
}

fn unix_modified(metadata: &Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs() as i64)
}

fn read_folder(
    name: String,
    path: &Path,
    modified: i64,
    toc_names: &mut Vec<String>,
    buffer: &mut Vec<u8>,
) -> LocalFolder {
    list_toc_files(path, toc_names);
    let toc_file = toc::pick_toc(&name, toc_names).map(str::to_string);
    let toc = toc_file
        .as_deref()
        .map(|file| read_toc(&path.join(file), buffer))
        .unwrap_or_default();
    LocalFolder {
        targets_forever: toc.targets_forever(),
        name,
        toc_file,
        toc,
        modified,
    }
}

fn list_toc_files(folder: &Path, names: &mut Vec<String>) {
    names.clear();
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let is_toc = Path::new(&file)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toc"));
        if is_toc
            && entry.file_type().is_ok_and(|kind| !kind.is_dir())
            && let Some(file) = file.to_str()
        {
            names.push(file.to_string());
        }
    }
}

fn read_toc(path: &Path, buffer: &mut Vec<u8>) -> Toc {
    buffer.clear();
    let read = File::open(path).and_then(|file| file.take(MAX_TOC_BYTES).read_to_end(buffer));
    match read {
        Ok(_) => toc::parse(&String::from_utf8_lossy(buffer)),
        Err(e) => {
            log::warn!("Couldn't read {}: {e}", path.display());
            Toc::default()
        }
    }
}

/// Unmanaged folders that belong together, shown as one addon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddonGroup {
    /// The folder that names the group.
    pub primary: String,
    /// Every folder in the group, primary first.
    pub folders: Vec<String>,
    /// From the primary folder's TOC, or its folder name.
    pub title: String,
    pub version: Option<String>,
    /// True when any folder in the group targets Forever, because zips are often
    /// only partly for Forever.
    pub targets_forever: bool,
    /// The most recent change in any folder, in Unix seconds.
    pub modified: i64,
}

/// Collapse multi-folder addons. A folder joins group `B` when it's named `B-…` or
/// `B_…` and either a bare `B` exists (`BigWigs` owns `BigWigs_Core`) or `B-Core`
/// does, which then leads the group (`DBM-Core` owns `DBM-GUI`). Folders whose TOCs
/// carry different project ids stay apart, because the names only look related.
pub fn group_unmanaged<I, S>(folders: &[LocalFolder], managed_folders: I) -> Vec<AddonGroup>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let leaders = group_leaders(folders, &lowercase_set(managed_folders));
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); folders.len()];
    for (i, leader) in leaders.iter().enumerate() {
        if let Some(leader) = *leader
            && leader != i
        {
            members[leader].push(i);
        }
    }

    let mut groups: Vec<AddonGroup> = leaders
        .iter()
        .enumerate()
        .filter(|&(i, leader)| *leader == Some(i))
        .map(|(i, _)| {
            let primary = &folders[i];
            let group: Vec<&LocalFolder> = std::iter::once(primary)
                .chain(members[i].iter().map(|&m| &folders[m]))
                .collect();
            AddonGroup {
                primary: primary.name.clone(),
                folders: group.iter().map(|f| f.name.clone()).collect(),
                title: primary
                    .toc
                    .title
                    .clone()
                    .unwrap_or_else(|| primary.name.clone()),
                version: group.iter().find_map(|f| f.toc.version.clone()),
                targets_forever: group.iter().any(|f| f.targets_forever),
                modified: group.iter().map(|f| f.modified).max().unwrap_or(0),
            }
        })
        .collect();
    groups.sort_by_cached_key(|g| g.primary.to_lowercase());
    groups
}

/// For each folder, the one leading its group under Other folders: itself, or the folder it
/// hangs off. `None` for a managed folder, which isn't in any group.
fn group_leaders(folders: &[LocalFolder], managed: &HashSet<String>) -> Vec<Option<usize>> {
    let lower: Vec<String> = folders.iter().map(|f| f.name.to_lowercase()).collect();
    let by_name: HashMap<String, usize> = lower
        .iter()
        .enumerate()
        .filter(|(_, name)| !managed.contains(*name))
        .map(|(i, name)| (name.clone(), i))
        .collect();
    lower
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if managed.contains(name) {
                return None;
            }
            let leader = group_primary(name, &by_name)
                .filter(|&primary| {
                    primary != i && !tocs_contradict(&folders[primary].toc, &folders[i].toc)
                })
                .unwrap_or(i);
            Some(leader)
        })
        .collect()
}

/// The folder that leads this folder's group, trying the shortest prefix first so
/// `BigWigs_Classic_Foo` lands under `BigWigs`, not `BigWigs_Classic`.
fn group_primary(lower_name: &str, by_name: &HashMap<String, usize>) -> Option<usize> {
    lower_name
        .char_indices()
        .filter(|&(i, c)| i > 0 && (c == '-' || c == '_'))
        .find_map(|(i, _)| {
            let base = &lower_name[..i];
            by_name
                .get(base)
                .or_else(|| by_name.get(&format!("{base}-core")))
                .or_else(|| by_name.get(&format!("{base}_core")))
                .copied()
        })
}

fn tocs_contradict(a: &Toc, b: &Toc) -> bool {
    differ(a.wowi_id.as_deref(), b.wowi_id.as_deref())
        || differ(a.curse_id.as_deref(), b.curse_id.as_deref())
        || differ(a.wago_id.as_deref(), b.wago_id.as_deref())
}

fn differ(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a != b)
}

/// Two TOCs naming the same project on some site, with none saying otherwise.
fn share_an_id(a: &Toc, b: &Toc) -> bool {
    let same =
        |x: &Option<String>, y: &Option<String>| matches!((x, y), (Some(x), Some(y)) if x == y);
    let shared = same(&a.wowi_id, &b.wowi_id)
        || same(&a.curse_id, &b.curse_id)
        || same(&a.wago_id, &b.wago_id);
    shared && !tocs_contradict(a, b)
}

/// How a local folder was recognized as a catalog addon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchHow {
    WowiId,
    CurseId,
    WagoId,
    MainFolder,
}

/// An addon already on disk that can start getting updates without a reinstall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub addon_id: String,
    /// The local folders this addon would own, spelled as on disk, main folder first.
    pub folders: Vec<String>,
    pub local_version: Option<String>,
    pub how: MatchHow,
}

/// Match unmanaged folders to catalog addons, by TOC ids first and then by the addon's
/// main folder name. Titles are never used, because same-named projects exist, and a
/// folder whose ids contradict an addon is never matched to it by name.
///
/// Only trusted entries can match, because a match hands the folder over to that entry's
/// updates, and a stranger's ids and folder names are only claims. A match by id also needs
/// the entry to list the folder, so copying an id can't take over a folder the entry has
/// nothing to do with. Whatever doesn't match stays with the player, under Other folders.
///
/// One adoption only ever takes folders the disk ties together: the same group under Other
/// folders, or TOCs carrying the same id. An entry listing a folder doesn't make it part of
/// the addon, so a separate addon the player has under its own name stays theirs, and the
/// first update asks about it.
pub fn match_to_catalog<I, S>(
    folders: &[LocalFolder],
    catalog: &Catalog,
    managed_folders: I,
) -> Vec<Adoption>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let index = CatalogIds::build(&catalog.addons);
    let local_by_name: HashMap<String, usize> = folders
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.to_lowercase(), i))
        .collect();
    let claimed = lowercase_set(managed_folders);
    let mut matcher = Adopter {
        folders,
        addons: &catalog.addons,
        local_by_name,
        leaders: group_leaders(folders, &claimed),
        claimed,
        found: BTreeMap::new(),
    };

    for (i, folder) in folders.iter().enumerate() {
        if matcher.can_adopt(folder)
            && let Some((addon, how)) = index.by_toc_ids(folder, &catalog.addons)
        {
            matcher.adopt(addon, i, how);
        }
    }
    // After all id matches, so a folder with ids of its own goes to its own addon.
    for addon in matcher.found.keys().copied().collect::<Vec<_>>() {
        matcher.claim_listed_folders(addon);
    }
    for (i, folder) in folders.iter().enumerate() {
        if matcher.can_adopt(folder)
            && let Some(addon) = index.by_main_folder(folder, &catalog.addons)
            && matcher.adopt(addon, i, MatchHow::MainFolder)
        {
            matcher.claim_listed_folders(addon);
        }
    }
    matcher.into_adoptions()
}

/// The trusted entries, by what could match them, most downloaded first.
struct CatalogIds {
    wowi: HashMap<String, Vec<usize>>,
    curse: HashMap<String, Vec<usize>>,
    wago: HashMap<String, Vec<usize>>,
    main_folders: HashMap<String, Vec<usize>>,
}

impl CatalogIds {
    fn build(addons: &[Addon]) -> Self {
        let mut index = Self {
            wowi: HashMap::new(),
            curse: HashMap::new(),
            wago: HashMap::new(),
            main_folders: HashMap::new(),
        };
        for (i, addon) in addons.iter().enumerate().filter(|(_, a)| a.trusted) {
            if let Some(id) = addon_wowi_id(addon) {
                index.wowi.entry(id).or_default().push(i);
            }
            if let Some(id) = &addon.ids.curse {
                index.curse.entry(id.clone()).or_default().push(i);
            }
            if let Some(id) = &addon.ids.wago {
                index.wago.entry(id.clone()).or_default().push(i);
            }
            if let Some(main) = addon.folders.first() {
                index
                    .main_folders
                    .entry(main.to_lowercase())
                    .or_default()
                    .push(i);
            }
        }
        // Stable sort, so the catalog's order settles exact ties.
        for map in [
            &mut index.wowi,
            &mut index.curse,
            &mut index.wago,
            &mut index.main_folders,
        ] {
            for candidates in map.values_mut() {
                candidates.sort_by_key(|&i| Reverse(popularity(&addons[i])));
            }
        }
        index
    }

    fn by_toc_ids(&self, folder: &LocalFolder, addons: &[Addon]) -> Option<(usize, MatchHow)> {
        let name = folder.name.to_lowercase();
        let look = |map: &HashMap<String, Vec<usize>>, id: &Option<String>| {
            map.get(id.as_deref()?)?
                .iter()
                .copied()
                .find(|&addon| lists_folder(&addons[addon], &name))
        };
        let toc = &folder.toc;
        look(&self.wowi, &toc.wowi_id)
            .map(|addon| (addon, MatchHow::WowiId))
            .or_else(|| look(&self.curse, &toc.curse_id).map(|addon| (addon, MatchHow::CurseId)))
            .or_else(|| look(&self.wago, &toc.wago_id).map(|addon| (addon, MatchHow::WagoId)))
    }

    fn by_main_folder(&self, folder: &LocalFolder, addons: &[Addon]) -> Option<usize> {
        self.main_folders
            .get(&folder.name.to_lowercase())?
            .iter()
            .copied()
            .find(|&addon| !contradicts(&folder.toc, &addons[addon]))
    }
}

fn lists_folder(addon: &Addon, lower_name: &str) -> bool {
    addon.folders.iter().any(|f| f.to_lowercase() == lower_name)
}

fn popularity(addon: &Addon) -> (u64, u32) {
    (addon.downloads, addon.stars)
}

/// WoWInterface addons carry their id in `source` too, sometimes only there.
fn addon_wowi_id(addon: &Addon) -> Option<String> {
    addon.ids.wowi.clone().or_else(|| match addon.source {
        Source::Wowi { id } => Some(id.to_string()),
        _ => None,
    })
}

fn contradicts(toc: &Toc, addon: &Addon) -> bool {
    differ(toc.wowi_id.as_deref(), addon_wowi_id(addon).as_deref())
        || differ(toc.curse_id.as_deref(), addon.ids.curse.as_deref())
        || differ(toc.wago_id.as_deref(), addon.ids.wago.as_deref())
}

struct Adopter<'a> {
    folders: &'a [LocalFolder],
    addons: &'a [Addon],
    local_by_name: HashMap<String, usize>,
    /// By folder: the folder leading its group under Other folders.
    leaders: Vec<Option<usize>>,
    /// Lowercased names of folders already spoken for.
    claimed: HashSet<String>,
    /// By addon index: how it was matched and the local folders it owns.
    found: BTreeMap<usize, (MatchHow, Vec<usize>)>,
}

impl Adopter<'_> {
    /// A folder without a TOC is ignored by the client, so it isn't an addon to adopt.
    fn can_adopt(&self, folder: &LocalFolder) -> bool {
        folder.toc_file.is_some() && !self.claimed.contains(&folder.name.to_lowercase())
    }

    /// Does the disk say this folder belongs with what the addon already has? Its group under
    /// Other folders, or an id its TOC shares with one of them. What the entry lists isn't
    /// evidence: that's only its claim.
    fn tied_to(&self, addon: usize, folder: usize) -> bool {
        let Some((_, owned)) = self.found.get(&addon) else {
            return true;
        };
        owned.iter().any(|&mine| {
            let same_group =
                self.leaders[mine].is_some() && self.leaders[mine] == self.leaders[folder];
            same_group || share_an_id(&self.folders[mine].toc, &self.folders[folder].toc)
        })
    }

    /// False when the addon already matched something on disk this folder has nothing to do
    /// with. The first match settles which of the two it is.
    fn adopt(&mut self, addon: usize, folder: usize, how: MatchHow) -> bool {
        if !self.tied_to(addon, folder) {
            return false;
        }
        self.claimed
            .insert(self.folders[folder].name.to_lowercase());
        self.found
            .entry(addon)
            .or_insert((how, Vec::new()))
            .1
            .push(folder);
        true
    }

    /// The addon's other folders come along, so a later update replaces all of them, as long
    /// as the disk ties them to what matched. One that's tied only through another listed
    /// folder still comes, so this goes round until nothing more joins.
    fn claim_listed_folders(&mut self, addon: usize) {
        let addons = self.addons;
        let mut joined = true;
        while joined {
            joined = false;
            for listed in &addons[addon].folders {
                let lower = listed.to_lowercase();
                let Some(&i) = self.local_by_name.get(&lower) else {
                    continue;
                };
                let local = &self.folders[i];
                if self.can_adopt(local)
                    && !contradicts(&local.toc, &addons[addon])
                    && self.tied_to(addon, i)
                    && let Some((_, owned)) = self.found.get_mut(&addon)
                {
                    owned.push(i);
                    self.claimed.insert(lower);
                    joined = true;
                }
            }
        }
    }

    fn into_adoptions(self) -> Vec<Adoption> {
        let mut adoptions: Vec<Adoption> = self
            .found
            .iter()
            .map(|(&addon, (how, owned))| {
                let addon = &self.addons[addon];
                let listed_position = |i: &usize| {
                    let name = self.folders[*i].name.to_lowercase();
                    addon
                        .folders
                        .iter()
                        .position(|f| f.to_lowercase() == name)
                        .unwrap_or(usize::MAX)
                };
                let mut owned = owned.clone();
                owned.sort_by_key(listed_position);
                Adoption {
                    addon_id: addon.id.clone(),
                    folders: owned
                        .iter()
                        .map(|&i| self.folders[i].name.clone())
                        .collect(),
                    local_version: owned
                        .iter()
                        .find_map(|&i| self.folders[i].toc.version.clone()),
                    how: *how,
                }
            })
            .collect();
        adoptions.sort_by(|a, b| a.addon_id.cmp(&b.addon_id));
        adoptions
    }
}

fn lowercase_set<I, S>(names: I) -> HashSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .map(|name| name.as_ref().to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ExternalIds;

    const NO_MANAGED: [&str; 0] = [];

    fn write_toc(addons: &Path, folder: &str, file: &str, text: &str) {
        let dir = addons.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), text).unwrap();
    }

    fn by_name<'a>(folders: &'a [LocalFolder], name: &str) -> &'a LocalFolder {
        folders.iter().find(|f| f.name == name).unwrap()
    }

    /// A local folder with a TOC, as a scan would produce it.
    fn folder(name: &str, toc_text: &str) -> LocalFolder {
        let toc = toc::parse(toc_text);
        LocalFolder {
            name: name.to_string(),
            toc_file: Some(format!("{name}.toc")),
            targets_forever: toc.targets_forever(),
            toc,
            modified: 0,
        }
    }

    fn names(folders: &[&str]) -> Vec<LocalFolder> {
        folders
            .iter()
            .map(|n| folder(n, "## Interface: 16001\n"))
            .collect()
    }

    fn addon(id: &str, folders: &[&str], downloads: u64) -> Addon {
        Addon {
            id: id.to_string(),
            name: id.to_string(),
            folders: folders.iter().map(|f| f.to_string()).collect(),
            downloads,
            trusted: true,
            ..Default::default()
        }
    }

    fn catalog(addons: Vec<Addon>) -> Catalog {
        Catalog {
            addons,
            ..Default::default()
        }
    }

    fn group_names(groups: &[AddonGroup]) -> Vec<(&str, Vec<&str>)> {
        groups
            .iter()
            .map(|g| {
                (
                    g.primary.as_str(),
                    g.folders.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    // ---- scan_addons ----

    #[test]
    fn scans_real_toc_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let addons = temp.path();

        write_toc(
            addons,
            "!BugGrabber",
            "!BugGrabber.toc",
            "## Interface: 16001\r\n## Title: !BugGrabber\r\n## Version: v11\r\n## X-Curse-Project-ID: 12345\r\nBugGrabber.lua\r\n",
        );
        // Questie's plain TOC declares 00000, and the Forever TOC is the real one.
        write_toc(
            addons,
            "Questie",
            "Questie.toc",
            "## Interface: 00000\n## Title: Questie\n",
        );
        write_toc(
            addons,
            "Questie",
            "Questie_Forever.toc",
            "## Interface: 16001\n## Title: Questie\n## Version: 12.6.2\n",
        );
        write_toc(
            addons,
            "Questie",
            "Questie_Vanilla.toc",
            "## Interface: 11508\n",
        );
        // A BigWigs-style multi-folder set, only partly for Forever.
        write_toc(
            addons,
            "BigWigs",
            "BigWigs.toc",
            "## Interface: 120105, 16001\n## Title: BigWigs\n",
        );
        write_toc(
            addons,
            "BigWigs_Core",
            "BigWigs_Core.toc",
            "## Interface: 16001\n",
        );
        write_toc(
            addons,
            "BigWigs_Retail",
            "BigWigs_Retail.toc",
            "## Interface: 120105\n",
        );
        // A folder the client would ignore.
        fs::create_dir_all(addons.join("OpenMe")).unwrap();
        fs::write(addons.join("OpenMe").join("readme.txt"), "hi").unwrap();

        let folders = scan_addons(addons).unwrap();
        let order: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            order,
            [
                "!BugGrabber",
                "BigWigs",
                "BigWigs_Core",
                "BigWigs_Retail",
                "OpenMe",
                "Questie"
            ]
        );

        let bug = by_name(&folders, "!BugGrabber");
        assert_eq!(bug.toc_file.as_deref(), Some("!BugGrabber.toc"));
        assert_eq!(bug.toc.version.as_deref(), Some("v11"));
        assert_eq!(bug.toc.curse_id.as_deref(), Some("12345"));
        assert!(bug.targets_forever);
        assert!(bug.modified > 0);

        let questie = by_name(&folders, "Questie");
        assert_eq!(questie.toc_file.as_deref(), Some("Questie_Forever.toc"));
        assert_eq!(questie.toc.version.as_deref(), Some("12.6.2"));
        assert!(questie.targets_forever);

        assert!(by_name(&folders, "BigWigs").targets_forever);
        assert!(!by_name(&folders, "BigWigs_Retail").targets_forever);

        let open_me = by_name(&folders, "OpenMe");
        assert_eq!(open_me.toc_file, None);
        assert_eq!(open_me.toc, Toc::default());
        assert!(!open_me.targets_forever);
    }

    #[test]
    fn skips_files_and_dot_folders() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("stray.txt"), "x").unwrap();
        fs::write(temp.path().join("Fake.toc"), "## Interface: 16001\n").unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        write_toc(
            temp.path(),
            ".hidden",
            ".hidden.toc",
            "## Interface: 16001\n",
        );
        write_toc(temp.path(), "Real", "Real.toc", "## Interface: 16001\n");
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Real");
    }

    #[test]
    fn sorts_case_insensitively() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["zeta", "Alpha", "beta", "Gamma"] {
            fs::create_dir_all(temp.path().join(name)).unwrap();
        }
        let folders = scan_addons(temp.path()).unwrap();
        let order: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(order, ["Alpha", "beta", "Gamma", "zeta"]);
    }

    #[test]
    fn a_missing_addons_folder_means_no_addons() {
        let temp = tempfile::tempdir().unwrap();
        assert!(scan_addons(&temp.path().join("AddOns")).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_addons_path_is_an_error_naming_it() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("AddOns");
        fs::write(&file, "not a folder").unwrap();
        let error = scan_addons(&file).unwrap_err().to_string();
        assert!(error.contains("AddOns"), "{error}");
    }

    #[test]
    fn a_folder_named_like_a_toc_is_not_a_toc() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Foo").join("Foo.toc")).unwrap();
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(folders[0].toc_file, None);
    }

    #[test]
    fn a_toc_with_no_matching_name_is_ignored_like_the_client_does() {
        let temp = tempfile::tempdir().unwrap();
        write_toc(temp.path(), "Foo", "Bar.toc", "## Interface: 16001\n");
        write_toc(
            temp.path(),
            "Baz",
            "Baz_Vanilla.toc",
            "## Interface: 11508\n",
        );
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(by_name(&folders, "Foo").toc_file, None);
        assert_eq!(by_name(&folders, "Baz").toc_file, None);
    }

    #[test]
    fn toc_files_with_odd_case_and_multibyte_names_do_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        write_toc(
            temp.path(),
            "Foo",
            "FOO_FOREVER.TOC",
            "## Interface: 16001\n",
        );
        write_toc(temp.path(), "Foo", "€.lua", "x");
        write_toc(temp.path(), "Foo", "ééé", "x");
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(folders[0].toc_file.as_deref(), Some("FOO_FOREVER.TOC"));
        assert!(folders[0].targets_forever);
    }

    #[test]
    fn only_the_first_256_kb_of_a_toc_is_read() {
        let temp = tempfile::tempdir().unwrap();
        let mut text = String::from("## Title: Head\n");
        text.push_str(&"-- filler line\n".repeat(20_000));
        text.push_str("## Notes: Past the limit\n");
        assert!(text.len() as u64 > MAX_TOC_BYTES);
        write_toc(temp.path(), "Big", "Big.toc", &text);
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(folders[0].toc.title.as_deref(), Some("Head"));
        assert_eq!(folders[0].toc.notes, None);
    }

    #[test]
    fn invalid_utf8_and_a_bom_in_a_toc_are_tolerated() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("Pawn");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Pawn.toc"),
            b"\xef\xbb\xbf## Interface: 16001\n## Notes: caf\xe9\n## Title: Pawn\n",
        )
        .unwrap();
        let folders = scan_addons(temp.path()).unwrap();
        assert_eq!(folders[0].toc.title.as_deref(), Some("Pawn"));
        assert!(folders[0].targets_forever);
    }

    // Links are the usual way to develop an addon, so they must count as folders.
    #[cfg(unix)]
    #[test]
    fn linked_folders_count_and_broken_links_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let addons = temp.path().join("AddOns");
        fs::create_dir_all(&addons).unwrap();
        write_toc(temp.path(), "Dev", "Dev.toc", "## Interface: 16001\n");
        std::os::unix::fs::symlink(temp.path().join("Dev"), addons.join("Dev")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("Gone"), addons.join("Gone")).unwrap();
        let folders = scan_addons(&addons).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].toc_file.as_deref(), Some("Dev.toc"));
    }

    // Junctions need no special rights, unlike symlinks, so they're the common case on Windows.
    #[cfg(windows)]
    #[test]
    fn junctioned_folders_count_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let addons = temp.path().join("AddOns");
        fs::create_dir_all(&addons).unwrap();
        write_toc(temp.path(), "Dev", "Dev.toc", "## Interface: 16001\n");
        let linked = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(addons.join("Dev"))
            .arg(temp.path().join("Dev"))
            .output()
            .unwrap();
        assert!(linked.status.success(), "{linked:?}");

        let folders = scan_addons(&addons).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].toc_file.as_deref(), Some("Dev.toc"));
        assert!(folders[0].targets_forever);
    }

    #[test]
    fn scanning_hundreds_of_folders_is_fast() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..500 {
            let name = format!("Addon{i}");
            write_toc(
                temp.path(),
                &name,
                &format!("{name}.toc"),
                "## Interface: 16001\n## Title: Something\n## Version: 1.2.3\nCore.lua\n",
            );
            fs::write(temp.path().join(&name).join("Core.lua"), "-- code").unwrap();
        }
        let best = (0..5)
            .map(|_| {
                let start = std::time::Instant::now();
                assert_eq!(scan_addons(temp.path()).unwrap().len(), 500);
                start.elapsed()
            })
            .min()
            .unwrap();
        println!("scan of 500 folders: {best:?}");
        // Very loose. This only catches something pathological: antivirus can sit on a
        // fresh temp folder, and a busy machine or CI runner can be slower still.
        assert!(best.as_millis() < 5000, "took {best:?}");
    }

    // ---- group_unmanaged ----

    #[test]
    fn a_bare_folder_owns_its_prefixed_siblings() {
        let folders = names(&["BigWigs", "BigWigs_Core", "BigWigs_Plugins", "Questie"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [
                (
                    "BigWigs",
                    vec!["BigWigs", "BigWigs_Core", "BigWigs_Plugins"]
                ),
                ("Questie", vec!["Questie"]),
            ]
        );
    }

    #[test]
    fn core_leads_when_there_is_no_bare_folder() {
        let folders = names(&["DBM-Core", "DBM-GUI", "DBM-Raids-Vanilla", "DBM_Extras"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [(
                "DBM-Core",
                vec!["DBM-Core", "DBM-GUI", "DBM-Raids-Vanilla", "DBM_Extras"]
            )]
        );
    }

    #[test]
    fn underscore_core_leads_too() {
        let folders = names(&["Foo_Core", "Foo_Options"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [("Foo_Core", vec!["Foo_Core", "Foo_Options"])]
        );
    }

    #[test]
    fn a_bare_folder_beats_core_as_the_leader() {
        let folders = names(&["Foo", "Foo-Core", "Foo-GUI"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [("Foo", vec!["Foo", "Foo-Core", "Foo-GUI"])]
        );
    }

    #[test]
    fn matching_ignores_case() {
        let folders = names(&["bigwigs", "BIGWIGS_Core", "dbm-core", "DBM-GUI"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [
                ("bigwigs", vec!["bigwigs", "BIGWIGS_Core"]),
                ("dbm-core", vec!["dbm-core", "DBM-GUI"])
            ]
        );
    }

    #[test]
    fn deep_names_land_under_the_shortest_owner() {
        let folders = names(&["BigWigs", "BigWigs_Classic", "BigWigs_Classic_Foo"]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].folders.len(), 3);
    }

    #[test]
    fn unrelated_names_never_group() {
        let folders = names(&[
            "Titan",
            "TitanBag",
            "Auc-Advanced",
            "Auc-Stat-Purchased",
            "Lib_One",
            "Lib_Two",
            "DBMX-Thing",
            "-Odd",
            "_Odd",
        ]);
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(groups.len(), folders.len(), "{:?}", group_names(&groups));
    }

    #[test]
    fn different_project_ids_keep_folders_apart() {
        let folders = vec![
            folder("DBM-Core", "## X-Curse-Project-ID: 8814\n"),
            folder("DBM-GUI", "## X-Curse-Project-ID: 8814\n"),
            folder("DBM-PvP", "## X-Curse-Project-ID: 99999\n"),
            folder("DBM-NoIds", ""),
        ];
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [
                ("DBM-Core", vec!["DBM-Core", "DBM-GUI", "DBM-NoIds"]),
                ("DBM-PvP", vec!["DBM-PvP"]),
            ]
        );
    }

    #[test]
    fn managed_folders_are_left_out_and_do_not_lead_groups() {
        let folders = names(&["BigWigs", "BigWigs_Core", "BigWigs_Extra", "Questie"]);
        let groups = group_unmanaged(&folders, ["bigwigs", "BIGWIGS_CORE"]);
        assert_eq!(
            group_names(&groups),
            [
                ("BigWigs_Extra", vec!["BigWigs_Extra"]),
                ("Questie", vec!["Questie"])
            ]
        );
    }

    #[test]
    fn a_group_summarizes_its_folders() {
        let mut core = folder(
            "DBM-Core",
            "## Interface: 120105\n## Title: Deadly Boss Mods\n",
        );
        core.modified = 100;
        let mut gui = folder("DBM-GUI", "## Interface: 16001\n## Version: 11.5\n");
        gui.modified = 300;
        let groups = group_unmanaged(&[core, gui], NO_MANAGED);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "Deadly Boss Mods");
        assert_eq!(groups[0].version.as_deref(), Some("11.5"));
        assert!(groups[0].targets_forever);
        assert_eq!(groups[0].modified, 300);
    }

    #[test]
    fn a_folder_without_a_toc_is_still_listed_under_its_own_name() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("OpenMe")).unwrap();
        let folders = scan_addons(temp.path()).unwrap();
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(groups[0].title, "OpenMe");
        assert!(!groups[0].targets_forever);
    }

    // ---- match_to_catalog ----

    #[test]
    fn matches_by_each_kind_of_toc_id() {
        let mut wowi = addon("wowi:5086", &["Main1", "A"], 1);
        wowi.source = Source::Wowi { id: 5086 };
        let mut explicit_wowi = addon("gh:a/explicit", &["Main2", "B"], 1);
        explicit_wowi.ids.wowi = Some("777".to_string());
        let mut curse = addon("gh:a/curse", &["Main3", "C"], 1);
        curse.ids.curse = Some("2382".to_string());
        let mut wago = addon("gh:a/wago", &["Main4", "D", "E"], 1);
        wago.ids.wago = Some("e56n8DG8".to_string());

        let folders = vec![
            folder("A", "## X-WoWI-ID: 5086\n## Version: 1\n"),
            folder("B", "## X-WoWI-ID: 777\n"),
            folder("C", "## X-Curse-Project-ID: 2382\n"),
            folder("D", "## X-Wago-ID: e56n8DG8\n"),
            folder("E", "## X-Wago-ID: E56N8DG8\n"),
        ];
        let found = match_to_catalog(
            &folders,
            &catalog(vec![wowi, explicit_wowi, curse, wago]),
            NO_MANAGED,
        );
        let summary: Vec<(&str, &str, MatchHow)> = found
            .iter()
            .map(|a| (a.addon_id.as_str(), a.folders[0].as_str(), a.how))
            .collect();
        assert_eq!(
            summary,
            [
                ("gh:a/curse", "C", MatchHow::CurseId),
                ("gh:a/explicit", "B", MatchHow::WowiId),
                ("gh:a/wago", "D", MatchHow::WagoId),
                ("wowi:5086", "A", MatchHow::WowiId),
            ]
        );
        assert_eq!(found[3].local_version.as_deref(), Some("1"));
    }

    #[test]
    fn wowi_id_wins_over_curse_id() {
        let mut by_curse = addon("gh:a/curse", &["Main1", "X"], 1);
        by_curse.ids.curse = Some("1".to_string());
        let mut by_wowi = addon("gh:a/wowi", &["Main2", "X"], 1);
        by_wowi.ids.wowi = Some("2".to_string());
        let folders = vec![folder("X", "## X-Curse-Project-ID: 1\n## X-WoWI-ID: 2\n")];
        let found = match_to_catalog(&folders, &catalog(vec![by_curse, by_wowi]), NO_MANAGED);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addon_id, "gh:a/wowi");
    }

    #[test]
    fn matches_the_main_folder_in_any_case_and_ties_go_to_the_most_downloaded() {
        let catalog = catalog(vec![
            addon("gh:small/questie", &["Questie"], 10),
            addon("gh:big/questie", &["QUESTIE"], 5_000),
            addon("gh:mid/questie", &["questie"], 100),
        ]);
        let folders = vec![folder("Questie", "## Version: 12\n")];
        let found = match_to_catalog(&folders, &catalog, NO_MANAGED);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addon_id, "gh:big/questie");
        assert_eq!(found[0].how, MatchHow::MainFolder);
        assert_eq!(found[0].local_version.as_deref(), Some("12"));
    }

    #[test]
    fn ids_beat_the_folder_name() {
        let mut real = addon("gh:real/thing", &["SomethingElse", "Thing"], 1);
        real.ids.wowi = Some("42".to_string());
        let lookalike = addon("gh:fake/thing", &["Thing"], 1_000_000);
        let folders = vec![folder("Thing", "## X-WoWI-ID: 42\n")];
        let found = match_to_catalog(&folders, &catalog(vec![lookalike, real]), NO_MANAGED);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addon_id, "gh:real/thing");
    }

    #[test]
    fn contradicting_ids_block_a_folder_name_match() {
        // Same folder name, different project: the ForeverPlus trap.
        let mut other_project = addon("gh:other/foreverplus", &["ForeverPlus"], 1_000_000);
        other_project.ids.curse = Some("111".to_string());
        let folders = vec![folder("ForeverPlus", "## X-Curse-Project-ID: 222\n")];
        assert!(
            match_to_catalog(&folders, &catalog(vec![other_project.clone()]), NO_MANAGED)
                .is_empty()
        );

        // The next candidate without a contradiction still gets its chance.
        let same_name = addon("gh:right/foreverplus", &["ForeverPlus"], 5);
        let found = match_to_catalog(
            &folders,
            &catalog(vec![other_project, same_name]),
            NO_MANAGED,
        );
        assert_eq!(found[0].addon_id, "gh:right/foreverplus");
    }

    #[test]
    fn a_wowi_source_id_contradicts_too() {
        let mut other = addon("wowi:100", &["Thing"], 10);
        other.source = Source::Wowi { id: 100 };
        let folders = vec![folder("Thing", "## X-WoWI-ID: 200\n")];
        assert!(match_to_catalog(&folders, &catalog(vec![other]), NO_MANAGED).is_empty());
    }

    #[test]
    fn titles_alone_never_match() {
        let mut same_title = addon("gh:a/questie", &["TotallyDifferent"], 1_000);
        same_title.name = "Questie".to_string();
        let folders = vec![folder("MyQuestie", "## Title: Questie\n")];
        assert!(match_to_catalog(&folders, &catalog(vec![same_title]), NO_MANAGED).is_empty());
    }

    #[test]
    fn only_the_main_folder_matches_by_name() {
        let addon = addon("gh:a/dbm", &["DBM-Core", "DBM-GUI"], 1);
        let folders = names(&["DBM-GUI"]);
        assert!(match_to_catalog(&folders, &catalog(vec![addon]), NO_MANAGED).is_empty());
    }

    #[test]
    fn a_multi_folder_addon_is_one_adoption_with_its_listed_folders() {
        let catalog = catalog(vec![addon(
            "gh:deadlybossmods/dbm",
            &["DBM-Core", "DBM-GUI", "DBM-Raids"],
            1,
        )]);
        let mut local = names(&["dbm-gui", "DBM-Core", "DBM-VP-Extra", "Unrelated"]);
        local[1].toc.version = Some("11.5".to_string());
        let found = match_to_catalog(&local, &catalog, NO_MANAGED);
        assert_eq!(found.len(), 1);
        // On-disk spelling, main folder first, and nothing the addon doesn't list.
        assert_eq!(found[0].folders, ["DBM-Core", "dbm-gui"]);
        assert_eq!(found[0].local_version.as_deref(), Some("11.5"));
    }

    #[test]
    fn folders_sharing_an_id_merge_into_one_adoption() {
        let mut bigwigs = addon(
            "gh:bigwigs/bigwigs",
            &["BigWigs", "BigWigs_Core", "BigWigs_Plugins"],
            1,
        );
        bigwigs.ids.wowi = Some("5086".to_string());
        let folders = vec![
            folder("BigWigs", "## X-WoWI-ID: 5086\n## Version: v402\n"),
            folder("BigWigs_Core", "## X-WoWI-ID: 5086\n"),
            folder("BigWigs_Plugins", "## Interface: 16001\n"),
        ];
        let found = match_to_catalog(&folders, &catalog(vec![bigwigs]), NO_MANAGED);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].how, MatchHow::WowiId);
        assert_eq!(
            found[0].folders,
            ["BigWigs", "BigWigs_Core", "BigWigs_Plugins"]
        );
        assert_eq!(found[0].local_version.as_deref(), Some("v402"));
    }

    #[test]
    fn an_id_match_needs_the_entry_to_list_the_folder() {
        // A TOC id is only a claim. Otherwise a repo copying BigWigs's id would adopt it
        // and, listing none of its folders, delete it at the next update.
        let mut stranger = addon("gh:stranger/thing", &["Unrelated"], 1_000_000);
        stranger.ids.wowi = Some("5086".to_string());
        let folders = vec![folder("BigWigs", "## X-WoWI-ID: 5086\n")];
        assert!(match_to_catalog(&folders, &catalog(vec![stranger]), NO_MANAGED).is_empty());
        assert_eq!(
            group_names(&group_unmanaged(&folders, NO_MANAGED)),
            [("BigWigs", vec!["BigWigs"])]
        );
    }

    #[test]
    fn an_id_goes_to_an_entry_that_lists_the_folder_even_if_another_is_bigger() {
        let mut big = addon("gh:a/big", &["Elsewhere"], 1_000_000);
        big.ids.wowi = Some("7".to_string());
        let mut small = addon("gh:a/small", &["Thing"], 1);
        small.ids.wowi = Some("7".to_string());
        let folders = vec![folder("Thing", "## X-WoWI-ID: 7\n")];
        let found = match_to_catalog(&folders, &catalog(vec![big, small]), NO_MANAGED);
        assert_eq!(found[0].addon_id, "gh:a/small");
        assert_eq!(found[0].how, MatchHow::WowiId);
    }

    #[test]
    fn an_untrusted_entry_never_matches_however_well_it_fits() {
        let mut by_id = addon(
            "gh:stranger/bigwigs",
            &["BigWigs", "BigWigs_Core"],
            9_000_000,
        );
        by_id.ids.wowi = Some("5086".to_string());
        by_id.trusted = false;
        let mut by_name = addon("gh:stranger/questie", &["Questie"], 9_000_000);
        by_name.trusted = false;
        let folders = vec![
            folder("BigWigs", "## X-WoWI-ID: 5086\n"),
            folder("BigWigs_Core", "## X-WoWI-ID: 5086\n"),
            folder("Questie", ""),
        ];
        assert!(match_to_catalog(&folders, &catalog(vec![by_id, by_name]), NO_MANAGED).is_empty());
        assert_eq!(
            group_names(&group_unmanaged(&folders, NO_MANAGED)),
            [
                ("BigWigs", vec!["BigWigs", "BigWigs_Core"]),
                ("Questie", vec!["Questie"]),
            ],
            "so they stay under Other folders"
        );
    }

    #[test]
    fn a_trusted_entry_wins_however_many_downloads_a_stranger_reports() {
        let mut stranger = addon("gh:0-free/details", &["Details"], 9_000_000);
        stranger.ids.wowi = Some("7".to_string());
        stranger.trusted = false;
        let mut real = addon("gh:tercioo/details", &["Details"], 0);
        real.ids.wowi = Some("7".to_string());

        let by_id = vec![folder("Details", "## X-WoWI-ID: 7\n")];
        let both = catalog(vec![stranger.clone(), real.clone()]);
        assert_eq!(
            match_to_catalog(&by_id, &both, NO_MANAGED)[0].addon_id,
            "gh:tercioo/details"
        );

        stranger.ids.wowi = None;
        real.ids.wowi = None;
        let by_name = vec![folder("Details", "")];
        let both = catalog(vec![stranger, real]);
        assert_eq!(
            match_to_catalog(&by_name, &both, NO_MANAGED)[0].addon_id,
            "gh:tercioo/details"
        );
    }

    #[test]
    fn a_listed_folder_the_disk_does_not_tie_to_the_match_stays_the_players() {
        // Side is its own row under Other folders, maybe an addon from somewhere else. The
        // entry listing it is only a claim, and the entry's own id is no evidence either.
        let mut foo = addon("gh:a/foo", &["Foo", "Side", "Foo_Options"], 1);
        foo.ids.wowi = Some("5".to_string());
        let by_name = vec![
            folder("Foo", "## Interface: 16001\n"),
            folder("Foo_Options", "## Interface: 16001\n"),
            folder("Side", "## Interface: 16001\n"),
        ];
        let by_id = vec![
            folder("Foo", "## X-WoWI-ID: 5\n"),
            folder("Foo_Options", "## Interface: 16001\n"),
            folder("Side", "## Interface: 16001\n"),
        ];
        for local in [by_name, by_id] {
            let found = match_to_catalog(&local, &catalog(vec![foo.clone()]), NO_MANAGED);
            assert_eq!(found.len(), 1);
            assert_eq!(
                found[0].folders,
                ["Foo", "Foo_Options"],
                "its own group only"
            );
            assert_eq!(
                group_names(&group_unmanaged(&local, NO_MANAGED)),
                [("Foo", vec!["Foo", "Foo_Options"]), ("Side", vec!["Side"])],
                "what the Other folders page shows"
            );
        }
    }

    #[test]
    fn a_listed_folder_comes_along_when_its_toc_shares_an_id_with_the_match() {
        let sack = addon("gh:a/bugsack", &["BugSack", "!BugGrabber"], 1);
        let shared = vec![
            folder("!BugGrabber", "## X-Curse-Project-ID: 7\n"),
            folder("BugSack", "## X-Curse-Project-ID: 7\n"),
        ];
        let found = match_to_catalog(&shared, &catalog(vec![sack.clone()]), NO_MANAGED);
        assert_eq!(found[0].folders, ["BugSack", "!BugGrabber"]);

        let its_own = vec![
            folder("!BugGrabber", "## X-Curse-Project-ID: 8\n"),
            folder("BugSack", "## X-Curse-Project-ID: 7\n"),
        ];
        let found = match_to_catalog(&its_own, &catalog(vec![sack]), NO_MANAGED);
        assert_eq!(
            found[0].folders,
            ["BugSack"],
            "asked about at the first update"
        );
    }

    #[test]
    fn an_entry_matching_two_separate_addons_on_disk_only_gets_the_first() {
        // The entry claims Side's real id and Foo's name. Side's id matched first, so it's
        // Side, and the player's Foo isn't swept in with it.
        let mut entry = addon("gh:a/both", &["Foo", "Side"], 1);
        entry.ids.curse = Some("999".to_string());
        let local = vec![
            folder("Foo", "## Interface: 16001\n"),
            folder("Side", "## X-Curse-Project-ID: 999\n"),
        ];
        let found = match_to_catalog(&local, &catalog(vec![entry]), NO_MANAGED);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].folders, ["Side"]);
        assert_eq!(found[0].how, MatchHow::CurseId);
    }

    #[test]
    fn the_main_folder_leads_an_adoption_whatever_case_the_disk_uses() {
        // Folded the way the installer folds, so a letter outside ASCII still matches.
        let mut entry = addon("gh:a/aerger", &["Ärger", "Ärger_Optionen"], 1);
        entry.ids.wowi = Some("3".to_string());
        let local = vec![
            folder("ärger", "## Interface: 16001\n"),
            folder("ärger_Optionen", "## X-WoWI-ID: 3\n"),
        ];
        let found = match_to_catalog(&local, &catalog(vec![entry]), NO_MANAGED);
        assert_eq!(found[0].folders, ["ärger", "ärger_Optionen"]);
    }

    #[test]
    fn a_listed_folder_with_its_own_project_id_is_not_swept_along() {
        let mut dbm = addon("gh:a/dbm", &["DBM-Core", "DBM-PvP"], 1);
        dbm.ids.curse = Some("8814".to_string());
        let mut pvp = addon("gh:a/pvp", &["DBM-PvP"], 1);
        pvp.ids.curse = Some("99".to_string());
        let folders = vec![
            folder("DBM-Core", "## X-Curse-Project-ID: 8814\n"),
            folder("DBM-PvP", "## X-Curse-Project-ID: 99\n"),
        ];
        let found = match_to_catalog(&folders, &catalog(vec![dbm, pvp]), NO_MANAGED);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].folders, ["DBM-Core"]);
        assert_eq!(found[1].folders, ["DBM-PvP"]);
    }

    #[test]
    fn managed_folders_are_not_adopted() {
        let mut questie = addon("gh:a/questie", &["Questie"], 1);
        questie.ids.wowi = Some("1".to_string());
        let folders = vec![folder("Questie", "## X-WoWI-ID: 1\n")];
        assert!(
            match_to_catalog(&folders, &catalog(vec![questie.clone()]), ["QUESTIE"]).is_empty()
        );

        // A listed folder that's already managed stays with its owner.
        let mut dbm = addon("gh:a/dbm", &["DBM-Core", "DBM-GUI"], 1);
        dbm.ids.wowi = Some("2".to_string());
        let local = vec![
            folder("DBM-Core", "## X-WoWI-ID: 2\n"),
            folder("DBM-GUI", ""),
        ];
        let found = match_to_catalog(&local, &catalog(vec![dbm]), ["dbm-gui"]);
        assert_eq!(found[0].folders, ["DBM-Core"]);
    }

    #[test]
    fn a_folder_without_a_toc_is_never_adopted() {
        let questie = addon("gh:a/questie", &["Questie"], 1);
        let folders = vec![LocalFolder {
            name: "Questie".to_string(),
            ..Default::default()
        }];
        assert!(match_to_catalog(&folders, &catalog(vec![questie]), NO_MANAGED).is_empty());
    }

    #[test]
    fn the_most_downloaded_wins_when_two_addons_claim_one_id() {
        let ids = ExternalIds {
            wowi: Some("7".to_string()),
            ..Default::default()
        };
        let mut small = addon("gh:a/small", &["X"], 1);
        small.ids = ids.clone();
        let mut big = addon("gh:a/big", &["X"], 100);
        big.ids = ids;
        let folders = vec![folder("X", "## X-WoWI-ID: 7\n")];
        let found = match_to_catalog(&folders, &catalog(vec![small, big]), NO_MANAGED);
        assert_eq!(found[0].addon_id, "gh:a/big");
    }

    #[test]
    fn an_empty_catalog_or_empty_folder_list_matches_nothing() {
        let folders = names(&["Questie"]);
        assert!(match_to_catalog(&folders, &catalog(Vec::new()), NO_MANAGED).is_empty());
        assert!(
            match_to_catalog(&[], &catalog(vec![addon("a", &["A"], 1)]), NO_MANAGED).is_empty()
        );
        let no_folders = addon("a", &[], 1);
        assert!(match_to_catalog(&folders, &catalog(vec![no_folders]), NO_MANAGED).is_empty());
    }

    #[test]
    fn scan_group_and_match_work_together_on_a_real_layout() {
        let temp = tempfile::tempdir().unwrap();
        let addons = temp.path();
        write_toc(
            addons,
            "!BugGrabber",
            "!BugGrabber.toc",
            "## Interface: 16001\n## Version: v11\n",
        );
        write_toc(addons, "Questie", "Questie.toc", "## Interface: 00000\n");
        write_toc(
            addons,
            "Questie",
            "Questie_Forever.toc",
            "## Interface: 16001\n## Version: 12.6.2\n",
        );
        write_toc(
            addons,
            "BigWigs",
            "BigWigs.toc",
            "## Interface: 16001\n## X-WoWI-ID: 5086\n## Version: v402\n",
        );
        write_toc(
            addons,
            "BigWigs_Core",
            "BigWigs_Core.toc",
            "## Interface: 16001\n## X-WoWI-ID: 5086\n",
        );
        write_toc(
            addons,
            "MyOwnAddon",
            "MyOwnAddon.toc",
            "## Interface: 16001\n",
        );
        fs::create_dir_all(addons.join("OpenMe")).unwrap();

        let folders = scan_addons(addons).unwrap();
        let groups = group_unmanaged(&folders, NO_MANAGED);
        assert_eq!(
            group_names(&groups),
            [
                ("!BugGrabber", vec!["!BugGrabber"]),
                ("BigWigs", vec!["BigWigs", "BigWigs_Core"]),
                ("MyOwnAddon", vec!["MyOwnAddon"]),
                ("OpenMe", vec!["OpenMe"]),
                ("Questie", vec!["Questie"]),
            ]
        );

        let mut bigwigs = addon("gh:bigwigs/bigwigs", &["BigWigs", "BigWigs_Core"], 1);
        bigwigs.ids.wowi = Some("5086".to_string());
        let catalog = catalog(vec![
            addon("gh:funkydude/buggrabber", &["!BugGrabber"], 1),
            addon("gh:questie/questie", &["Questie"], 1),
            bigwigs,
        ]);
        let found = match_to_catalog(&folders, &catalog, NO_MANAGED);
        let summary: Vec<(&str, usize, Option<&str>)> = found
            .iter()
            .map(|a| {
                (
                    a.addon_id.as_str(),
                    a.folders.len(),
                    a.local_version.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("gh:bigwigs/bigwigs", 2, Some("v402")),
                ("gh:funkydude/buggrabber", 1, Some("v11")),
                ("gh:questie/questie", 1, Some("12.6.2")),
            ]
        );
    }
}
