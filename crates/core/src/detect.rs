//! Finding World of Warcraft: Forever on this machine.
//!
//! Folder names prove nothing: `_retail_`, `_classic_` and even `_forever_` can be
//! leftovers with no game in them. An install only counts when `.flavor.info` says
//! `wowf` and a game binary sits next to it. Linux runs the game in Wine prefixes,
//! where every path segment can be spelled in any case.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const PRODUCT: &str = "wowf";
const FOREVER_FOLDER: &str = "_forever_";
const WOW_FOLDER: &str = "World of Warcraft";
/// The game's Windows binaries, which Wine runs too. Compared without regard to case.
pub const GAME_EXES: [&str; 2] = ["Wow.exe", "Wow-ARM64.exe"];
const GAME_BUNDLE: &str = "World of Warcraft.app";
const MAX_INFO_BYTES: u64 = 64 * 1024;
const MAX_PRODUCT_DB_BYTES: u64 = 16 * 1024 * 1024;
/// How many text runs after `wowf` may hold its path. The record is `uid`, then
/// `product code`, then the install path, so the real distance is one or two.
const PATH_LOOKAHEAD: usize = 4;
/// Printable bytes a following protobuf field can glue onto the end of a path.
const MAX_STUCK_BYTES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundBy {
    BattleNet,
    Registry,
    CommonPath,
    /// Inside a Wine prefix. Names the launcher: Wine, Lutris, Bottles, Steam or Faugus.
    WinePrefix(String),
    Manual,
}

/// A validated Forever install. Paths use the spelling found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// The World of Warcraft folder, which holds `.build.info` and `_forever_`.
    pub root: PathBuf,
    pub forever_dir: PathBuf,
    /// `_forever_/Interface/AddOns`. May not exist yet: see [`ensure_addons_dir`].
    pub addons_dir: PathBuf,
    pub wtf_dir: PathBuf,
    pub version: Option<String>,
    pub found_by: FoundBy,
}

impl Install {
    /// Stable identity for this install, used as the key in `State::installs`.
    /// Two spellings of the same folder give the same key.
    pub fn key(&self) -> String {
        path_key(&self.forever_dir)
    }
}

fn path_key(path: &Path) -> String {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = real.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if cfg!(any(windows, target_os = "macos")) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

/// Check a folder the player picked or a detector suggested. Accepts the World of
/// Warcraft folder, `_forever_`, or `_forever_/Interface/AddOns`. The error says
/// what's wrong and what to do, ready to show.
pub fn validate(path: &Path) -> Result<Install> {
    validate_as(path, FoundBy::Manual)
}

fn validate_as(path: &Path, found_by: FoundBy) -> Result<Install> {
    // Tidies separators, so product.db's `C:/Games` becomes `C:\Games` on Windows.
    let path: PathBuf = path.components().collect();
    let root = root_from(&path);
    let Some(forever_dir) = find_child_ci(root, FOREVER_FOLDER) else {
        return Err(Error::Other(format!(
            "{} has no {FOREVER_FOLDER} folder, so WoW Forever isn't installed there. \
             Pick the World of Warcraft folder that holds {FOREVER_FOLDER}.",
            path.display()
        )));
    };
    if !flavor_is_forever(&forever_dir) {
        return Err(Error::Other(format!(
            "{} isn't marked as WoW Forever: its .flavor.info is missing or names another \
             game. Install or repair WoW Forever in Battle.net, then try again.",
            forever_dir.display()
        )));
    }
    if !has_game_binary(&forever_dir) {
        return Err(Error::Other(format!(
            "{} has no game in it, only leftover files. Install or repair WoW Forever in \
             Battle.net, then try again.",
            forever_dir.display()
        )));
    }
    let interface = child_or_default(&forever_dir, "Interface");
    Ok(Install {
        root: root.to_path_buf(),
        addons_dir: child_or_default(&interface, "AddOns"),
        wtf_dir: child_or_default(&forever_dir, "WTF"),
        version: read_version(root),
        forever_dir,
        found_by,
    })
}

/// Walk up from `_forever_/Interface/AddOns`, `_forever_` or any other flavor folder
/// to the World of Warcraft folder. A folder that holds `_forever_` is already there,
/// whatever it's called.
fn root_from(path: &Path) -> &Path {
    if find_child_ci(path, FOREVER_FOLDER).is_some() {
        return path;
    }
    let mut current = path;
    if name_is(current, "AddOns")
        && let Some(parent) = current.parent()
        && name_is(parent, "Interface")
    {
        current = parent;
    }
    if name_is(current, "Interface")
        && let Some(parent) = current.parent()
    {
        current = parent;
    }
    strip_flavor_folder(current)
}

fn name_is(path: &Path, name: &str) -> bool {
    path.file_name()
        .is_some_and(|n| same_name(&n.to_string_lossy(), name))
}

/// Registry hints point inside a flavor folder such as `_retail_`.
fn strip_flavor_folder(path: &Path) -> &Path {
    let is_flavor = path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.len() > 2 && name.starts_with('_') && name.ends_with('_')
    });
    match path.parent() {
        Some(parent) if is_flavor => parent,
        _ => path,
    }
}

fn flavor_is_forever(forever_dir: &Path) -> bool {
    find_child_ci(forever_dir, ".flavor.info")
        .and_then(|file| read_text(&file))
        .is_some_and(|text| {
            parse_flavor_info(&text)
                .iter()
                .any(|flavor| flavor.eq_ignore_ascii_case(PRODUCT))
        })
}

fn has_game_binary(forever_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(forever_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `is_dir` follows links, and is only asked for names that could match.
        (GAME_EXES.iter().any(|exe| same_name(&name, exe)) || same_name(&name, GAME_BUNDLE))
            && is_game_binary(&name, entry.path().is_dir())
    })
}

/// True for `Wow.exe` or `Wow-ARM64.exe` as files, and `World of Warcraft.app` as a bundle folder.
pub fn is_game_binary(name: &str, is_dir: bool) -> bool {
    if is_dir {
        same_name(name, GAME_BUNDLE)
    } else {
        GAME_EXES.iter().any(|exe| same_name(name, exe))
    }
}

fn read_version(root: &Path) -> Option<String> {
    let text = read_text(&find_child_ci(root, ".build.info")?)?;
    parse_build_info(&text)
}

