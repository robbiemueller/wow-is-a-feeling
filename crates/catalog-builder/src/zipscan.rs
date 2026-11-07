//! Reads what we need from an addon zip: which top-level folders an install would
//! write, whether any of them targets Forever, and the main folder's details. Nothing
//! is extracted. Works over anything that reads and seeks, so remote range readers and
//! in-memory downloads share this code. Names go through the installer's own cleaning
//! and layout rules, so a zip can't show the catalog one list and install another.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};

use feeling_core::extract::{clean_entry_path, planned_folders};
use feeling_core::toc::{self, TOC_SUFFIXES, Toc};
use zip::ZipArchive;
use zip::read::{ArchiveOffset, Config};
use zip::result::ZipError;

use crate::model::{MAX_DEPS, ZipScan, is_short_id};
use crate::text::{clean_author, normalize};

/// No real TOC comes close to this, and it stops a hostile one from filling memory.
const MAX_TOC_BYTES: u64 = 256 * 1024;
/// Huge packs exist, and each TOC can cost a request. The main folder is read
/// first, so this only limits how sure we are about the long tail of a pack.
const MAX_FOLDERS_READ: usize = 64;
/// Far more than any real zip brings: the largest today has 32. A zip past this is
/// treated as having no usable folders, so it never reaches the catalog or the scan cache.
pub const MAX_FOLDERS: usize = 256;
/// Longer than any real folder name, and the most the catalog's gate accepts.
const MAX_FOLDER_NAME_BYTES: usize = 120;
/// Title, notes and category from a TOC are kept in the public scan cache. Real ones are
/// well under this, so a longer one is only ever someone padding that file.
const MAX_TEXT_BYTES: usize = 1024;

const JUNK_FOLDERS: [&str; 8] = [
    "__macosx",
    ".git",
    ".github",
    ".svn",
    "images",
    "openme",
    "screenshots",
    "docs",
];

/// TOC suffixes for other game versions.
const OTHER_GAME_SUFFIXES: [&str; 11] = [
    "Vanilla", "Classic", "TBC", "BCC", "Wrath", "WOTLK", "WOTLKC", "Cata", "Mists", "MOP",
    "Standard",
];

/// Top-level folders that are packaging leftovers, not addons.
pub fn is_junk_folder(name: &str) -> bool {
    JUNK_FOLDERS
        .iter()
        .any(|junk| name.eq_ignore_ascii_case(junk))
}

/// Why a zip couldn't be read.
#[derive(Debug)]
pub enum ScanError {
    /// Reading failed, which over HTTP is worth another try later.
    Io(String),
    /// The bytes aren't a usable zip, and they won't get better.
    Invalid(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Io(detail) | ScanError::Invalid(detail) => {
                write!(f, "couldn't read the zip: {detail}")
            }
        }
    }
}

/// Scan a zip. `hints` are names the main folder is likely to match, such as the
/// repository and the zip's own name. Fails only when the zip can't be read at
/// all: a zip with no usable folders is a successful scan with an empty list, and so
/// is one an install would refuse, because it won't get better by being read again.
pub fn scan_zip<R: Read + Seek>(mut reader: R, hints: &[&str]) -> Result<ZipScan, ScanError> {
    // Known(0) stops the zip crate from scanning the whole file for a prefix, which over
    // HTTP would mean downloading it. A zip that only opens that way is refused below.
    let config = Config {
        archive_offset: ArchiveOffset::Known(0),
    };
    ZipArchive::with_config(config, &mut reader).map_err(|e| match e {
        ZipError::Io(io) => ScanError::Io(io.to_string()),
        other => ScanError::Invalid(other.to_string()),
    })?;
    // Nobody prepends data to addons. A zip that claims to start past its first byte has
    // two directories to choose from, and readers disagree about which one is real. This
    // second look costs nothing for a normal zip: the same bytes are already at hand.
    let mut archive = match ZipArchive::new(reader) {
        Ok(archive) if archive.offset() == 0 => archive,
        _ => return Ok(ZipScan::default()),
    };

    let Some(names) = entry_names(&archive) else {
        return Ok(ZipScan::default());
    };
    let Ok(planned) = planned_folders(&names) else {
        return Ok(ZipScan::default());
    };
    let too_long = |folder: &String| folder.len() > MAX_FOLDER_NAME_BYTES;
    if planned.len() > MAX_FOLDERS || planned.iter().any(too_long) {
        return Ok(ZipScan::default());
    }

    let paths: Vec<(String, usize)> = names
        .iter()
        .enumerate()
        .filter(|(_, (_, is_dir))| !is_dir)
        .filter_map(|(index, (name, _))| Some((clean_entry_path(name).ok()?, index)))
        .collect();
    let loadable = addon_folders(&paths);
    let mut picked: Vec<(String, Option<usize>)> = planned
        .into_iter()
        .map(|folder| {
            let toc = loadable
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&folder))
                .map(|(_, index)| *index);
            (folder, toc)
        })
        .collect();

    let hints: Vec<String> = hints
        .iter()
        .map(|h| normalize(h))
        .filter(|h| !h.is_empty())
        .collect();
    // A folder the game wouldn't load still gets installed, so it's listed, but it can't
    // be what the addon is named after.
    picked.sort_by_key(|(folder, toc)| {
        (toc.is_none(), std::cmp::Reverse(best_match(folder, &hints)))
    });

    let mut folders = Vec::with_capacity(picked.len());
    for (position, (folder, index)) in picked.iter().enumerate() {
        let parsed = index
            .filter(|_| position < MAX_FOLDERS_READ)
            .and_then(|index| read_toc(&mut archive, index));
        folders.push(Folder {
            name: folder.clone(),
            toc: parsed,
        });
    }
    Ok(summarize(folders, &hints))
}

