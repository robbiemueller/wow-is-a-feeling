//! Reads what we need from an addon zip: which top-level folders have a TOC the
//! client would load, whether any of them targets Forever, and the main folder's
//! details. Nothing is extracted. Works over anything that reads and seeks, so
//! remote range readers and in-memory downloads share this code.

use std::collections::BTreeMap;
use std::io::{Read, Seek};

use feeling_core::toc::{self, Toc};
use zip::ZipArchive;
use zip::read::{ArchiveOffset, Config};
use zip::result::ZipError;

use crate::model::ZipScan;
use crate::text::normalize;

/// No real TOC comes close to this, and it stops a hostile one from filling memory.
const MAX_TOC_BYTES: u64 = 256 * 1024;
/// Huge packs exist, and each TOC can cost a request. The main folder is read
/// first, so this only limits how sure we are about the long tail of a pack.
const MAX_FOLDERS_READ: usize = 64;

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
/// all: a zip with no usable folders is a successful scan with an empty list.
pub fn scan_zip<R: Read + Seek>(reader: R, hints: &[&str]) -> Result<ZipScan, ScanError> {
    // Known(0) stops the zip crate from scanning the whole file for a prefix,
    // which over HTTP would mean downloading it. Nobody prepends data to addons.
    let config = Config {
        archive_offset: ArchiveOffset::Known(0),
    };
    let mut archive = ZipArchive::with_config(config, reader).map_err(|e| match e {
        ZipError::Io(io) => ScanError::Io(io.to_string()),
        other => ScanError::Invalid(other.to_string()),
    })?;

    let mut tocs_by_folder: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for index in 0..archive.len() {
        let Some(Ok(raw)) = archive.name_for_index(index) else {
            continue;
        };
        let name = raw.replace('\\', "/");
        let mut parts = name.split('/');
        let (Some(folder), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if folder.is_empty() || is_junk_folder(folder) || !is_toc_name(file) {
            continue;
        }
        tocs_by_folder
            .entry(folder.to_string())
            .or_default()
            .push((file.to_string(), index));
    }

    let mut picked: Vec<(String, usize)> = Vec::new();
    for (folder, files) in &tocs_by_folder {
        let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
        if let Some(choice) = toc::pick_toc(folder, &names) {
            let index = files
                .iter()
                .find(|(name, _)| name == choice)
                .map_or(0, |(_, index)| *index);
            picked.push((folder.clone(), index));
        }
    }

    let hints: Vec<String> = hints
        .iter()
        .map(|h| normalize(h))
        .filter(|h| !h.is_empty())
        .collect();
    picked.sort_by_key(|(folder, _)| std::cmp::Reverse(best_match(folder, &hints)));

    let mut folders = Vec::with_capacity(picked.len());
    for (position, (folder, index)) in picked.iter().enumerate() {
        let parsed = (position < MAX_FOLDERS_READ)
            .then(|| read_toc(&mut archive, *index))
            .flatten();
        folders.push(Folder {
            name: folder.clone(),
            toc: parsed,
        });
    }
    Ok(summarize(folders, &hints))
}

fn is_toc_name(file: &str) -> bool {
    file.len() > 4
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
        scan.title = toc.title;
        scan.notes = toc.notes;
        scan.author = toc.author;
        scan.deps = toc.deps;
        scan.category = toc.category;
        scan.curse = toc.curse_id;
        scan.wowi = toc.wowi_id;
        scan.wago = toc.wago_id;
    }
    scan
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

fn match_score(name: &str, hint: &str) -> u32 {
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
    fn junk_folders_and_folders_without_a_loadable_toc_do_not_count() {
        let scan = scan(
            &[
                ("__MACOSX/Foo/._Foo.toc", "junk"),
                ("__MACOSX/__MACOSX.toc", "## Interface: 16001\n"),
                ("images/images.toc", "## Interface: 16001\n"),
                ("OpenMe/readme.txt", "hello"),
                ("Foo/Foo.toc", "## Interface: 16001\n"),
                ("Foo/sub/Sub.toc", "## Interface: 16001\n"),
                ("Bar/Baz.toc", "## Interface: 16001\n"),
                ("Vanilla/Vanilla_Vanilla.toc", "## Interface: 11508\n"),
                ("loose.toc", "## Interface: 16001\n"),
            ],
            &["Foo"],
        );
        assert_eq!(scan.folders, ["Foo"]);
    }

    #[test]
    fn a_wrapper_folder_is_not_an_addon() {
        let scan = scan(
            &[("Foo-1.0/Foo/Foo.toc", "## Interface: 16001\n")],
            &["Foo"],
        );
        assert!(scan.folders.is_empty());
        assert!(!scan.forever);
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