fn read_capped(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = read_capped(path, MAX_INFO_BYTES)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Lines of a Blizzard info file, without its BOM, blanks and `## seqn` comments.
fn info_lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Which column of a `Name!TYPE:n|Name!TYPE:n` header is called `name`.
fn info_column(header: &[&str], name: &str) -> Option<usize> {
    header.iter().position(|cell| {
        cell.split('!')
            .next()
            .is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
    })
}

/// The game version of the `wowf` row in `.build.info`. When several rows match,
/// one marked active wins.
pub fn parse_build_info(text: &str) -> Option<String> {
    let mut lines = info_lines(text);
    let header: Vec<&str> = lines.next()?.split('|').collect();
    let product = info_column(&header, "Product")?;
    let version = info_column(&header, "Version")?;
    let active = info_column(&header, "Active");

    let mut inactive_match = None;
    for line in lines {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if !cells
            .get(product)
            .is_some_and(|p| p.eq_ignore_ascii_case(PRODUCT))
        {
            continue;
        }
        let Some(found) = cells.get(version).filter(|v| !v.is_empty()) else {
            continue;
        };
        if active.and_then(|a| cells.get(a)) == Some(&"1") {
            return Some((*found).to_string());
        }
        inactive_match.get_or_insert(*found);
    }
    inactive_match.map(str::to_string)
}

/// The product codes in `.flavor.info`, whose whole body on a real install is
/// `Product Flavor!STRING:0` then `wowf`.
pub fn parse_flavor_info(text: &str) -> Vec<String> {
    let mut lines = info_lines(text).peekable();
    let column = match lines.next_if(|first| first.contains('!')) {
        Some(header) => {
            let header: Vec<&str> = header.split('|').collect();
            info_column(&header, "Product Flavor").unwrap_or(0)
        }
        None => 0,
    };
    lines
        .filter_map(|row| row.split('|').nth(column))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// The install paths Battle.net recorded for `wowf`, as written in `product.db`
/// (usually with forward slashes).
///
/// The file is a protobuf, but the strings in it are plain, so this scans for text
/// instead of tying itself to a message layout Blizzard can change. A `wowf` run
/// that stands alone is a product id. The file's later mentions glue to neighboring
/// bytes (`/install/wowf`, `wowf:`), and those sit next to Battle.net's own paths.
pub fn paths_from_product_db(bytes: &[u8]) -> Vec<String> {
    let runs = text_runs(bytes);
    let mut paths: Vec<String> = Vec::new();
    for (i, (_, run)) in runs.iter().enumerate() {
        if !run.eq_ignore_ascii_case(PRODUCT) {
            continue;
        }
        let recorded = runs[i + 1..]
            .iter()
            .take(PATH_LOOKAHEAD)
            .find_map(|&(start, text)| recorded_path(bytes, start, text));
        if let Some(path) = recorded
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

/// Runs of text between control characters and invalid UTF-8, with their byte
/// offsets. Non-ASCII stays inside a run, because install paths can be in any language.
fn text_runs(bytes: &[u8]) -> Vec<(usize, &str)> {
    let base = bytes.as_ptr() as usize;
    bytes
        .utf8_chunks()
        .flat_map(|chunk| chunk.valid().split(char::is_control))
        .filter(|run| run.len() >= 3)
        .map(|run| (run.as_ptr() as usize - base, run))
        .collect()
}

/// The path inside a text run. A protobuf length byte in front of a string is
/// printable from 32 bytes up, so the real file holds `(C:/Program Files...`,
/// where `(` is the length 40.
fn recorded_path(bytes: &[u8], run_start: usize, run: &str) -> Option<String> {
    let offset = path_offset(run)?;
    let text = &run[offset..];
    let text = declared_len(bytes, run_start + offset)
        .filter(|&n| n >= 3 && n <= text.len() && text.len() - n <= MAX_STUCK_BYTES)
        .and_then(|n| text.get(..n))
        .unwrap_or(text);
    Some(text.trim_end().to_string())
}

fn path_offset(run: &str) -> Option<usize> {
    let b = run.as_bytes();
    let windows_style = b.windows(3).position(|w| {
        w[0].is_ascii_alphabetic() && w[1] == b':' && (w[2] == b'/' || w[2] == b'\\')
    });
    windows_style.or_else(|| match (b.first(), b.get(1)) {
        (_, Some(b'/')) if b[0].is_ascii() => Some(1),
        (Some(b'/'), _) => Some(0),
        _ => None,
    })
}

/// The protobuf varint right before `start`, which is a string's length in bytes.
fn declared_len(bytes: &[u8], start: usize) -> Option<usize> {
    let last = *bytes.get(start.checked_sub(1)?)?;
    if last >= 0x80 {
        return None;
    }
    match start.checked_sub(2).map(|i| bytes[i]) {
        Some(previous) if previous >= 0x80 => {
            Some(usize::from(previous & 0x7f) | usize::from(last) << 7)
        }
        _ => Some(usize::from(last)),
    }
}

/// Create `Interface/AddOns` if the game hasn't yet.
pub fn ensure_addons_dir(install: &Install) -> Result<()> {
    fs::create_dir_all(&install.addons_dir).map_err(|e| not_writable(&install.addons_dir, &e))
}

/// Really create and delete a file in `Interface`, because permissions can lie
/// (antivirus, read-only volumes, a game installed by another account).
pub fn check_writable(install: &Install) -> Result<()> {
    let interface = install.addons_dir.parent().unwrap_or(&install.forever_dir);
    let probe = interface.join(format!(".wiaf-write-test-{}", std::process::id()));
    let write = fs::create_dir_all(interface)
        .and_then(|()| File::create(&probe))
        .and_then(|mut file| file.write_all(b"ok"));
    if let Err(e) = write {
        let _ = fs::remove_file(&probe);
        return Err(not_writable(interface, &e));
    }
    fs::remove_file(&probe).map_err(|e| not_writable(interface, &e))
}

fn not_writable(path: &Path, source: &std::io::Error) -> Error {
    let fix = if cfg!(windows) {
        "Give your Windows account permission to change that folder, or move the game out of \
         Program Files with the Battle.net launcher, then try again."
    } else {
        "Make sure your account owns that folder and the disk isn't read-only, then try again."
    };
    Error::Other(format!(
        "Can't write to {} ({source}). {fix}",
        path.display()
    ))
}

/// Every Forever install this machine knows about, best guess first, without duplicates.
///
/// The slow last resorts, every drive letter on Windows and Spotlight on macOS, only
/// run when the direct sources found nothing. Probing a drive can wake a sleeping disk
/// or a cloud drive, which took 6 seconds on a real machine, against 5 ms without.
pub fn detect_all() -> Vec<Install> {
    dedupe(candidates())
}

fn dedupe(installs: Vec<Install>) -> Vec<Install> {
    let mut seen = std::collections::HashSet::new();
    installs
        .into_iter()
        .filter(|install| seen.insert(install.key()))
        .collect()
}

fn push_valid(found: &mut Vec<Install>, path: &Path, by: FoundBy) {
    match validate_as(path, by) {
        Ok(install) => found.push(install),
        Err(e) => log::debug!("Not a Forever install: {e}"),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn push_from_product_db(found: &mut Vec<Install>, db: &Path) {
    let Some(bytes) = read_capped(db, MAX_PRODUCT_DB_BYTES) else {
        return;
    };
    for recorded in paths_from_product_db(&bytes) {
        push_valid(found, Path::new(&recorded), FoundBy::BattleNet);
    }
}

#[cfg(windows)]
fn candidates() -> Vec<Install> {
    let mut found = Vec::new();
    let program_data = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    push_from_product_db(
        &mut found,
        &program_data.join(r"Battle.net\Agent\product.db"),
    );
    if let Some(hint) = registry_install_path() {
        push_valid(&mut found, Path::new(&hint), FoundBy::Registry);
    }
    if found.is_empty() {
        for path in common_windows_paths(&local_drives()) {
            push_valid(&mut found, &path, FoundBy::CommonPath);
        }
    }
    found
}

#[cfg(windows)]
fn registry_install_path() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\WOW6432Node\Blizzard Entertainment\World of Warcraft")
        .ok()?
        .get_value("InstallPath")
        .ok()
}

/// Drive letters worth probing. Network drives and optical drives are skipped,
/// because a disconnected one can stall a file check for many seconds.
#[cfg(windows)]
fn local_drives() -> Vec<char> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_UNKNOWN,
    };

    // SAFETY: takes no arguments and returns a plain bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
        .filter(|letter| {
            let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
            // SAFETY: `root` is NUL-terminated UTF-16 and outlives the call.
            let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
            !matches!(
                kind,
                DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR | DRIVE_REMOTE | DRIVE_CDROM
            )
        })
        .collect()
}

/// The usual Windows install locations on the given drive letters.
pub fn common_windows_paths(drives: &[char]) -> Vec<PathBuf> {
    drives
        .iter()
        .flat_map(|drive| {
            ["Program Files (x86)", "Program Files", "Games"]
                .map(|folder| PathBuf::from(format!(r"{drive}:\{folder}\{WOW_FOLDER}")))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<Install> {
    let mut found = Vec::new();
    push_valid(
        &mut found,
        &Path::new("/Applications").join(WOW_FOLDER),
        FoundBy::CommonPath,
    );
    if let Some(home) = dirs::home_dir() {
        push_valid(
            &mut found,
            &home.join("Applications").join(WOW_FOLDER),
            FoundBy::CommonPath,
        );
    }
    push_from_product_db(
        &mut found,
        Path::new("/Users/Shared/Battle.net/Agent/product.db"),
    );
    if found.is_empty() {
        for line in spotlight_game_bundles() {
            if let Some(folder) = folder_of_app_bundle(&line) {
                push_valid(&mut found, folder, FoundBy::CommonPath);
            }
        }
    }
    found
}

/// Ask Spotlight for the game's bundle. Spotlight can stall while it indexes, so
/// this gives up after a few seconds rather than hang detection.
#[cfg(target_os = "macos")]
fn spotlight_game_bundles() -> Vec<String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let Ok(mut child) = Command::new("/usr/bin/mdfind")
        .arg("kMDItemCFBundleIdentifier == 'com.blizzard.worldofwarcraft'")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    output.lines().map(str::to_string).collect()
}

/// From a Spotlight result such as `.../_forever_/World of Warcraft.app`, the folder
/// that holds the bundle. [`validate`] takes it from there.
pub fn folder_of_app_bundle(line: &str) -> Option<&Path> {
    let path = Path::new(line.trim());
    let is_app = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
    if is_app { path.parent() } else { None }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn candidates() -> Vec<Install> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let lutris = fs::read_to_string(home.join(".config").join("lutris").join("system.yml")).ok();
    scan_wine_prefixes(&home, lutris.as_deref())
}

/// Forever installs in every known Wine prefix under `home`. `lutris_system_yml` is
/// the text of `~/.config/lutris/system.yml`, when there is one.
pub fn scan_wine_prefixes(home: &Path, lutris_system_yml: Option<&str>) -> Vec<Install> {
    let found = wine_prefixes(home, lutris_system_yml)
        .iter()
        .flat_map(|(prefix, launcher)| scan_wine_prefix(prefix, launcher))
        .collect();
    dedupe(found)
}

/// The Wine prefixes at the fixed places each launcher uses, as `(prefix, launcher)`.
/// Only ever looks at known levels, so a huge home folder costs nothing.
pub fn wine_prefixes(home: &Path, lutris_system_yml: Option<&str>) -> Vec<(PathBuf, String)> {
    let under = |parts: &[&str]| {
        parts
            .iter()
            .fold(home.to_path_buf(), |p, part| p.join(part))
    };
    let mut prefixes = Vec::new();
    let mut add = |path: PathBuf, launcher: &str| {
        if find_child_ci(&path, "drive_c").is_some() {
            prefixes.push((path, launcher.to_string()));
        }
    };

    add(under(&[".wine"]), "Wine");

    let default_games = under(&["Games"]);
    for child in subfolders(&default_games) {
        add(child, "Lutris");
    }
    if let Some(games) = lutris_system_yml.and_then(|yml| parse_lutris_game_path(yml, home))
        && games != default_games
    {
        for child in subfolders(&games) {
            add(child, "Lutris");
        }
        add(games, "Lutris");
    }

    for parent in [
        under(&[".local", "share", "bottles", "bottles"]),
        under(&[
            ".var",
            "app",
            "com.usebottles.bottles",
            "data",
            "bottles",
            "bottles",
        ]),
    ] {
        for child in subfolders(&parent) {
            add(child, "Bottles");
        }
    }

    for steam in [
        under(&[".steam", "steam"]),
        under(&[".local", "share", "Steam"]),
        under(&["snap", "steam", "common", ".local", "share", "Steam"]),
    ] {
        if let Some(compat) = resolve_ci(&steam, &["steamapps", "compatdata"]) {
            for child in subfolders(&compat) {
                if let Some(pfx) = find_child_ci(&child, "pfx") {
                    add(pfx, "Steam");
                }
            }
        }
    }

    for child in subfolders(&under(&["Faugus"])) {
        add(child, "Faugus");
    }
    prefixes
}

/// Forever installs in one Wine prefix: wherever its Battle.net says, plus the
/// default folder.
pub fn scan_wine_prefix(prefix: &Path, launcher: &str) -> Vec<Install> {
    let by = FoundBy::WinePrefix(launcher.to_string());
    let mut found = Vec::new();
    let db = resolve_ci(
        prefix,
        &[
            "drive_c",
            "ProgramData",
            "Battle.net",
            "Agent",
            "product.db",
        ],
    );
    if let Some(bytes) = db.and_then(|db| read_capped(&db, MAX_PRODUCT_DB_BYTES)) {
        for recorded in paths_from_product_db(&bytes) {
            if let Some(path) = map_wine_path(prefix, &recorded) {
                push_valid(&mut found, &path, by.clone());
            }
        }
    }
    if let Some(path) = resolve_ci(prefix, &["drive_c", "Program Files (x86)", WOW_FOLDER]) {
        push_valid(&mut found, &path, by);
    }
    dedupe(found)
}

/// Map a Windows path from inside a prefix to the real folder, matching each segment
/// in any case. `C:` is `drive_c`, and other letters live under `dosdevices`.
pub fn map_wine_path(prefix: &Path, windows_path: &str) -> Option<PathBuf> {
    let (drive, rest) = windows_path.split_once(':')?;
    let [letter] = drive.as_bytes() else {
        return None;
    };
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let letter = letter.to_ascii_lowercase();
    let mut path = if letter == b'c' {
        find_child_ci(prefix, "drive_c")?
    } else {
        find_child_ci(prefix, "dosdevices")?.join(format!("{}:", char::from(letter)))
    };
    for segment in rest.split(['/', '\\']).filter(|s| !s.is_empty()) {
        if segment == "." || segment == ".." {
            return None;
        }
        path = find_child_ci(&path, segment)?;
    }
    Some(path)
}

/// The `game_path` setting from Lutris's `system.yml`, which is plain enough that
/// reading one key doesn't justify a YAML parser.
pub fn parse_lutris_game_path(yml: &str, home: &Path) -> Option<PathBuf> {
    let value = yml.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix("game_path:")?;
        let value = value.split(" #").next().unwrap_or(value);
        Some(value.trim().trim_matches(['"', '\'']))
    })?;
    if value.is_empty() || value == "null" || value == "~" {
        return None;
    }
    Some(match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    })
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    folders.sort();
    folders
}

fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// The child of `dir` called `name` in any case, trying the exact spelling first.
fn find_child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        Some(exact)
    } else {
        find_in_listing(dir, name)
    }
}