/// Every entry's name and whether it's a directory, read the way the installer reads
/// them. `None` when a name can't be read, which the installer refuses too.
fn entry_names<R: Read + Seek>(archive: &ZipArchive<R>) -> Option<Vec<(String, bool)>> {
    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    (0..archive.len())
        .map(|index| {
            let entry = archive.by_index_data(index).ok()?;
            let name = entry.name().ok()?.into_owned();
            let is_dir = entry.is_dir() || entry.unix_mode().is_some_and(|m| m & S_IFMT == S_IFDIR);
            Some((name, is_dir))
        })
        .collect()
}

/// The addon folders a zip's layout gives, each with the index of the TOC the
/// client would load. That's normally every top-level folder with a TOC named after
/// it. Hand-made zips also put the TOC at the root, leave a version in the folder's
/// name, or wrap everything in one more folder. The installer knows those layouts
/// too, so they're listed under the names they get installed as.
pub fn addon_folders(paths: &[(String, usize)]) -> Vec<(String, usize)> {
    let mut root = Vec::new();
    let mut tops: BTreeMap<&str, Vec<(String, usize)>> = BTreeMap::new();
    let mut wrapped: BTreeMap<(&str, &str), Vec<(String, usize)>> = BTreeMap::new();
    for (path, index) in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let junk = parts
            .iter()
            .rev()
            .skip(1)
            .any(|part| part.is_empty() || is_junk_folder(part));
        match parts.as_slice() {
            _ if junk => {}
            [file] if is_toc_name(file) => root.push(((*file).to_string(), *index)),
            [top, file] if is_toc_name(file) => {
                tops.entry(top)
                    .or_default()
                    .push(((*file).to_string(), *index));
            }
            [top, child, file] if is_toc_name(file) => {
                wrapped
                    .entry((top, child))
                    .or_default()
                    .push(((*file).to_string(), *index));
            }
            _ => {}
        }
    }

    let mut found: Vec<(String, usize)> = tops
        .iter()
        .filter_map(|(top, files)| Some(((*top).to_string(), loaded_toc(top, files)?)))
        .collect();
    if let Some(folder) = root_folder(&root)
        && let Some(index) = loaded_toc(&folder, &root)
        && !found
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(&folder))
    {
        found.push((folder, index));
    }
    if !found.is_empty() {
        return found;
    }

    // One folder, with TOCs named for something else: `Repo-main/Foo.toc` can only work as `Foo`.
    if let [(_, files)] = tops.iter().collect::<Vec<_>>().as_slice() {
        let mut bases: Vec<&str> = files.iter().map(|(name, _)| toc_base(name)).collect();
        bases.sort_unstable();
        bases.dedup();
        if let [only] = bases.as_slice()
            && let Some(index) = loaded_toc(only, files)
        {
            return vec![((*only).to_string(), index)];
        }
    }
    // Nothing else explains the zip, so one wrapper around real addon folders.
    let wrappers: BTreeSet<&str> = wrapped.keys().map(|(top, _)| *top).collect();
    if tops.is_empty() && wrappers.len() == 1 {
        return wrapped
            .iter()
            .filter_map(|((_, child), files)| {
                Some(((*child).to_string(), loaded_toc(child, files)?))
            })
            .collect();
    }
    Vec::new()
}

/// The TOC the client would load for `folder`, out of `files`.
fn loaded_toc(folder: &str, files: &[(String, usize)]) -> Option<usize> {
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    let choice = toc::pick_toc(folder, &names)?;
    files
        .iter()
        .find(|(name, _)| name == choice)
        .map(|(_, index)| *index)
}

/// The folder a zip's root-level TOCs belong in. TOCs for Forever decide first, and
/// alphabetical order settles the rest, because that's what the installer does.
fn root_folder(files: &[(String, usize)]) -> Option<String> {
    let stems: Vec<&str> = files.iter().map(|(name, _)| toc_stem(name)).collect();
    TOC_SUFFIXES
        .iter()
        .filter(|suffix| !suffix.is_empty())
        .find_map(|suffix| {
            stems
                .iter()
                .filter_map(|stem| strip_suffix_ignoring_case(stem, suffix))
                .filter(|base| !base.is_empty())
                .min()
        })
        .or_else(|| {
            stems
                .iter()
                .map(|stem| stem_base(stem))
                .filter(|base| !base.is_empty())
                .min()
        })
        .map(str::to_string)
}

fn toc_stem(file: &str) -> &str {
    &file[..file.len() - 4]
}

/// `Foo_Vanilla.toc` and `Foo-Forever.toc` both belong to `Foo`.
fn toc_base(file: &str) -> &str {
    stem_base(toc_stem(file))
}