/// The slow half of [`find_child_ci`]. A case-insensitive filesystem has already
/// answered by the time it would matter, so this is separate to test on every OS.
fn find_in_listing(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|entry| same_name(&entry.file_name().to_string_lossy(), name))
        .map(|entry| entry.path())
}

fn resolve_ci(base: &Path, segments: &[&str]) -> Option<PathBuf> {
    segments
        .iter()
        .try_fold(base.to_path_buf(), |path, segment| {
            find_child_ci(&path, segment)
        })
}

fn child_or_default(dir: &Path, name: &str) -> PathBuf {
    find_child_ci(dir, name).unwrap_or_else(|| dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.build.info` as shipped on a real install, November 2026.
    const REAL_BUILD_INFO: &str = "Branch!STRING:0|Active!DEC:1|Build Key!HEX:16|CDN Key!HEX:16|Install Key!HEX:16|IM Size!DEC:4|CDN Path!STRING:0|CDN Hosts!STRING:0|CDN Servers!STRING:0|Tags!STRING:0|Armadillo!STRING:0|Last Activated!STRING:0|Version!STRING:0|KeyRing!HEX:16|Product!STRING:0\n\
us|1|9006ba6948388dd0d4a0578e64b8a92e|0e139f0e95542dbe2a5462d8a6b2ebae|||tpr/wow|level3.blizzard.com us.cdn.blizzard.com|http://level3.blizzard.com/?maxhosts=8|Windows x86_64 US? acct-USA? geoip-US? enUS speech?:Windows x86_64 US? acct-USA? geoip-US? enUS text?|||1.60.1.70790||wowf\n";
    const REAL_FLAVOR_INFO: &str = "Product Flavor!STRING:0\nwowf\n";

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A complete Forever install inside `root`, shaped like a real one.
    fn make_game(root: &Path) -> PathBuf {
        let forever = root.join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        write(&forever.join("Wow.exe"), "MZ");
        fs::create_dir_all(forever.join("Interface").join("AddOns")).unwrap();
        fs::create_dir_all(forever.join("WTF")).unwrap();
        write(&root.join(".build.info"), REAL_BUILD_INFO);
        forever
    }

    /// Length-prefixed protobuf field, for building `product.db` fixtures.
    fn field(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let mut len = payload.len();
        while len >= 0x80 {
            out.push((len & 0x7f) as u8 | 0x80);
            len >>= 7;
        }
        out.push(len as u8);
        out.extend_from_slice(payload);
        out
    }

    /// A product record laid out exactly like the real `wowf` one.
    fn product_record(uid: &str, code: &str, path: &str) -> Vec<u8> {
        let mut settings = field(0x0a, path.as_bytes());
        settings.extend(field(0x12, b"us"));
        settings.extend([0x18, 0x02, 0x20, 0x02, 0x28, 0x03]);
        settings.extend(field(0x32, b"enUS"));
        settings.extend(field(0x3a, b"enUS"));
        settings.extend(field(0x5a, b"USA"));
        settings.extend(field(0x62, b"US"));
        settings.extend(field(0x6a, b"_forever_"));

        let mut state = vec![0x08, 0x01, 0x10, 0x00];
        state.extend(field(0x3a, b"1.60.1.70790"));
        state.push(0x52);
        state.push(0x00);
        state.extend(field(0x72, b"9006ba6948388dd0d4a0578e64b8a92e"));

        let mut record = field(0x0a, uid.as_bytes());
        record.extend(field(0x12, code.as_bytes()));
        record.extend(field(0x1a, &settings));
        record.extend(field(0x22, &state));
        field(0x0a, &record)
    }

    /// Other products around `wowf`, then Battle.net's own launcher block, which
    /// mentions `wowf` right next to an unrelated path.
    fn product_db_with(wowf_path: &str) -> Vec<u8> {
        let mut db = product_record("hs_beta", "hsb", "C:/Program Files (x86)/Hearthstone");
        db.extend(product_record(
            "s2",
            "s2",
            "C:/Program Files (x86)/StarCraft II",
        ));
        db.extend(product_record("wowf", "wowf", wowf_path));
        db.extend(product_record(
            "bdko",
            "bdko",
            "C:/ProgramData/Battle.net/bdk",
        ));
        let mut launcher = field(0x0a, b"C:/Program Files (x86)/Battle.net/Battle.net.exe");
        launcher.extend(field(0x1a, b"/install/wowf"));
        launcher.extend(field(0x1a, b"/update/wowf"));
        db.extend(field(0x1a, &launcher));
        db.extend(field(0x3a, b"wowf"));
        db.extend(field(0x3a, b"hs_beta"));
        db.extend(field(0x12, b"wowf"));
        db
    }

    // ---- .build.info ----

    #[test]
    fn reads_the_version_from_a_real_build_info() {
        assert_eq!(
            parse_build_info(REAL_BUILD_INFO).as_deref(),
            Some("1.60.1.70790")
        );
    }

    #[test]
    fn build_info_finds_columns_by_name_and_tolerates_crlf_and_bom() {
        let text = "\u{feff}Product!STRING:0|Version!STRING:0\r\nwow|12.0.0.1\r\nwowf|1.60.1.2\r\n";
        assert_eq!(parse_build_info(text).as_deref(), Some("1.60.1.2"));
    }

    #[test]
    fn build_info_without_a_wowf_row_has_no_version() {
        let text = "Version!STRING:0|Product!STRING:0\n12.0.0.1|wow\n1.15.0.1|wow_classic\n";
        assert_eq!(parse_build_info(text), None);
        let lookalike = "Version!STRING:0|Product!STRING:0\n1.0.0.1|wowfx\n";
        assert_eq!(parse_build_info(lookalike), None);
    }

    #[test]
    fn build_info_prefers_the_active_row() {
        let text = "Active!DEC:1|Version!STRING:0|Product!STRING:0\n\
                    0|1.59.0.1|wowf\n\
                    1|1.60.1.2|wowf\n";
        assert_eq!(parse_build_info(text).as_deref(), Some("1.60.1.2"));
        let only_inactive = "Active!DEC:1|Version!STRING:0|Product!STRING:0\n0|1.59.0.1|wowf\n";
        assert_eq!(parse_build_info(only_inactive).as_deref(), Some("1.59.0.1"));
    }

    #[test]
    fn broken_build_info_never_panics() {
        for text in [
            "",
            "\n\n",
            "garbage",
            "Product!STRING:0\nwowf\n",
            "Version!STRING:0\n1.0\n",
            "Version!STRING:0|Product!STRING:0\n1.0\n",
            "Version!STRING:0|Product!STRING:0\n|wowf\n",
            "## seqn = 5\nVersion!STRING:0|Product!STRING:0\n",
        ] {
            assert_eq!(parse_build_info(text), None, "input: {text:?}");
        }
    }

    // ---- .flavor.info ----

    #[test]
    fn reads_a_real_flavor_info() {
        assert_eq!(parse_flavor_info(REAL_FLAVOR_INFO), ["wowf"]);
        assert_eq!(
            parse_flavor_info("Product Flavor!STRING:0\r\nwowf"),
            ["wowf"]
        );
        assert_eq!(parse_flavor_info("\u{feff}wowf\n"), ["wowf"]);
    }

    #[test]
    fn flavor_info_returns_what_it_says_and_nothing_else() {
        assert_eq!(parse_flavor_info("Product Flavor!STRING:0\nwow\n"), ["wow"]);
        assert_eq!(
            parse_flavor_info("Other!STRING:0|Product Flavor!STRING:0\nx|wow\ny|wowf\nz\n"),
            ["wow", "wowf"]
        );
        assert!(parse_flavor_info("Product Flavor!STRING:0\n").is_empty());
        assert!(parse_flavor_info("").is_empty());
    }

    #[test]
    fn any_row_saying_wowf_counts_but_a_lookalike_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join("Wow.exe"), "MZ");
        write(
            &forever.join(".flavor.info"),
            "Product Flavor!STRING:0\nwow\nWOWF\n",
        );
        assert!(validate(temp.path()).is_ok());
        write(
            &forever.join(".flavor.info"),
            "Product Flavor!STRING:0\nnot wowf\n",
        );
        assert!(validate(temp.path()).is_err());
    }

    // ---- product.db ----

    #[test]
    fn finds_the_wowf_path_among_other_products() {
        let db = product_db_with("C:/Program Files (x86)/World of Warcraft");
        assert_eq!(
            paths_from_product_db(&db),
            ["C:/Program Files (x86)/World of Warcraft"]
        );
    }

    #[test]
    fn reads_bytes_cut_from_a_real_product_db() {
        // The `wowf` record, then the launcher block that follows it in the real file.
        let record: &[u8] = b"\x0a\x04wowf\x12\x04wowf\x1a\x62\x0a\x28C:/Program Files (x86)/World of Warcraft\x12\x02us\x18\x02\x20\x02\x28\x03\x32\x04enUS\x3a\x04enUS\x42\x08\x0a\x04enUS\x10\x03\x4a\x00\x52\x00\x5a\x03USA\x62\x02US\x6a\x09_forever_\x22\xa0\x02\x0a\xac\x01\x08\x01\x10\x00\x18\x00\x20\x00\x28\x00\x3a\x0c1.60.1.70790\x52\x00";
        let launcher: &[u8] = b"\x1a\x53\x0a\x30C:/Program Files (x86)/Battle.net/Battle.net.exe\x10\xe0\xd7\x01\x1a\x0d/install/wowf\x1a\x0c/update/wowf\x2a\x08\x08\x00\x10\xc0\x84\x3d\x18\x00\x30\x83\xd1\xfb\x01\x3a\x04wowf\x3a\x07hs_beta\x3a\x02s1\x3a\x02s2\x42\xf0\x02\x0a\xe7\x02\x0a\x04bdko\x12\x04bdko\x1a\x3c\x0a\x1dC:/ProgramData/Battle.net/bdk\x12\x02us\x18\x02\x20\x02\x12\x04wowf";

        let mut db = record.to_vec();
        db.extend_from_slice(launcher);
        assert_eq!(
            paths_from_product_db(&db),
            ["C:/Program Files (x86)/World of Warcraft"]
        );
        // Without the record, the launcher block's mentions of wowf must find nothing.
        assert!(paths_from_product_db(launcher).is_empty());
    }

    #[test]
    fn a_path_whose_length_byte_is_a_letter_still_starts_at_the_drive() {
        // 67 bytes long, so the length prefix is the printable letter 'C'.
        let path = format!("D:/{}", "x".repeat(64));
        assert_eq!(path.len(), 67);
        assert_eq!(paths_from_product_db(&product_db_with(&path)), [path]);
    }

    #[test]
    fn a_printable_tag_glued_to_the_end_of_the_path_is_cut_off() {
        // Without a region field, the next field's tag `2` follows the path directly.
        let mut settings = field(0x0a, b"C:/Program Files (x86)/World of Warcraft");
        settings.extend(field(0x32, b"enUS"));
        let mut record = field(0x0a, b"wowf");
        record.extend(field(0x12, b"wowf"));
        record.extend(field(0x1a, &settings));
        assert_eq!(
            paths_from_product_db(&field(0x0a, &record)),
            ["C:/Program Files (x86)/World of Warcraft"]
        );
    }

    #[test]
    fn non_ascii_install_paths_survive() {
        let path = "D:/Игры/Wörld of Warcraft";
        assert_eq!(paths_from_product_db(&product_db_with(path)), [path]);
    }

    #[test]
    fn long_paths_with_two_byte_lengths_survive() {
        let path = format!("D:/{}/World of Warcraft", "games/".repeat(30));
        assert!(path.len() > 128);
        assert_eq!(paths_from_product_db(&product_db_with(&path)), [path]);
    }

    #[test]
    fn mac_paths_are_found_even_with_a_printable_length_byte() {
        let short = "/Applications/World of Warcraft";
        assert_eq!(paths_from_product_db(&product_db_with(short)), [short]);
        // 34 bytes long, so the length prefix is `"`.
        let long = "/Users/Pat/Games/World of Warcraft";
        assert_eq!(long.len(), 34);
        assert_eq!(paths_from_product_db(&product_db_with(long)), [long]);
        // 47 bytes long, so the length prefix is itself a slash.
        let slashy = "/Volumes/External Disk/Games/World of Warcraft1";
        assert_eq!(slashy.len(), 47);
        assert_eq!(paths_from_product_db(&product_db_with(slashy)), [slashy]);
    }

    #[test]
    fn products_other_than_wowf_are_ignored() {
        let mut db = product_record("wow", "wow", "C:/Program Files (x86)/World of Warcraft");
        db.extend(product_record(
            "wow_classic",
            "wow_classic",
            "D:/Classic/World of Warcraft",
        ));
        db.extend(product_record("wowfx", "wowfx", "E:/Lookalike"));
        assert!(paths_from_product_db(&db).is_empty());
    }

    #[test]
    fn a_stray_trailing_mention_of_wowf_adds_nothing() {
        let mut launcher = field(0x0a, b"C:/Program Files (x86)/Battle.net/Battle.net.exe");
        launcher.extend(field(0x1a, b"/install/wowf"));
        let mut db = field(0x1a, &launcher);
        db.extend(field(0x3a, b"wowf"));
        db.extend(field(0x3a, b"hs_beta"));
        assert!(paths_from_product_db(&db).is_empty());
    }

    #[test]
    fn junk_bytes_never_panic_or_invent_paths() {
        assert!(paths_from_product_db(b"").is_empty());
        assert!(paths_from_product_db(b"wowf").is_empty());
        assert!(paths_from_product_db(&[0xff; 4096]).is_empty());
        let bytes: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
        assert!(paths_from_product_db(&bytes).is_empty());
    }

    // ---- validate ----

    #[test]
    fn validate_accepts_the_root_the_forever_folder_and_addons() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        let forever = make_game(&root);
        let addons = forever.join("Interface").join("AddOns");

        for path in [
            root.clone(),
            forever.clone(),
            forever.join("Interface"),
            addons.clone(),
        ] {
            let install = validate(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(install.root, root);
            assert_eq!(install.forever_dir, forever);
            assert_eq!(install.addons_dir, addons);
            assert_eq!(install.wtf_dir, forever.join("WTF"));
            assert_eq!(install.version.as_deref(), Some("1.60.1.70790"));
            assert_eq!(install.found_by, FoundBy::Manual);
        }
    }

    #[test]
    fn validate_finds_forever_from_another_flavor_folder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        let forever = make_game(&root);
        fs::create_dir_all(root.join("_retail_")).unwrap();
        assert_eq!(
            validate(&root.join("_retail_")).unwrap().forever_dir,
            forever
        );
    }

    #[test]
    fn folder_presence_alone_never_counts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        fs::create_dir_all(root.join("_forever_").join("Interface").join("AddOns")).unwrap();
        fs::create_dir_all(root.join("_retail_")).unwrap();
        let error = validate(&root).unwrap_err().to_string();
        assert!(error.contains("isn't marked as WoW Forever"), "{error}");
    }

    #[test]
    fn a_flavor_file_without_a_binary_is_a_leftover() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        write(
            &root.join("_forever_").join(".flavor.info"),
            REAL_FLAVOR_INFO,
        );
        let error = validate(&root).unwrap_err().to_string();
        assert!(error.contains("no game in it"), "{error}");
        assert!(error.contains("repair"), "{error}");
    }

    #[test]
    fn a_binary_without_the_right_flavor_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join("Wow.exe"), "MZ");

        assert!(validate(temp.path()).is_err(), "no flavor file");
        for flavor in ["wow", "wow_classic", "wowfx", ""] {
            write(
                &forever.join(".flavor.info"),
                &format!("Product Flavor!STRING:0\n{flavor}\n"),
            );
            assert!(validate(temp.path()).is_err(), "flavor {flavor:?}");
        }
    }

    #[test]
    fn other_flavors_with_a_binary_do_not_count() {
        let temp = tempfile::tempdir().unwrap();
        let retail = temp.path().join("_retail_");
        write(&retail.join("Wow.exe"), "MZ");
        write(
            &retail.join(".flavor.info"),
            "Product Flavor!STRING:0\nwow\n",
        );
        let error = validate(temp.path()).unwrap_err().to_string();
        assert!(error.contains("no _forever_ folder"), "{error}");
    }

    #[test]
    fn a_folder_with_nothing_relevant_is_rejected_with_a_fix() {
        let temp = tempfile::tempdir().unwrap();
        let error = validate(temp.path()).unwrap_err().to_string();
        assert!(
            error.contains("Pick the World of Warcraft folder"),
            "{error}"
        );
        assert!(validate(&temp.path().join("missing")).is_err());
    }

    #[test]
    fn every_game_binary_spelling_counts() {
        for binary in ["Wow.exe", "Wow-ARM64.exe", "WOW.EXE", "wow-arm64.exe"] {
            let temp = tempfile::tempdir().unwrap();
            let forever = temp.path().join("_forever_");
            write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
            write(&forever.join(binary), "MZ");
            assert!(validate(temp.path()).is_ok(), "{binary}");
        }

        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        fs::create_dir_all(forever.join("World of Warcraft.app").join("Contents")).unwrap();
        assert!(validate(temp.path()).is_ok(), "mac bundle");
    }

    #[test]
    fn a_folder_named_like_the_exe_is_not_a_game() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        fs::create_dir_all(forever.join("Wow.exe")).unwrap();
        assert!(validate(temp.path()).is_err());
        assert!(is_game_binary("World of Warcraft.app", true));
        assert!(!is_game_binary("World of Warcraft.app", false));
        assert!(!is_game_binary("WowClassic.exe", false));
    }

    #[test]
    fn a_missing_build_info_only_loses_the_version() {
        let temp = tempfile::tempdir().unwrap();
        make_game(temp.path());
        fs::remove_file(temp.path().join(".build.info")).unwrap();
        let install = validate(temp.path()).unwrap();
        assert_eq!(install.version, None);
    }

    #[test]
    fn missing_addons_and_wtf_get_their_usual_names() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        write(&forever.join("Wow.exe"), "MZ");
        let install = validate(temp.path()).unwrap();
        assert_eq!(install.addons_dir, forever.join("Interface").join("AddOns"));
        assert_eq!(install.wtf_dir, forever.join("WTF"));
    }

    #[test]
    fn listing_lookup_finds_other_spellings_and_keeps_them() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Addons")).unwrap();
        let found = find_in_listing(temp.path(), "AddOns").unwrap();
        assert_eq!(found.file_name().unwrap(), "Addons");
        assert!(find_in_listing(temp.path(), "WTF").is_none());
        assert!(find_in_listing(&temp.path().join("missing"), "WTF").is_none());
    }

    #[test]
    fn name_comparison_ignores_case_for_any_language() {
        assert!(same_name("_FOREVER_", "_forever_"));
        assert!(same_name("ИГРЫ", "игры"));
        assert!(!same_name("_retail_", "_forever_"));
    }

    // ---- Wine prefixes ----

    #[test]
    fn scans_a_prefix_at_the_default_location() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let root = prefix
            .join("drive_c")
            .join("Program Files (x86)")
            .join("World of Warcraft");
        let forever = make_game(&root);

        let found = scan_wine_prefix(&prefix, "Lutris");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].forever_dir, forever);
        assert_eq!(found[0].version.as_deref(), Some("1.60.1.70790"));
        assert_eq!(found[0].found_by, FoundBy::WinePrefix("Lutris".into()));
    }

    #[test]
    fn scans_a_prefix_with_every_folder_in_the_wrong_case() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let forever = prefix
            .join("DRIVE_C")
            .join("PROGRAM FILES (X86)")
            .join("world of warcraft")
            .join("_FOREVER_");
        write(&forever.join(".FLAVOR.INFO"), REAL_FLAVOR_INFO);
        write(&forever.join("WOW.EXE"), "MZ");
        fs::create_dir_all(forever.join("INTERFACE").join("Addons")).unwrap();
        fs::create_dir_all(forever.join("wtf")).unwrap();

        let found = scan_wine_prefix(&prefix, "Wine");
        assert_eq!(found.len(), 1, "{found:?}");
        let addons = found[0].addons_dir.to_string_lossy().to_lowercase();
        assert!(addons.ends_with("interface/addons") || addons.ends_with(r"interface\addons"));
        assert!(found[0].addons_dir.is_dir());
        assert!(found[0].wtf_dir.is_dir());
    }

    #[test]
    fn follows_product_db_inside_a_prefix_in_any_case() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let forever = make_game(&prefix.join("drive_c").join("games").join("wow"));
        write(
            &prefix
                .join("drive_c")
                .join("programdata")
                .join("battle.net")
                .join("agent")
                .join("placeholder"),
            "",
        );
        let db = product_db_with("C:/Games/WoW");
        fs::write(
            prefix
                .join("drive_c")
                .join("programdata")
                .join("battle.net")
                .join("agent")
                .join("product.db"),
            db,
        )
        .unwrap();

        let found = scan_wine_prefix(&prefix, "Bottles");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            fs::canonicalize(&found[0].forever_dir).unwrap(),
            fs::canonicalize(forever).unwrap()
        );
    }

    #[test]
    fn a_game_found_through_both_routes_in_a_prefix_is_listed_once() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        make_game(
            &prefix
                .join("drive_c")
                .join("Program Files (x86)")
                .join("World of Warcraft"),
        );
        write(
            &prefix
                .join("drive_c")
                .join("ProgramData")
                .join("Battle.net")
                .join("Agent")
                .join("placeholder"),
            "",
        );
        fs::write(
            prefix
                .join("drive_c")
                .join("ProgramData")
                .join("Battle.net")
                .join("Agent")
                .join("product.db"),
            product_db_with("C:/Program Files (x86)/World of Warcraft"),
        )
        .unwrap();
        assert_eq!(scan_wine_prefix(&prefix, "Wine").len(), 1);
    }

    #[test]
    fn leftover_flavor_folders_in_a_prefix_are_not_installs() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let root = prefix
            .join("drive_c")
            .join("Program Files (x86)")
            .join("World of Warcraft");
        // A leftover _forever_ with no binary, and a real retail game beside it.
        write(
            &root.join("_forever_").join(".flavor.info"),
            REAL_FLAVOR_INFO,
        );
        fs::create_dir_all(root.join("_forever_").join("Interface").join("AddOns")).unwrap();
        write(&root.join("_retail_").join("Wow.exe"), "MZ");
        write(
            &root.join("_retail_").join(".flavor.info"),
            "Product Flavor!STRING:0\nwow\n",
        );
        assert!(scan_wine_prefix(&prefix, "Wine").is_empty());
    }

    #[test]
    fn a_prefix_with_no_game_or_broken_db_finds_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(prefix.join("drive_c")).unwrap();
        assert!(scan_wine_prefix(&prefix, "Wine").is_empty());

        write(
            &prefix
                .join("drive_c")
                .join("ProgramData")
                .join("Battle.net")
                .join("Agent")
                .join("product.db"),
            "not a protobuf at all",
        );
        assert!(scan_wine_prefix(&prefix, "Wine").is_empty());
        assert!(scan_wine_prefix(&temp.path().join("missing"), "Wine").is_empty());
    }

    #[test]
    fn maps_wine_paths_and_refuses_to_climb_out() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path();
        fs::create_dir_all(prefix.join("drive_c").join("Games").join("WoW")).unwrap();

        let real = fs::canonicalize(prefix.join("drive_c").join("Games").join("WoW")).unwrap();
        for recorded in ["C:/GAMES/wow", r"c:\Games\WoW", "C:/Games/WoW/"] {
            let mapped = map_wine_path(prefix, recorded).unwrap();
            assert_eq!(fs::canonicalize(mapped).unwrap(), real, "{recorded}");
        }

        assert_eq!(map_wine_path(prefix, "C:/Games/../Games/WoW"), None);
        assert_eq!(map_wine_path(prefix, "C:/Games/Missing"), None);
        assert_eq!(map_wine_path(prefix, "Games/WoW"), None);
        assert_eq!(map_wine_path(prefix, "/Games/WoW"), None);
        assert_eq!(map_wine_path(prefix, "CC:/Games"), None);
    }

    // A folder named `d:` can't exist on Windows.
    #[cfg(unix)]
    #[test]
    fn other_drive_letters_go_through_dosdevices() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path();
        fs::create_dir_all(prefix.join("dosdevices").join("d:").join("Games")).unwrap();
        fs::create_dir_all(prefix.join("drive_c")).unwrap();
        let mapped = map_wine_path(prefix, "D:/Games").unwrap();
        assert!(mapped.ends_with("dosdevices/d:/Games"), "{mapped:?}");
        assert!(map_wine_path(prefix, "d:/GAMES").unwrap().is_dir());
        assert_eq!(map_wine_path(prefix, "E:/Games"), None);
    }

    fn make_prefix(path: &Path) -> PathBuf {
        fs::create_dir_all(path.join("drive_c")).unwrap();
        path.to_path_buf()
    }

    #[test]
    fn enumerates_each_launcher_at_its_fixed_level() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let under = |parts: &[&str]| {
            parts
                .iter()
                .fold(home.to_path_buf(), |p, part| p.join(part))
        };

        let expected = [
            (make_prefix(&under(&[".wine"])), "Wine"),
            (make_prefix(&under(&["Games", "battlenet"])), "Lutris"),
            (make_prefix(&under(&["BigDisk", "wow"])), "Lutris"),
            (make_prefix(&under(&["BigDisk"])), "Lutris"),
            (
                make_prefix(&under(&[".local", "share", "bottles", "bottles", "WoW"])),
                "Bottles",
            ),
            (
                make_prefix(&under(&[
                    ".var",
                    "app",
                    "com.usebottles.bottles",
                    "data",
                    "bottles",
                    "bottles",
                    "Flat",
                ])),
                "Bottles",
            ),
            (
                make_prefix(&under(&[
                    ".steam",
                    "steam",
                    "steamapps",
                    "compatdata",
                    "111",
                    "pfx",
                ])),
                "Steam",
            ),
            (
                make_prefix(&under(&[
                    ".local",
                    "share",
                    "Steam",
                    "steamapps",
                    "compatdata",
                    "222",
                    "pfx",
                ])),
                "Steam",
            ),
            (
                make_prefix(&under(&[
                    "snap",
                    "steam",
                    "common",
                    ".local",
                    "share",
                    "Steam",
                    "steamapps",
                    "compatdata",
                    "333",
                    "pfx",
                ])),
                "Steam",
            ),
            (make_prefix(&under(&["Faugus", "wow"])), "Faugus"),
        ];
        // Things that must not be found: no drive_c, and too deep to be reached.
        fs::create_dir_all(under(&["Games", "not-a-prefix"])).unwrap();
        make_prefix(&under(&["Games", "battlenet", "nested", "deep"]));
        make_prefix(&under(&["Documents", "wow"]));

        let yml = "system:\n  game_path: ~/BigDisk\n";
        let found = wine_prefixes(home, Some(yml));
        for (path, launcher) in &expected {
            assert!(
                found.contains(&(path.clone(), (*launcher).to_string())),
                "missing {} from {found:?}",
                path.display()
            );
        }
        assert_eq!(found.len(), expected.len(), "{found:?}");
    }

    #[test]
    fn an_empty_home_has_no_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        assert!(wine_prefixes(temp.path(), None).is_empty());
        assert!(wine_prefixes(&temp.path().join("missing"), None).is_empty());
    }

    #[test]
    fn finds_a_game_through_a_full_home_scan() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let prefix = home
            .join(".local")
            .join("share")
            .join("Steam")
            .join("steamapps")
            .join("compatdata")
            .join("12345")
            .join("pfx");
        let forever = make_game(
            &prefix
                .join("drive_c")
                .join("Program Files (x86)")
                .join("World of Warcraft"),
        );
        let found = scan_wine_prefixes(home, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].forever_dir, forever);
        assert_eq!(found[0].found_by, FoundBy::WinePrefix("Steam".into()));
    }

    #[test]
    fn parses_the_lutris_game_path() {
        let home = Path::new("/home/pat");
        let parse = |yml| parse_lutris_game_path(yml, home);
        assert_eq!(
            parse("system:\n  game_path: /mnt/games\n"),
            Some(PathBuf::from("/mnt/games"))
        );
        assert_eq!(
            parse("system:\n  game_path: '/mnt/my games'\n"),
            Some(PathBuf::from("/mnt/my games"))
        );
        assert_eq!(
            parse("system:\n  game_path: \"/mnt/games\"\r\n"),
            Some(PathBuf::from("/mnt/games"))
        );
        assert_eq!(
            parse("system:\n  game_path: ~/Lutris\n"),
            Some(home.join("Lutris"))
        );
        assert_eq!(
            parse("system:\n  game_path: /mnt/games # the big disk\n"),
            Some(PathBuf::from("/mnt/games"))
        );
        assert_eq!(parse("system:\n  # game_path: /old\n"), None);
        assert_eq!(parse("system:\n  game_path:\n"), None);
        assert_eq!(parse("system:\n  game_path: null\n"), None);
        assert_eq!(parse(""), None);
    }

    // ---- macOS ----

    #[test]
    fn spotlight_results_map_to_the_folder_holding_the_bundle() {
        assert_eq!(
            folder_of_app_bundle("/Applications/World of Warcraft/_forever_/World of Warcraft.app"),
            Some(Path::new("/Applications/World of Warcraft/_forever_"))
        );
        assert_eq!(folder_of_app_bundle("/Applications/Notes"), None);
        assert_eq!(folder_of_app_bundle(""), None);
    }

    #[test]
    fn a_spotlight_folder_from_another_flavor_still_finds_forever() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        let forever = root.join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        fs::create_dir_all(forever.join(GAME_BUNDLE)).unwrap();
        fs::create_dir_all(root.join("_retail_").join(GAME_BUNDLE)).unwrap();
        let line = root.join("_retail_").join(GAME_BUNDLE);
        let folder = folder_of_app_bundle(line.to_str().unwrap()).unwrap();
        assert_eq!(validate(folder).unwrap().forever_dir, forever);
    }

    // ---- Windows ----

    #[test]
    fn common_windows_paths_cover_each_drive() {
        let paths = common_windows_paths(&['C', 'D']);
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            paths,
            [
                r"C:\Program Files (x86)\World of Warcraft",
                r"C:\Program Files\World of Warcraft",
                r"C:\Games\World of Warcraft",
                r"D:\Program Files (x86)\World of Warcraft",
                r"D:\Program Files\World of Warcraft",
                r"D:\Games\World of Warcraft",
            ]
        );
    }

    #[test]
    fn a_registry_style_hint_inside_a_flavor_folder_is_stripped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("World of Warcraft");
        let forever = make_game(&root);
        let hint = format!("{}{}", forever.display(), std::path::MAIN_SEPARATOR);
        let install = validate_as(Path::new(&hint), FoundBy::Registry).unwrap();
        assert_eq!(install.root, root);
        assert_eq!(install.found_by, FoundBy::Registry);
    }

    #[test]
    fn strip_flavor_folder_only_touches_flavor_folders() {
        let base = Path::new("games");
        assert_eq!(strip_flavor_folder(&base.join("_forever_")), base);
        assert_eq!(strip_flavor_folder(&base.join("_classic_era_")), base);
        assert_eq!(strip_flavor_folder(&base.join("wow")), base.join("wow"));
        assert_eq!(strip_flavor_folder(&base.join("__")), base.join("__"));
    }

    // ---- de-duplication ----

    #[test]
    fn the_same_install_found_twice_is_listed_once_with_its_first_source() {
        let temp = tempfile::tempdir().unwrap();
        make_game(temp.path());
        let installs = vec![
            validate_as(temp.path(), FoundBy::BattleNet).unwrap(),
            validate_as(temp.path(), FoundBy::Registry).unwrap(),
            validate_as(&temp.path().join("_forever_"), FoundBy::CommonPath).unwrap(),
        ];
        let unique = dedupe(installs);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].found_by, FoundBy::BattleNet);
    }

    #[test]
    fn different_installs_stay_in_priority_order() {
        let temp = tempfile::tempdir().unwrap();
        let (first, second) = (temp.path().join("a"), temp.path().join("b"));
        make_game(&first);
        make_game(&second);
        let installs = vec![
            validate_as(&second, FoundBy::BattleNet).unwrap(),
            validate_as(&first, FoundBy::CommonPath).unwrap(),
        ];
        let unique = dedupe(installs);
        assert_eq!(unique.len(), 2);
        assert_eq!(unique[0].root, second);
        assert_eq!(unique[1].root, first);
        assert_ne!(unique[0].key(), unique[1].key());
    }

    #[cfg(windows)]
    #[test]
    fn install_keys_ignore_case_and_verbatim_prefixes_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        make_game(temp.path());
        let install = validate(temp.path()).unwrap();
        let shouted = PathBuf::from(temp.path().display().to_string().to_uppercase());
        assert_eq!(install.key(), validate(&shouted).unwrap().key());
        assert!(!install.key().starts_with(r"\\?\"));
    }

    // ---- writing ----

    #[test]
    fn ensure_addons_dir_creates_the_folder() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        write(&forever.join("Wow.exe"), "MZ");
        let install = validate(temp.path()).unwrap();
        assert!(!install.addons_dir.exists());
        ensure_addons_dir(&install).unwrap();
        assert!(install.addons_dir.is_dir());
        ensure_addons_dir(&install).unwrap();
    }

    #[test]
    fn check_writable_cleans_up_after_itself() {
        let temp = tempfile::tempdir().unwrap();
        let forever = make_game(temp.path());
        let install = validate(temp.path()).unwrap();
        check_writable(&install).unwrap();
        let leftovers: Vec<_> = fs::read_dir(forever.join("Interface"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "AddOns")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn unwritable_folders_give_an_error_with_the_path_and_a_fix() {
        let temp = tempfile::tempdir().unwrap();
        let forever = temp.path().join("_forever_");
        write(&forever.join(".flavor.info"), REAL_FLAVOR_INFO);
        write(&forever.join("Wow.exe"), "MZ");
        // A file where the Interface folder should be makes every write fail, on every OS.
        write(&forever.join("Interface"), "in the way");
        let install = validate(temp.path()).unwrap();

        for error in [
            check_writable(&install).unwrap_err(),
            ensure_addons_dir(&install).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(message.contains("Interface"), "{message}");
            assert!(message.contains("then try again"), "{message}");
        }
    }

    // ---- this machine ----

    /// Read-only check against the real game: `cargo test -p feeling-core -- --ignored --nocapture real_machine`
    #[test]
    #[ignore = "needs a real Forever install on this machine"]
    fn real_machine_finds_forever_and_nothing_else() {
        let start = std::time::Instant::now();
        let found = detect_all();
        println!("detect_all took {:?} and found {found:#?}", start.elapsed());

        let install = found.first().expect("no install found");
        assert!(install.forever_dir.ends_with(FOREVER_FOLDER));
        assert!(
            install
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("1.60.1.")),
            "version: {:?}",
            install.version
        );
        for install in &found {
            let path = install.forever_dir.to_string_lossy();
            for leftover in ["_retail_", "_classic_", "_classic_era_"] {
                assert!(!path.contains(leftover), "reported a leftover: {path}");
            }
        }
        let folders = crate::scan::scan_addons(&install.addons_dir).unwrap();
        println!("folders in AddOns: {}", folders.len());
        println!("game running: {}", crate::gameproc::is_game_running());
    }

    /// Each Windows source on its own, since [`detect_all`] stops early once one hits.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a real Battle.net install on this machine"]
    fn real_machine_every_windows_source_finds_forever() {
        let expected = r"C:\Program Files (x86)\World of Warcraft\_forever_";

        let program_data = std::env::var_os("ProgramData").unwrap();
        let db = Path::new(&program_data).join(r"Battle.net\Agent\product.db");
        let paths = paths_from_product_db(&fs::read(db).unwrap());
        println!("product.db: {paths:?}");
        assert_eq!(paths, ["C:/Program Files (x86)/World of Warcraft"]);

        let hint = registry_install_path().expect("no registry value");
        println!("registry: {hint}");
        let install = validate_as(Path::new(&hint), FoundBy::Registry).unwrap();
        assert_eq!(install.forever_dir, Path::new(expected));

        let drives = local_drives();
        println!("local drives: {drives:?}");
        assert!(drives.contains(&'C'));
        let mut found = Vec::new();
        for path in common_windows_paths(&drives) {
            push_valid(&mut found, &path, FoundBy::CommonPath);
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].forever_dir, Path::new(expected));
    }
}