fn stem_base(stem: &str) -> &str {
    let known = TOC_SUFFIXES
        .iter()
        .filter(|suffix| !suffix.is_empty())
        .find_map(|suffix| strip_suffix_ignoring_case(stem, suffix));
    if let Some(base) = known {
        return base;
    }
    match stem.rfind(['_', '-']) {
        Some(cut)
            if OTHER_GAME_SUFFIXES
                .iter()
                .any(|f| stem[cut + 1..].eq_ignore_ascii_case(f)) =>
        {
            &stem[..cut]
        }
        _ => stem,
    }
}

fn strip_suffix_ignoring_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = text.len().checked_sub(suffix.len())?;
    let tail = text.get(cut..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &text[..cut])
}

fn is_toc_name(file: &str) -> bool {
    !file.starts_with('.')
        && file.len() > 4
        && file
            .get(file.len() - 4..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".toc"))
}

fn read_toc<R: Read + Seek>(archive: &mut ZipArchive<R>, index: usize) -> Option<ForeverToc> {
    let file = archive.by_index(index).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TOC_BYTES).read_to_end(&mut bytes).ok()?;
    Some(parse_toc(&String::from_utf8_lossy(&bytes)))
}

struct Folder {
    name: String,
    toc: Option<ForeverToc>,
}

/// A TOC as the Forever client would see it.
pub struct ForeverToc {
    pub toc: Toc,
    pub targets_forever: bool,
}

fn summarize(mut folders: Vec<Folder>, hints: &[String]) -> ZipScan {
    let forever = folders
        .iter()
        .any(|f| f.toc.as_ref().is_some_and(|t| t.targets_forever));
    // The sort put the best name first. Among equally good names, prefer one that runs on Forever.
    let top = folders.first().map(|f| best_match(&f.name, hints));
    let main = folders
        .iter()
        .position(|f| {
            Some(best_match(&f.name, hints)) == top
                && f.toc.as_ref().is_some_and(|t| t.targets_forever)
        })
        .unwrap_or(0);
    if main != 0 {
        folders.swap(0, main);
    }
    let names = folders.iter().map(|f| f.name.clone()).collect();
    let mut scan = ZipScan {
        folders: names,
        forever,
        ..ZipScan::default()
    };
    if let Some(ForeverToc { toc, .. }) = folders.into_iter().next().and_then(|f| f.toc) {
        scan.title = toc.title.map(bounded_text);
        scan.notes = toc.notes.map(bounded_text);
        // Kept in the public scan cache, so no addresses from here on.
        scan.author = toc.author.as_deref().map(clean_author);
        scan.deps = toc
            .deps
            .into_iter()
            .filter(|dep| dep.len() <= MAX_FOLDER_NAME_BYTES)
            .take(MAX_DEPS)
            .collect();
        scan.category = toc.category.map(bounded_text);
        scan.curse = toc.curse_id.filter(|id| is_short_id(id));
        scan.wowi = toc.wowi_id.filter(|id| is_short_id(id));
        scan.wago = toc.wago_id.filter(|id| is_short_id(id));
    }
    scan
}

/// Cuts free text from a TOC to size, at a character boundary.
fn bounded_text(mut text: String) -> String {
    if text.len() > MAX_TEXT_BYTES {
        let mut end = MAX_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// How well a folder name matches the hints. Higher is better, and ties go to
/// shorter names, then alphabetical order, which the caller's sort keeps stable.
fn best_match(folder: &str, hints: &[String]) -> (u32, std::cmp::Reverse<usize>) {
    let name = normalize(folder);
    let score = hints
        .iter()
        .map(|hint| match_score(&name, hint))
        .max()
        .unwrap_or(0);
    (score, std::cmp::Reverse(name.len()))
}

/// How alike two normalized names are: the same, one starting with the other, or else how
/// long a start they share. Higher is better.
pub(crate) fn match_score(name: &str, hint: &str) -> u32 {
    if name == hint {
        return 2000;
    }
    let shared = name
        .chars()
        .zip(hint.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let shared = u32::try_from(shared).unwrap_or(0);
    if hint.starts_with(name) || name.starts_with(hint) {
        1000 + shared
    } else {
        shared
    }
}

/// Parse TOC text the way Forever would read it, honoring per-line game type
/// conditions such as `## Title: X [AllowLoadGameType camelot]`, which the plain
/// parser would keep as part of the value.
pub fn parse_toc(text: &str) -> ForeverToc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut header = String::with_capacity(text.len().min(4096));
    let mut excluded = false;
    let mut localized_category = None;
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("##") else {
            continue;
        };
        if let Some((key, value)) = rest.split_once(':') {
            let key = key.trim();
            if key.eq_ignore_ascii_case("ExcludeLoadGameType") {
                excluded |= lists_forever(value);
                continue;
            }
            if key.eq_ignore_ascii_case("Category-enUS") && localized_category.is_none() {
                localized_category = Some(value.trim().to_string());
                continue;
            }
        }
        if let Some(applicable) = apply_conditions(line) {
            header.push_str(&applicable);
            header.push('\n');
        }
    }
    let mut toc = toc::parse(&header);
    if toc.category.is_none() {
        toc.category = localized_category.filter(|c| !c.is_empty());
    }
    let targets_forever = toc.targets_forever() && !excluded;
    ForeverToc {
        toc,
        targets_forever,
    }
}

fn lists_forever(value: &str) -> bool {
    value.split(',').any(|item| {
        let item = item.trim();
        ["camelot", "forever", "mainline"]
            .iter()
            .any(|t| item.eq_ignore_ascii_case(t))
    })
}

/// Returns the line without its game type conditions, or `None` when they rule Forever out.
fn apply_conditions(line: &str) -> Option<String> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(len) = rest[open..].find(']') else {
            break;
        };
        let inner = &rest[open + 1..open + len];
        let (word, values) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        out.push_str(&rest[..open]);
        if word.eq_ignore_ascii_case("AllowLoadGameType") {
            if !lists_forever(values) {
                return None;
            }
        } else if word.eq_ignore_ascii_case("ExcludeLoadGameType") {
            if lists_forever(values) {
                return None;
            }
        } else {
            out.push_str(&rest[open..=open + len]);
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn scan(files: &[(&str, &str)], hints: &[&str]) -> ZipScan {
        scan_zip(Cursor::new(make_zip(files)), hints).unwrap()
    }

    #[test]
    fn questie_trap_forever_toc_wins_over_a_placeholder() {
        let scan = scan(
            &[
                (
                    "Questie/Questie.toc",
                    "## Interface: 00000\n## Title: Questie Plain\n",
                ),
                (
                    "Questie/Questie_Forever.toc",
                    "## Interface: 16001\n## Title: Questie\n## Notes: Quest helper\n## Author: Aero\n",
                ),
                (
                    "Questie/Questie_Vanilla.toc",
                    "## Interface: 11508\n## Title: Questie Vanilla\n",
                ),
                (
                    "QuestieDB/QuestieDB_Forever.toc",
                    "## Interface: 16001\n## Title: QuestieDB\n",
                ),
            ],
            &["Questie", "Questie-v12.6.2+v1.0.11.zip"],
        );
        assert_eq!(scan.folders, ["Questie", "QuestieDB"]);
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Questie"));
        assert_eq!(scan.notes.as_deref(), Some("Quest helper"));
        assert_eq!(scan.author.as_deref(), Some("Aero"));
    }

    #[test]
    fn a_bom_before_the_header_is_fine() {
        let scan = scan(
            &[(
                "Pawn/Pawn.toc",
                "\u{feff}## Interface: 120105, 16001\r\n## Title: Pawn\r\n",
            )],
            &["Pawn"],
        );
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Pawn"));
    }

    #[test]
    fn partly_forever_zips_list_every_folder_and_count_as_forever() {
        let scan = scan(
            &[
                (
                    "DBM-Core/DBM-Core.toc",
                    "## Interface: 120105, 16001\n## Title: DBM Core\n",
                ),
                (
                    "DBM-Raids-Midnight/DBM-Raids-Midnight.toc",
                    "## Interface: 120105\n",
                ),
                ("DBM-Forever/DBM-Forever.toc", "## Interface: 16001\n"),
            ],
            &["DeadlyBossMods", "DBM-Core-12.0.20.zip"],
        );
        assert_eq!(scan.folders[0], "DBM-Core");
        assert_eq!(scan.folders.len(), 3);
        assert!(scan.forever);
    }

    #[test]
    fn retail_only_and_restricted_zips_are_not_forever() {
        let retail = scan(&[("Foo/Foo.toc", "## Interface: 120105\n")], &["Foo"]);
        assert_eq!(retail.folders, ["Foo"]);
        assert!(!retail.forever);

        let restricted = scan(
            &[(
                "Foo/Foo.toc",
                "## Interface: 120105, 16001\n## AllowLoadGameType: standard\n",
            )],
            &["Foo"],
        );
        assert!(!restricted.forever);

        let excluded = scan(
            &[(
                "Foo/Foo.toc",
                "## Interface: 120105, 16001\n## ExcludeLoadGameType: camelot\n",
            )],
            &["Foo"],
        );
        assert!(!excluded.forever);
    }

    #[test]
    fn folders_the_game_wouldnt_load_are_listed_because_they_get_installed_but_never_lead() {
        let scan = scan(
            &[
                ("__MACOSX/Foo/._Foo.toc", "junk"),
                ("__MACOSX/__MACOSX.toc", "## Interface: 16001\n"),
                ("images/images.toc", "## Interface: 16001\n"),
                ("OpenMe/readme.txt", "hello"),
                ("Foo/Foo.toc", "## Interface: 16001\n## Title: Foo\n"),
                ("Foo/sub/Sub.toc", "## Interface: 16001\n"),
                ("Bar/Baz.toc", "## Interface: 16001\n"),
                ("Vanilla/Vanilla_Vanilla.toc", "## Interface: 11508\n"),
            ],
            &["Bar", "images"],
        );
        assert_eq!(scan.folders[0], "Foo", "the only folder the client loads");
        assert_eq!(scan.title.as_deref(), Some("Foo"));
        let mut rest = scan.folders[1..].to_vec();
        rest.sort();
        assert_eq!(rest, ["Bar", "Vanilla", "images"]);
    }

    /// What the real installer writes for this zip.
    fn installed_folders(bytes: &[u8]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("addon.zip");
        std::fs::write(&path, bytes).unwrap();
        let dest = dir.path().join("out");
        feeling_core::extract::extract(
            &path,
            &dest,
            &[],
            feeling_core::extract::Limits::default(),
            &mut |_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )
        .map(|extracted| extracted.folders)
        .unwrap_or_default()
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[test]
    fn the_scan_lists_exactly_what_the_installer_writes() {
        const TOC: &str = "## Interface: 16001\n## Title: Harmless\n";
        let cases: &[&[(&str, &str)]] = &[
            // Names the installer cleans up, which a plain reading misses.
            &[
                ("Harmless/Harmless.toc", TOC),
                ("./DBM-Core/DBM-Core.toc", TOC),
            ],
            &[
                ("Harmless/Harmless.toc", TOC),
                ("DBM-Core//DBM-Core.toc", TOC),
            ],
            &[
                ("Harmless/Harmless.toc", TOC),
                ("DBM-Core/./DBM-Core.toc", TOC),
            ],
            &[
                ("Harmless/Harmless.toc", TOC),
                ("DBM-Core\\DBM-Core.toc", TOC),
            ],
            // A TOC not named after its folder: the game can't load it, the folder still lands.
            &[
                ("Harmless/Harmless.toc", TOC),
                ("DBM-Core/whatever.toc", TOC),
            ],
            &[
                ("Harmless/Harmless.toc", TOC),
                ("DBM-Core/DBM-Core_Vanilla.toc", TOC),
            ],
            // Layouts both sides have always known.
            &[("Harmless/Harmless.toc", TOC), ("Harmless/core.lua", "x")],
            &[("Harmless.toc", TOC), ("core.lua", "x")],
            &[
                ("Harmless-1.2.3/Harmless.toc", TOC),
                ("Harmless-1.2.3/core.lua", "x"),
            ],
            &[
                ("Wrap/Harmless/Harmless.toc", TOC),
                ("Wrap/Other/Other.toc", TOC),
            ],
            &[
                ("Main/Main.toc", TOC),
                ("Extras/Optional/Optional.toc", TOC),
            ],
            &[("Main/Main.toc", TOC), ("Root.toc", TOC), ("core.lua", "x")],
            // Leftovers the installer skips.
            &[
                ("Harmless/Harmless.toc", TOC),
                ("__MACOSX/__MACOSX.toc", TOC),
            ],
            &[("Harmless/Harmless.toc", TOC), (".git/.git.toc", TOC)],
            &[("Harmless/Harmless.toc", TOC), ("images/images.toc", TOC)],
            &[("Harmless/Harmless.toc", TOC), ("docs/readme.txt", "x")],
            // Things it refuses outright, or finds nothing in.
            &[("Harmless/Harmless.toc", TOC), ("../Evil/Evil.toc", TOC)],
            &[("Harmless/Harmless.toc", TOC), ("/Evil/Evil.toc", TOC)],
            &[("Harmless/Harmless.toc", TOC), ("Evil:x/Evil.toc", TOC)],
            &[("Harmless/Harmless.toc", TOC), ("harmless/Other.toc", TOC)],
            &[("readme.txt", "x")],
        ];
        for files in cases {
            let bytes = make_zip(files);
            let scanned = scan_zip(Cursor::new(&bytes), &["Harmless"]).unwrap();
            assert_eq!(
                sorted(scanned.folders),
                installed_folders(&bytes),
                "{files:?}"
            );
        }
    }

    #[test]
    fn a_hidden_folder_shows_up_beside_the_one_the_addon_is_named_for() {
        for hidden in [
            "./DBM-Core/DBM-Core.toc",
            "DBM-Core//DBM-Core.toc",
            "DBM-Core/whatever.toc",
        ] {
            let scan = scan(
                &[
                    (
                        "Harmless/Harmless.toc",
                        "## Interface: 16001\n## Title: Harmless\n",
                    ),
                    (hidden, "## Interface: 16001\n"),
                ],
                &["Harmless"],
            );
            assert_eq!(scan.folders[0], "Harmless", "{hidden}");
            assert!(
                scan.folders.iter().any(|f| f == "DBM-Core"),
                "{hidden}: {scan:?}"
            );
        }
    }

    /// One file with two directories: the EOCD's offset points at the first, and its size
    /// makes the second look like the one sitting right before it.
    fn two_directories(first: &[u8], second: &[u8]) -> Vec<u8> {
        const EOCD_LEN: usize = 22;
        let field = |bytes: &[u8], at: usize| {
            usize::try_from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())).unwrap()
        };
        let eocd = |bytes: &[u8]| bytes.len() - EOCD_LEN;
        let cd_offset = |bytes: &[u8]| field(bytes, eocd(bytes) + 16);
        let cd_size = |bytes: &[u8]| field(bytes, eocd(bytes) + 12);
        assert!(cd_offset(first) >= cd_offset(second), "pad the first zip");

        // Everything from the first zip but its end record, then the second zip's entry.
        let mut out = first[..eocd(first)].to_vec();
        let local_at = out.len();
        out.extend_from_slice(&second[..eocd(second)]);
        let directory_at = local_at + cd_offset(second);

        // A reader that trusts the size adds this much to every offset in the directory.
        let shift = directory_at - cd_offset(first);
        let local_field = directory_at + 42;
        let adjusted = u32::try_from(local_at - shift).unwrap();
        out[local_field..local_field + 4].copy_from_slice(&adjusted.to_le_bytes());

        let mut end = first[eocd(first)..].to_vec();
        end[12..16].copy_from_slice(&u32::try_from(cd_size(second)).unwrap().to_le_bytes());
        out.extend_from_slice(&end);
        out
    }

    #[test]
    fn a_zip_with_two_directories_is_refused_rather_than_read_one_way() {
        let padding = "x".repeat(4096);
        let first_toc = format!("## Interface: 16001\n## Title: Harmless\n## Notes: {padding}\n");
        let first = make_zip(&[("Harmless/Harmless.toc", first_toc.as_str())]);
        let second = make_zip(&[("DBM-Core/DBM-Core.toc", "## Interface: 16001\n")]);
        let both = two_directories(&first, &second);

        // Readers really do disagree about this file, which is the trick being closed.
        let strict = ZipArchive::with_config(
            Config {
                archive_offset: ArchiveOffset::Known(0),
            },
            Cursor::new(&both),
        )
        .unwrap();
        assert_eq!(
            strict.name_for_index(0).unwrap().unwrap(),
            "Harmless/Harmless.toc"
        );
        let detecting = ZipArchive::new(Cursor::new(&both)).unwrap();
        assert_ne!(detecting.offset(), 0);
        assert_eq!(
            detecting.name_for_index(0).unwrap().unwrap(),
            "DBM-Core/DBM-Core.toc"
        );

        let scan = scan_zip(Cursor::new(&both), &["Harmless"]).unwrap();
        assert!(scan.folders.is_empty(), "{scan:?}");
        assert!(!scan.forever);
    }

    fn folders_zip(count: usize) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for at in 0..count {
            writer
                .start_file(format!("Pack{at}/Pack{at}.toc"), options)
                .unwrap();
            writer.write_all(b"## Interface: 16001\n").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn a_zip_with_too_many_folders_is_an_empty_scan_not_a_failure() {
        let allowed = scan_zip(Cursor::new(folders_zip(MAX_FOLDERS)), &["Pack0"]).unwrap();
        assert_eq!(allowed.folders.len(), MAX_FOLDERS);
        assert!(allowed.forever);

        // Empty rather than invalid: an error would never be remembered, and the whole
        // zip would be downloaded again on every run.
        let too_many = scan_zip(Cursor::new(folders_zip(MAX_FOLDERS + 1)), &["Pack0"]).unwrap();
        assert_eq!(too_many, ZipScan::default());
    }

    #[test]
    fn an_absurdly_long_folder_name_is_an_empty_scan() {
        let long = "x".repeat(MAX_FOLDER_NAME_BYTES + 1);
        let path = format!("{long}/{long}.toc");
        let bytes = make_zip(&[(path.as_str(), "## Interface: 16001\n")]);
        assert_eq!(
            scan_zip(Cursor::new(bytes), &["x"]).unwrap(),
            ZipScan::default()
        );
    }

    #[test]
    fn text_from_a_toc_is_kept_short_before_it_reaches_the_scan_cache() {
        let long = "é".repeat(MAX_TEXT_BYTES);
        let deps: Vec<String> = (0..MAX_DEPS + 40).map(|i| format!("Lib{i}")).collect();
        let toc = format!(
            "## Interface: 16001\n## Title: {long}\n## Notes: {long}\n## X-Category: {long}\n\
             ## Author: {long}\n## Dependencies: {}, {}\n## X-WoWI-ID: 1{zeros}\n\
             ## X-Curse-Project-ID: 2{zeros}\n## X-Wago-ID: a{zeros}\n",
            "x".repeat(MAX_FOLDER_NAME_BYTES + 1),
            deps.join(", "),
            zeros = "0".repeat(40),
        );
        let scan = scan(&[("Foo/Foo.toc", toc.as_str())], &["Foo"]);
        for text in [&scan.title, &scan.notes, &scan.category, &scan.author] {
            assert!(text.as_ref().unwrap().len() <= MAX_TEXT_BYTES, "{text:?}");
        }
        assert_eq!(scan.deps.len(), MAX_DEPS);
        assert_eq!(
            scan.deps[0], "Lib0",
            "the name too long to be a folder is gone"
        );
        assert_eq!((scan.wowi, scan.curse, scan.wago), (None, None, None));
    }

    #[test]
    fn real_sized_toc_fields_pass_the_bounds_untouched() {
        let scan = scan(
            &[(
                "Foo/Foo.toc",
                "## Interface: 16001\n## Title: Foo\n## Dependencies: Ace3, !BugGrabber\n\
                 ## X-WoWI-ID: 5086\n## X-Curse-Project-ID: 1725334\n## X-Wago-ID: kGr9ReKy\n",
            )],
            &["Foo"],
        );
        assert_eq!(scan.title.as_deref(), Some("Foo"));
        assert_eq!(scan.deps, ["!BugGrabber", "Ace3"]);
        assert_eq!(scan.wowi.as_deref(), Some("5086"));
        assert_eq!(scan.curse.as_deref(), Some("1725334"));
        assert_eq!(scan.wago.as_deref(), Some("kGr9ReKy"));
    }

    fn names_for(paths: &[&str]) -> Vec<String> {
        let paths: Vec<(String, usize)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| ((*p).to_string(), i))
            .collect();
        addon_folders(&paths)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn a_wrapper_around_addon_folders_is_looked_through() {
        let scan = scan(
            &[
                ("Foo-1.0/README.md", "hi"),
                (
                    "Foo-1.0/Foo/Foo.toc",
                    "## Interface: 16001\n## Title: Foo\n",
                ),
                (
                    "Foo-1.0/Foo_Options/Foo_Options.toc",
                    "## Interface: 16001\n",
                ),
                ("Foo-1.0/tools/readme.txt", "hi"),
            ],
            &["Foo"],
        );
        assert_eq!(scan.folders, ["Foo", "Foo_Options"]);
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Foo"));
    }

    #[test]
    fn two_wrappers_are_not_a_layout_anyone_can_install() {
        assert!(names_for(&["W1/A/A.toc", "W2/B/B.toc"]).is_empty());
    }

    #[test]
    fn a_real_addon_folder_beside_a_lookalike_wrapper_wins() {
        assert_eq!(
            names_for(&["Main/Main.toc", "Extras/Optional/Optional.toc"]),
            ["Main"]
        );
    }

    #[test]
    fn a_toc_at_the_root_names_the_folder() {
        for (toc, folder) in [
            ("MyAddon.toc", "MyAddon"),
            ("MyAddon_Forever.toc", "MyAddon"),
            ("MyAddon-Forever.toc", "MyAddon"),
            ("MyAddon_Camelot.toc", "MyAddon"),
            ("MyAddon-camelot.TOC", "MyAddon"),
            ("MyAddon_Mainline.toc", "MyAddon"),
            ("My_Addon.toc", "My_Addon"),
        ] {
            assert_eq!(
                names_for(&[toc, "core.lua", "Libs/x.lua"]),
                [folder],
                "{toc}"
            );
        }
        // A real zip: SimpleDatatexts is zipped by hand from inside its folder.
        let scan = scan(
            &[
                ("SimpleDatatexts.toc", "## Interface: 120105\n"),
                (
                    "SimpleDatatexts_Forever.toc",
                    "## Interface: 16001\n## Title: Simple\n",
                ),
                ("Core.lua", ""),
                (
                    ".release/SimpleDatatexts/SimpleDatatexts.toc",
                    "## Interface: 120105\n",
                ),
            ],
            &["SimpleDatatexts"],
        );
        assert_eq!(scan.folders, ["SimpleDatatexts"]);
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Simple"));
    }

    #[test]
    fn root_tocs_that_only_fit_another_game_load_nothing() {
        assert!(names_for(&["MyAddon_Vanilla.toc"]).is_empty());
        assert!(
            names_for(&["._MyAddon.toc"]).is_empty(),
            "hidden macOS files aren't TOCs"
        );
        assert_eq!(
            names_for(&[
                "Foo.toc",
                "Foo_Forever.toc",
                "Foo_Vanilla.toc",
                "Other_Forever.toc"
            ]),
            ["Foo"],
            "several TOCs at the root: the same pick as the installer's"
        );
    }

    #[test]
    fn a_folder_with_a_version_in_its_name_is_listed_as_it_installs() {
        assert_eq!(
            names_for(&["Foo-1.2.3/Foo.toc", "Foo-1.2.3/core.lua"]),
            ["Foo"]
        );
        assert_eq!(
            names_for(&[
                "Foo-main/Foo.toc",
                "Foo-main/Foo_Forever.toc",
                "Foo-main/Foo_Vanilla.toc"
            ]),
            ["Foo"]
        );
        assert!(
            names_for(&["Mix/One.toc", "Mix/Two.toc"]).is_empty(),
            "no way to tell which"
        );
        assert!(
            names_for(&["Foo/Foo_Vanilla.toc"]).is_empty(),
            "the name fits, but not for this game"
        );
    }

    #[test]
    fn leading_bang_folders_are_kept() {
        let scan = scan(
            &[(
                "!BugGrabber/!BugGrabber.toc",
                "## Interface: 16001\n## Title: !BugGrabber\n",
            )],
            &["BugGrabber"],
        );
        assert_eq!(scan.folders, ["!BugGrabber"]);
        assert!(scan.forever);
    }

    #[test]
    fn backslash_paths_from_windows_zippers_are_understood() {
        let scan = scan(
            &[("Foo\\Foo.toc", "## Interface: 16001\n## Title: Foo\n")],
            &["Foo"],
        );
        assert_eq!(scan.folders, ["Foo"]);
        assert_eq!(scan.title.as_deref(), Some("Foo"));
    }

    #[test]
    fn deps_and_ids_come_from_the_main_toc() {
        let scan = scan(
            &[
                (
                    "Foo/Foo.toc",
                    "## Interface: 16001\n## Dependencies: Foo_Lib, Ace3\n## X-WoWI-ID: 42\n## X-Curse-Project-ID: 7\n## X-Wago-ID: abc\n## X-Category: Combat\n",
                ),
                (
                    "Foo_Lib/Foo_Lib.toc",
                    "## Interface: 16001\n## X-WoWI-ID: 99\n",
                ),
            ],
            &["Foo"],
        );
        assert_eq!(scan.wowi.as_deref(), Some("42"));
        assert_eq!(scan.curse.as_deref(), Some("7"));
        assert_eq!(scan.wago.as_deref(), Some("abc"));
        assert_eq!(scan.category.as_deref(), Some("Combat"));
        assert_eq!(
            scan.external_deps(),
            ["Ace3"],
            "a dep shipped in the same zip isn't external"
        );
    }

    #[test]
    fn main_folder_prefers_the_closest_name() {
        let main = |folders: &[&str], hints: &[&str]| -> String {
            let files: Vec<(String, String)> = folders
                .iter()
                .map(|f| (format!("{f}/{f}.toc"), "## Interface: 16001\n".to_string()))
                .collect();
            let refs: Vec<(&str, &str)> = files
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            scan(&refs, hints).folders[0].clone()
        };
        assert_eq!(
            main(&["Titan", "TitanBag", "TitanXP"], &["Titan Panel"]),
            "Titan"
        );
        assert_eq!(
            main(&["Prat-3.0", "Prat-3.0_Libraries"], &["prat-3-0", "Prat"]),
            "Prat-3.0"
        );
        assert_eq!(
            main(
                &["ShortestPath", "ShortestPathData"],
                &["shortest-path-forever"]
            ),
            "ShortestPath"
        );
        assert_eq!(
            main(&["Alpha", "Beta"], &["unrelated"]),
            "Beta",
            "shortest wins without a match"
        );
    }

    #[test]
    fn a_best_name_that_skips_forever_loses_only_to_an_equal_match() {
        let scan = scan(
            &[
                ("Foo/Foo.toc", "## Interface: 120105\n## Title: Retail\n"),
                (
                    "foo_/foo__Forever.toc",
                    "## Interface: 16001\n## Title: Forever\n",
                ),
            ],
            &["Foo"],
        );
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Forever"), "{scan:?}");
    }

    #[test]
    fn inline_game_type_conditions_are_honored() {
        let parsed = parse_toc(
            "## Interface: 11509, 16001, 120105\n\
             ## Title: UI Mists [AllowLoadGameType mists]\n\
             ## Title: UI Forever [AllowLoadGameType camelot][ExcludeLoadGameType standard, classic]\n\
             ## Title: UI Retail [AllowLoadGameType standard]\n\
             ## Title: UI Classic [AllowLoadGameType vanilla]\n\
             ## Notes: Not here [ExcludeLoadGameType camelot]\n\
             ## Dependencies: [Family:Mainline] Ace3\n",
        );
        assert_eq!(parsed.toc.title.as_deref(), Some("UI Forever"));
        assert_eq!(parsed.toc.notes, None);
        assert_eq!(parsed.toc.deps, ["Ace3"]);
        assert!(parsed.targets_forever);
    }

    #[test]
    fn localized_category_is_a_fallback() {
        let parsed = parse_toc(
            "## Interface: 16001\n## Category-enUS: UI Overhaul\n## Category-deDE: Foo\n",
        );
        assert_eq!(parsed.toc.category.as_deref(), Some("UI Overhaul"));
        let parsed =
            parse_toc("## Interface: 16001\n## Category: Combat\n## Category-enUS: UI Overhaul\n");
        assert_eq!(parsed.toc.category.as_deref(), Some("Combat"));
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        let invalid = |bytes: Vec<u8>| {
            matches!(
                scan_zip(Cursor::new(bytes), &["x"]),
                Err(ScanError::Invalid(_))
            )
        };
        assert!(invalid(b"<html>not a zip</html>".to_vec()));
        assert!(invalid(Vec::new()));
        let mut zip = make_zip(&[("Foo/Foo.toc", "## Interface: 16001\n")]);
        zip.truncate(zip.len() / 2);
        assert!(invalid(zip));
    }

    #[test]
    fn read_failures_are_told_apart_from_bad_zips() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("HTTP 503"))
            }
        }
        impl Seek for Broken {
            fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
                Ok(1000)
            }
        }
        assert!(matches!(scan_zip(Broken, &["x"]), Err(ScanError::Io(_))));
    }

    #[test]
    fn an_oversized_toc_is_cut_off_rather_than_read_whole() {
        let padding = format!(
            "## Interface: 16001\n{}\n## Title: Too Late\n",
            "x".repeat(1024 * 1024)
        );
        let scan = scan(&[("Foo/Foo.toc", padding.as_str())], &["Foo"]);
        assert!(scan.forever);
        assert_eq!(scan.title, None);
    }

    #[test]
    fn non_utf8_toc_text_is_tolerated() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("Foo/Foo.toc", SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(b"## Interface: 16001\n## Title: Caf\xe9\n")
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let scan = scan_zip(Cursor::new(bytes), &["Foo"]).unwrap();
        assert!(scan.forever);
        assert!(scan.title.unwrap().starts_with("Caf"));
    }

    #[test]
    fn an_empty_zip_is_a_scan_with_nothing_in_it() {
        let writer = ZipWriter::new(Cursor::new(Vec::new()));
        let bytes = writer.finish().unwrap().into_inner();
        let scan = scan_zip(Cursor::new(bytes), &["Foo"]).unwrap();
        assert!(scan.folders.is_empty());
    }
}
