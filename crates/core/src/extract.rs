//! Safe zip extraction. Every archive is treated as hostile.
//!
//! All checks run before the first byte is written, so a rejected zip never leaves
//! anything behind. Names are validated by the strictest rules (Windows) on every
//! OS, so a zip behaves the same for every player. `ZipArchive::extract` is never
//! used, and nothing here creates a symlink or keeps a file's permissions by default.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use zip::read::{ArchiveOffset, Config};
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive};

use crate::catalog::Move;
use crate::error::{Error, Result};
use crate::net::Progress;
use crate::toc::TOC_SUFFIXES;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const BUFFER_SIZE: usize = 64 * 1024;
/// Longest path an entry may have, in bytes, out of the 65,535 a zip allows. Every step
/// after reading a name costs at least its length. The longest in the catalog is 139.
const MAX_ENTRY_NAME_BYTES: usize = 1024;
/// Most folders above an entry. Making a folder walks its whole path, so this is what keeps
/// one name from taking minutes. The deepest in the catalog is 11.
const MAX_ENTRY_DEPTH: usize = 64;

/// Dropped without a word: operating systems and git leave these everywhere.
/// `Icon\r` is the marker macOS adds to folders with a custom icon.
const JUNK_NAMES: [&str; 8] = [
    "__MACOSX",
    ".git",
    ".github",
    ".gitignore",
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    "Icon\r",
];

/// Never written, because nothing in an addon needs them and players do double-click
/// things. Skipped and reported rather than fatal, since dev scripts turn up in real
/// source archives. The last few aren't in the design's list, but a shortcut or an
/// HTML application in an addon folder is the same lure as an `.exe`. Checked on every
/// segment of a path, so nothing inside a `Foo.app/` bundle gets written either.
const BLOCKED_EXTENSIONS: [&str; 19] = [
    "exe", "dll", "scr", "com", "msi", "bat", "cmd", "ps1", "sh", "jar", "vbs", "app", "dylib",
    "so", "lnk", "pif", "hta", "cpl", "wsf",
];

/// The same lure, on file names only, because a folder called `Foo.Terminal` is just a
/// folder: script hosts and shortcuts that run on a double-click, then disk images and
/// archives that put a program one click away. The game reads none of these. It loads
/// what its TOCs list, which is `lua`, `xml`, `blp`, `tga`, `ogg`, `mp3`, `ttf` and so on.
const BLOCKED_FILE_EXTENSIONS: [&str; 32] = [
    "vbe",
    "jse",
    "js",
    "wsh",
    "msc",
    "chm",
    "reg",
    "msp",
    "url",
    "website",
    "scf",
    "library-ms",
    "searchconnector-ms",
    "appref-ms",
    "application",
    "xbap",
    "settingcontent-ms",
    "xll",
    "diagcab",
    "py",
    "pyw",
    "pyz",
    "command",
    "terminal",
    "iso",
    "img",
    "vhd",
    "vhdx",
    "zip",
    "7z",
    "rar",
    "cab",
];

/// `Foo_Vanilla.toc`, `Foo-Classic.toc` and friends: TOC names the Forever client
/// doesn't load, but that still tell us what a root-level addon's folder should be.
const OTHER_FLAVORS: [&str; 10] = [
    "Standard", "Classic", "Vanilla", "TBC", "BCC", "Wrath", "WOTLKC", "Cata", "Mists", "MoP",
];

/// Hard caps on what one zip may do to the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_entries: usize,
    /// How much of the file the zip crate may read to list what's inside. It keeps several
    /// copies of what it reads, so this is what bounds its memory, however big the zip is.
    pub max_directory_bytes: u64,
    /// Folders the zip would create, whether or not it lists them. Each one costs real
    /// time and disk that no count of entries or bytes would ever see.
    pub max_folders: usize,
    /// Output bytes per compressed byte. Lua compresses well, but not this well.
    pub max_ratio: u64,
    /// The ratio is only judged past this many bytes, because tiny files can
    /// legitimately have silly ratios.
    pub ratio_floor_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_total_bytes: 2 * GB,
            max_file_bytes: 512 * MB,
            max_entries: 200_000,
            // The biggest in the catalog is 11 MB, for 104,000 files. The catalog scan gives
            // up at this same size, so nothing it could read trips this.
            max_directory_bytes: 64 * MB,
            // The most in the catalog is 1,700.
            max_folders: 20_000,
            max_ratio: 1000,
            ratio_floor_bytes: 64 * MB,
        }
    }
}

/// What landed in the destination directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    /// Addon folders, sorted.
    pub folders: Vec<String>,
    /// Executables and scripts that were left out, as `Folder/path/file.exe`.
    pub skipped_files: Vec<String>,
    pub bytes: u64,
}

/// Extract an addon zip into `dest_dir/<Folder>/...`, following the layout rules:
/// `moves` when given (see [`Move`]), otherwise top-level folders with a TOC, one
/// unwrapped wrapper folder, or a TOC at the zip's root. On error nothing is left.
pub fn extract(
    zip_path: &Path,
    dest_dir: &Path,
    moves: &[Move],
    limits: Limits,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Extracted> {
    let (mut archive, zip_len) = open_archive(zip_path, &limits)?;
    let entries = read_entries(&mut archive, &limits)?;
    let plan = plan_addons(&entries, moves, &limits)?;
    run_plan(
        &mut archive,
        zip_len,
        &plan,
        dest_dir,
        limits,
        false,
        progress,
        cancel,
    )
}

/// How [`extract_tree`] treats what it finds.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TreeOptions {
    /// Write executables instead of skipping them, for our own signed app bundle.
    pub allow_executables: bool,
    /// Restore the exec bit from the zip on Unix.
    pub keep_exec_bits: bool,
}

/// Extract a whole zip, without the addon layout rules, through the same checks and
/// limits. For WTF backups and the macOS app bundle. `folders` lists the top-level names.
pub(crate) fn extract_tree(
    zip_path: &Path,
    dest_dir: &Path,
    limits: Limits,
    options: TreeOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Extracted> {
    let (mut archive, zip_len) = open_archive(zip_path, &limits)?;
    let entries = read_entries(&mut archive, &limits)?;
    let plan = plan_tree(&entries, options, &limits)?;
    run_plan(
        &mut archive,
        zip_len,
        &plan,
        dest_dir,
        limits,
        options.keep_exec_bits,
        progress,
        cancel,
    )
}

/// The absolute form of `path` that survives the 260 character limit: `\\?\`-prefixed
/// on Windows, plain elsewhere. Verbatim paths skip all normalization, so build on
/// the result one clean segment at a time, never with `/` or `..` in a push.
pub(crate) fn long_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    to_verbatim(&absolute)
}

#[cfg(windows)]
fn to_verbatim(absolute: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::path::{Component, Prefix};

    let mut components = absolute.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return absolute.to_path_buf();
    };
    match prefix.kind() {
        Prefix::Disk(_) => {
            let mut verbatim = OsString::from(r"\\?\");
            verbatim.push(absolute.as_os_str());
            PathBuf::from(verbatim)
        }
        Prefix::UNC(server, share) => {
            let mut verbatim = OsString::from(r"\\?\UNC\");
            verbatim.push(server);
            verbatim.push(r"\");
            verbatim.push(share);
            let mut result = PathBuf::from(verbatim);
            result.extend(components.filter(|c| matches!(c, Component::Normal(_))));
            result
        }
        _ => absolute.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn to_verbatim(absolute: &Path) -> PathBuf {
    absolute.to_path_buf()
}

/// `path` without any `\\?\`, for showing to a player.
pub(crate) fn display_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// An IO error for players: the path they know, not our internal spelling of it.
pub(crate) fn io_error(path: &Path, error: io::Error) -> Error {
    Error::io(display_path(path), error)
}

/// Whether `name` can be one folder directly under AddOns. For names that come from
/// outside a zip too, such as catalog moves and managed-folder records.
pub(crate) fn is_safe_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && !name.contains(['/', '\\'])
        && segment_problem(name).is_none()
}

fn unsafe_zip(reason: impl Into<String>) -> Error {
    Error::UnsafeZip(reason.into())
}

/// Open a zip the way the catalog scan does: the file list is where the end of the file
/// says it is, with nothing in front of the zip. The crate's default hunts for whichever
/// list it likes best instead, so one file with two lists could show the catalog one
/// addon and install another, and the hunt reads through the whole file.
fn open_archive(zip_path: &Path, limits: &Limits) -> Result<(ZipArchive<ZipReader>, u64)> {
    let long = long_path(zip_path);
    let mut file = File::open(&long).map_err(|e| io_error(&long, e))?;
    let zip_len = file.metadata().map_err(|e| io_error(&long, e))?.len();
    let declared = most_files_declared(&mut file, zip_len).map_err(|e| io_error(&long, e))?;
    if declared > limits.max_entries as u64 {
        return Err(too_many_files(declared, limits));
    }
    let budget = Rc::new(Cell::new(limits.max_directory_bytes));
    let reader = ReadBudget {
        inner: BufReader::new(file),
        left: Rc::clone(&budget),
    };
    let config = Config {
        archive_offset: ArchiveOffset::Known(0),
    };
    let archive = ZipArchive::with_config(config, reader).map_err(|e| {
        if budget.get() == 0 {
            return unsafe_zip(format!(
                "its list of files is over the {} MB limit.",
                limits.max_directory_bytes / MB
            ));
        }
        match e {
            ZipError::Io(io) if io.kind() != io::ErrorKind::UnexpectedEof => io_error(&long, io),
            other => match foreign_archive(&long) {
                Some(kind) => Error::Other(format!(
                    "This addon is packed as a {kind} file, which this app can't open. \
                     Its author may offer a zip of another version."
                )),
                None => zip_error(other, "the zip"),
            },
        }
    })?;
    // From here on it's reading the files themselves, which have limits of their own.
    budget.set(u64::MAX);
    Ok((archive, zip_len))
}

type ZipReader = ReadBudget<BufReader<File>>;

/// Fails once a set number of bytes has been read, however they were asked for. Counting
/// here can't be fooled the way believing what a zip says about itself can.
struct ReadBudget<R> {
    inner: R,
    left: Rc<Cell<u64>>,
}

impl<R: Read> Read for ReadBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self.left.get();
        if left == 0 {
            return Err(io::Error::other("the zip's list of files is too long"));
        }
        let wanted = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut buffer[..wanted])?;
        self.left.set(left - count as u64);
        Ok(count)
    }
}

impl<R: Seek> Seek for ReadBudget<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

/// Far enough from the end of a file to hold the longest zip comment and what's around it.
const ZIP_TAIL_BYTES: u64 = 66 * KB;

/// The most files any zip64 record near the end of the file claims. The zip crate sets
/// aside room for that many before it reads a single one, and the only check on the number
/// is the size of the file, so a big zip can ask for several times its size up front. This
/// looks at every end marker in reach, because the crate moves on to earlier ones when one
/// doesn't work out, so a harmless last one must not hide what's in front of it.
fn most_files_declared(file: &mut File, zip_len: u64) -> io::Result<u64> {
    file.seek(SeekFrom::Start(zip_len.saturating_sub(ZIP_TAIL_BYTES)))?;
    let mut tail = Vec::new();
    Read::by_ref(file)
        .take(ZIP_TAIL_BYTES)
        .read_to_end(&mut tail)?;
    let read_u64 = |bytes: &[u8]| {
        let mut number = [0u8; 8];
        number.copy_from_slice(bytes);
        u64::from_le_bytes(number)
    };
    let mut most = 0;
    for (end, _) in tail
        .windows(4)
        .enumerate()
        .filter(|(end, mark)| *end >= 20 && *mark == b"PK\x05\x06")
    {
        // A zip64 locator is 20 bytes: its mark, a disk number, then where the record is.
        let locator = &tail[end - 20..end];
        if &locator[..4] != b"PK\x06\x07" {
            continue;
        }
        let mut record = [0u8; 40];
        if file
            .seek(SeekFrom::Start(read_u64(&locator[8..16])))
            .is_ok()
            && file.read_exact(&mut record).is_ok()
            && &record[..4] == b"PK\x06\x06"
        {
            most = most.max(read_u64(&record[32..40]));
        }
    }
    Ok(most)
}

/// Older WoWInterface uploads include the first two. A real, intact file, so "corrupt,
/// try again" would send the player in circles.
fn foreign_archive(path: &Path) -> Option<&'static str> {
    let mut magic = [0u8; 6];
    File::open(path).ok()?.read_exact(&mut magic).ok()?;
    match magic {
        [b'R', b'a', b'r', b'!', ..] => Some("RAR"),
        [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C] => Some("7-Zip"),
        [b'M', b'Z', ..] => Some("self-extracting program"),
        _ => None,
    }
}

fn zip_error(error: ZipError, name: &str) -> Error {
    match error {
        ZipError::Io(e) => read_error(e, name),
        ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
            unsafe_zip("it's packed in a way this app can't read, such as unusual compression.")
        }
        _ => Error::Corrupt("it isn't a valid zip file".into()),
    }
}

/// One file or folder in the archive, with a checked path.
#[derive(Debug, Clone)]
struct ZipEntry {
    index: usize,
    /// Cleaned path: `/` separators, no empty, `.` or `..` segments.
    path: String,
    is_dir: bool,
    size: u64,
    compressed_size: u64,
}

/// Validate every entry before anything is written, including ones the layout rules
/// would drop, so a zip caught doing something strange is rejected outright.
fn read_entries<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: &Limits,
) -> Result<Vec<ZipEntry>> {
    if archive.len() > limits.max_entries {
        return Err(too_many_files(archive.len() as u64, limits));
    }
    if archive.has_duplicate_names() {
        return Err(unsafe_zip("it lists the same file name twice."));
    }
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let meta = archive
            .by_index_data(index)
            .map_err(|_| Error::Corrupt("it isn't a valid zip file".into()))?;
        let name = meta
            .name()
            .map_err(|_| Error::Corrupt("a file name in it can't be read".into()))?;
        let path = clean_entry_path(&name).map_err(Error::UnsafeZip)?;
        // The crate's own check, on top of ours. Junk is exempt because it's never
        // written and `Icon\r` trips the crate's Windows rules.
        if !is_junk_path(&path) && meta.enclosed_name().is_none() {
            return Err(unsafe_zip(format!(
                "it has an unsafe path ({}).",
                printable(&name)
            )));
        }
        if meta.is_symlink() {
            return Err(unsafe_zip(format!(
                "it contains a symlink ({}).",
                printable(&name)
            )));
        }
        if let Some(mode) = meta.unix_mode()
            && !matches!(mode & S_IFMT, 0 | S_IFREG | S_IFDIR)
        {
            return Err(unsafe_zip(format!(
                "it contains a special device or pipe ({}).",
                printable(&name)
            )));
        }
        if meta.encrypted() {
            return Err(unsafe_zip("it's protected by a password."));
        }
        if !matches!(
            meta.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(unsafe_zip(format!(
                "it's packed with compression this app can't read ({}).",
                printable(&name)
            )));
        }
        if !path.is_empty() {
            let is_dir = meta.is_dir() || meta.unix_mode().is_some_and(|m| m & S_IFMT == S_IFDIR);
            let (size, compressed_size) = (meta.size(), meta.compressed_size());
            entries.push(ZipEntry {
                index,
                path,
                is_dir,
                size,
                compressed_size,
            });
        }
    }
    // Entries sharing their compressed data are how non-recursive zip bombs get to
    // trillions of output bytes from a few kilobytes. No honest packer does this.
    let overlapping = archive
        .has_overlapping_files()
        .map_err(|_| Error::Corrupt("it isn't a valid zip file".into()))?;
    if overlapping {
        return Err(unsafe_zip(
            "several of its files share the same data, a trick used by zip bombs.",
        ));
    }
    Ok(entries)
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// Normalize an entry name to `/`-separated clean segments, or say why it's unsafe.
/// Backslashes count as separators on every OS, because the same zip must never mean
/// `..\..\evil` on Windows and an innocent file name on Linux.
fn clean_entry_path(name: &str) -> std::result::Result<String, String> {
    if !entry_name_fits(name) {
        return Err(format!(
            "it has a file path that's too long or has too many folders ({}).",
            printable(name)
        ));
    }
    if name.contains('\0') {
        return Err(format!(
            "it has a file name with a hidden NUL ({}).",
            printable(name)
        ));
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(format!("it has an absolute path ({}).", printable(name)));
    }
    let mut clean = String::with_capacity(unified.len());
    for segment in unified.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(format!(
                    "it has a path that climbs out of its folder ({}).",
                    printable(name)
                ));
            }
            segment => {
                if !segment.eq_ignore_ascii_case("Icon\r")
                    && let Some(problem) = segment_problem(segment)
                {
                    return Err(format!("{problem} ({}).", printable(name)));
                }
                if !clean.is_empty() {
                    clean.push('/');
                }
                clean.push_str(segment);
            }
        }
    }
    Ok(clean)
}

/// Whether a path is short and shallow enough for a zip entry. Both separators count, and
/// so do empty and `.` segments, because this runs before any of them are cleaned away.
pub(crate) fn entry_name_fits(name: &str) -> bool {
    name.len() <= MAX_ENTRY_NAME_BYTES && name.matches(['/', '\\']).count() <= MAX_ENTRY_DEPTH
}

/// Why one path segment can't be written safely on every OS, if it can't.
fn segment_problem(segment: &str) -> Option<&'static str> {
    if segment == ".." {
        return Some("it has a path that climbs out of its folder");
    }
    if segment.contains(':') {
        return Some("it has a colon in a file name, which could hide data or name a drive");
    }
    if segment
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Some("it has a file name Windows can't store");
    }
    if segment.ends_with(['.', ' ']) {
        return Some("it has a file name ending in a dot or space, which Windows silently changes");
    }
    if is_device_name(segment) {
        return Some("it uses a reserved Windows device name");
    }
    if segment.encode_utf16().count() > 255 {
        return Some("it has a file name that's too long");
    }
    None
}

/// `CON`, `nul.txt`, `COM1.tar.gz` and the like open a device on Windows, not a file.
fn is_device_name(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return true;
    }
    let Some(number) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(
        number,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

/// Keep untrusted names short and harmless in error text.
fn printable(name: &str) -> String {
    let mut shown: String = name
        .chars()
        .take(80)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if name.chars().count() > 80 {
        shown.push('…');
    }
    shown
}

/// For comparing names the way Windows and macOS do. Good enough to catch collisions
/// early; `create_new` on write is the backstop for whatever Unicode slips past.
pub(crate) fn fold(text: &str) -> String {
    text.to_lowercase()
}

fn is_junk_path(path: &str) -> bool {
    path.split('/').any(|segment| {
        JUNK_NAMES
            .iter()
            .any(|junk| segment.eq_ignore_ascii_case(junk))
    })
}

fn is_blocked_path(path: &str, is_dir: bool) -> bool {
    let has_extension_in = |name: &str, list: &[&str]| {
        name.rsplit_once('.')
            .is_some_and(|(_, ext)| list.iter().any(|blocked| ext.eq_ignore_ascii_case(blocked)))
    };
    let file_name = path.rsplit('/').next().unwrap_or(path);
    (!is_dir && has_extension_in(file_name, &BLOCKED_FILE_EXTENSIONS))
        || path
            .split('/')
            .any(|segment| has_extension_in(segment, &BLOCKED_EXTENSIONS))
}

fn is_toc_name(name: &str) -> bool {
    !name.starts_with('.')
        && name.len() > 4
        && name.is_char_boundary(name.len() - 4)
        && name[name.len() - 4..].eq_ignore_ascii_case(".toc")
}

fn strip_ignore_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = text.len().checked_sub(suffix.len())?;
    if text.is_char_boundary(cut) && text[cut..].eq_ignore_ascii_case(suffix) {
        Some(&text[..cut])
    } else {
        None
    }
}

/// The folder a root-level TOC belongs in: its stem without any flavor suffix. When
/// several disagree, the one the Forever client would pick first wins, then the
/// alphabetically first, so the answer never depends on the zip's order.
fn folder_from_root_tocs(toc_names: &[&str]) -> Option<String> {
    let stems: Vec<&str> = toc_names
        .iter()
        .map(|name| &name[..name.len() - 4])
        .collect();
    let for_forever = TOC_SUFFIXES
        .iter()
        .filter(|suffix| !suffix.is_empty())
        .find_map(|suffix| {
            stems
                .iter()
                .filter_map(|stem| strip_ignore_case(stem, suffix))
                .filter(|base| !base.is_empty())
                .min()
        });
    let best = for_forever.or_else(|| {
        stems
            .iter()
            .map(|stem| strip_any_flavor(stem))
            .filter(|base| !base.is_empty())
            .min()
    });
    best.filter(|name| is_safe_folder_name(name))
        .map(str::to_string)
}

/// The game only loads a folder through a TOC named after it, so a source archive's
/// `Repo-main/Foo.toc` can only ever work as `Foo/`. `None` when a TOC already fits the
/// folder, or when several different names leave no way to tell.
fn folder_from_mismatched_tocs(folded_top: &str, toc_names: &[&str]) -> Option<String> {
    let stems: Vec<&str> = toc_names
        .iter()
        .map(|name| &name[..name.len() - 4])
        .collect();
    let fits = |stem: &&str| fold(stem) == folded_top || fold(strip_any_flavor(stem)) == folded_top;
    if stems.iter().any(fits) {
        return None;
    }
    let mut bases: Vec<&str> = stems.iter().map(|stem| strip_any_flavor(stem)).collect();
    bases.sort_unstable();
    bases.dedup();
    match bases.as_slice() {
        [only] if is_safe_folder_name(only) => Some((*only).to_string()),
        _ => None,
    }
}

fn strip_any_flavor(stem: &str) -> &str {
    if let Some(base) = TOC_SUFFIXES
        .iter()
        .filter(|suffix| !suffix.is_empty())
        .find_map(|suffix| strip_ignore_case(stem, suffix))
    {
        return base;
    }
    let Some(cut) = stem.rfind(['_', '-']) else {
        return stem;
    };
    if OTHER_FLAVORS
        .iter()
        .any(|flavor| stem[cut + 1..].eq_ignore_ascii_case(flavor))
    {
        &stem[..cut]
    } else {
        stem
    }
}

/// What to write, decided entirely from names.
#[derive(Debug, Default)]
struct Plan {
    items: Vec<PlanItem>,
    folders: Vec<String>,
    skipped: Vec<String>,
}

#[derive(Debug)]
struct PlanItem {
    index: usize,
    dest: String,
    is_dir: bool,
    size: u64,
    compressed_size: u64,
}

fn plan_addons(entries: &[ZipEntry], moves: &[Move], limits: &Limits) -> Result<Plan> {
    let live: Vec<&ZipEntry> = entries.iter().filter(|e| !is_junk_path(&e.path)).collect();
    let mapped = if moves.is_empty() {
        map_by_layout(&live)
    } else {
        map_by_moves(&live, moves)?
    };
    plan_from_mapped(mapped, true, false, limits)
}

fn plan_tree(entries: &[ZipEntry], options: TreeOptions, limits: &Limits) -> Result<Plan> {
    let mapped = entries
        .iter()
        .filter(|e| !is_junk_path(&e.path))
        .map(|e| (e, e.path.clone()))
        .collect();
    plan_from_mapped(mapped, false, options.allow_executables, limits)
}

/// Layout rules for a zip with no moves. Every rule keys off where the TOCs are.
fn map_by_layout<'a>(entries: &[&'a ZipEntry]) -> Vec<(&'a ZipEntry, String)> {
    let mut root_tocs: Vec<&str> = Vec::new();
    let mut addon_tops: HashMap<String, Vec<&str>> = HashMap::new();
    let mut wrapped: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let segments: Vec<&str> = entry.path.split('/').collect();
        match segments.as_slice() {
            [toc] if is_toc_name(toc) => root_tocs.push(toc),
            [top, toc] if is_toc_name(toc) => {
                addon_tops.entry(fold(top)).or_default().push(toc);
            }
            [wrapper, child, toc] if is_toc_name(toc) => {
                wrapped
                    .entry(fold(wrapper))
                    .or_default()
                    .insert(fold(child));
            }
            _ => {}
        }
    }
    let root_folder = folder_from_root_tocs(&root_tocs);
    let renamed_top = match addon_tops.iter().next() {
        Some((top, tocs)) if addon_tops.len() == 1 && root_folder.is_none() => {
            folder_from_mismatched_tocs(top, tocs)
        }
        _ => None,
    };
    // A real wrapper only exists when nothing else explains the zip's layout.
    let wrapper = if addon_tops.is_empty() && root_folder.is_none() && wrapped.len() == 1 {
        wrapped.iter().next()
    } else {
        None
    };

    let mut mapped = Vec::with_capacity(entries.len());
    for entry in entries {
        let (top, rest) = match entry.path.split_once('/') {
            Some((top, rest)) => (top, Some(rest)),
            None => (entry.path.as_str(), None),
        };
        let folded_top = fold(top);
        if addon_tops.contains_key(&folded_top) {
            let dest = match (&renamed_top, rest) {
                (Some(name), Some(rest)) => format!("{name}/{rest}"),
                (Some(name), None) => name.clone(),
                (None, _) => entry.path.clone(),
            };
            mapped.push((*entry, dest));
        } else if let Some(folder) = &root_folder {
            mapped.push((*entry, format!("{folder}/{}", entry.path)));
        } else if let (Some((wrapper_name, children)), Some(rest)) = (wrapper, rest)
            && folded_top == *wrapper_name
        {
            let child = rest.split('/').next().unwrap_or(rest);
            if children.contains(&fold(child)) {
                mapped.push((*entry, rest.to_string()));
            }
        }
    }
    mapped
}

struct ParsedMove<'m> {
    segments: Vec<&'m str>,
    /// The `from` length, which is what "longest wins" compares.
    length: usize,
    has_wildcard: bool,
    to: &'m str,
}

fn map_by_moves<'a>(
    entries: &[&'a ZipEntry],
    moves: &[Move],
) -> Result<Vec<(&'a ZipEntry, String)>> {
    let mut parsed = Vec::with_capacity(moves.len());
    for item in moves {
        // The catalog is data from the internet too. A bad `to` must not reach a rename.
        if !is_safe_folder_name(&item.to) {
            return Err(unsafe_zip(
                "its catalog entry tries to install outside the AddOns folder.",
            ));
        }
        let from = item.from.trim_matches('/');
        parsed.push(ParsedMove {
            segments: from.split('/').collect(),
            length: from.len(),
            has_wildcard: from == "*" || from.starts_with("*/"),
            to: &item.to,
        });
    }

    let mut mapped = Vec::with_capacity(entries.len());
    for entry in entries {
        let segments: Vec<&str> = entry.path.split('/').collect();
        let best = parsed
            .iter()
            .filter(|m| move_matches(m, &segments))
            .max_by_key(|m| (m.length, !m.has_wildcard));
        let Some(best) = best else { continue };
        let rest = segments[best.segments.len()..].join("/");
        if rest.is_empty() {
            if entry.is_dir {
                mapped.push((*entry, best.to.to_string()));
            }
        } else {
            mapped.push((*entry, format!("{}/{rest}", best.to)));
        }
    }
    Ok(mapped)
}

fn move_matches(item: &ParsedMove<'_>, segments: &[&str]) -> bool {
    item.segments.len() <= segments.len()
        && item
            .segments
            .iter()
            .zip(segments)
            .enumerate()
            .all(|(i, (want, have))| {
                (i == 0 && item.has_wildcard) || want.eq_ignore_ascii_case(have)
            })
}

/// Turn "this entry goes there" into the final plan: drop folders without a TOC when
/// that's required, set executables aside, and refuse anything that would collide.
fn plan_from_mapped(
    mapped: Vec<(&ZipEntry, String)>,
    require_toc: bool,
    allow_executables: bool,
    limits: &Limits,
) -> Result<Plan> {
    let is_held_back = |dest: &str, is_dir| !allow_executables && is_blocked_path(dest, is_dir);
    // Case-folded top-level name to its spelling.
    let mut folders: HashMap<String, String> = HashMap::new();
    for (entry, dest) in &mapped {
        if is_held_back(dest, entry.is_dir) {
            continue;
        }
        match dest.split_once('/') {
            Some((top, rest))
                if !require_toc || (!entry.is_dir && !rest.contains('/') && is_toc_name(rest)) =>
            {
                folders.entry(fold(top)).or_insert_with(|| top.to_string());
            }
            None if !require_toc => {
                folders.entry(fold(dest)).or_insert_with(|| dest.clone());
            }
            _ => {}
        }
    }
    if require_toc && folders.is_empty() {
        return Err(Error::NoAddonInZip);
    }

    let mut plan = Plan::default();
    for (entry, dest) in mapped {
        let top = dest.split('/').next().unwrap_or(&dest);
        let Some(spelling) = folders.get(&fold(top)) else {
            continue;
        };
        if spelling != top {
            return Err(unsafe_zip(format!(
                "it has two folders that differ only by capital letters ({spelling} and {top})."
            )));
        }
        if is_held_back(&dest, entry.is_dir) {
            log::warn!("Skipped an executable in a zip: {}", printable(&dest));
            plan.skipped.push(dest);
            continue;
        }
        plan.items.push(PlanItem {
            index: entry.index,
            dest,
            is_dir: entry.is_dir,
            size: entry.size,
            compressed_size: entry.compressed_size,
        });
    }
    check_names(&plan.items, limits)?;
    plan.folders = folders.into_values().collect();
    plan.folders.sort();
    plan.skipped.sort();
    Ok(plan)
}

/// Refuse a plan with too many folders, or whose names would collide. Everything here
/// borrows whole paths: a set owning a copy of every parent of every path is how a 1 MB
/// zip could once ask for all of a machine's memory before any limit got a look at it.
fn check_names(items: &[PlanItem], limits: &Limits) -> Result<()> {
    // As spelled, because a case-sensitive disk makes a folder for each spelling.
    let mut folders: HashSet<&str> = HashSet::new();
    for item in items {
        note_folders(&mut folders, &item.dest, item.is_dir);
        if folders.len() > limits.max_folders {
            return Err(unsafe_zip(format!(
                "it would create more than {} folders.",
                limits.max_folders
            )));
        }
    }
    let folded: Vec<String> = items.iter().map(|item| fold(&item.dest)).collect();
    let mut seen_files: HashSet<&str> = HashSet::new();
    let mut seen_dirs: HashSet<&str> = HashSet::new();
    for (item, path) in items.iter().zip(&folded) {
        note_folders(&mut seen_dirs, path, item.is_dir);
        if !item.is_dir && !seen_files.insert(path) {
            return Err(unsafe_zip(format!(
                "two of its files would land on the same name ({}).",
                printable(&item.dest)
            )));
        }
    }
    if let Some(clash) = seen_files.iter().find(|path| seen_dirs.contains(*path)) {
        return Err(unsafe_zip(format!(
            "the same name is used for a file and a folder ({}).",
            printable(clash)
        )));
    }
    Ok(())
}

/// Add the folder `path` is, or is in, and the ones above it. Stops at the first one it
/// already knows, because whatever added that one added its parents too.
fn note_folders<'a>(folders: &mut HashSet<&'a str>, path: &'a str, is_dir: bool) {
    let parent = |path: &'a str| path.rsplit_once('/').map(|(parent, _)| parent);
    let mut folder = if is_dir { Some(path) } else { parent(path) };
    while let Some(current) = folder.filter(|current| folders.insert(current)) {
        folder = parent(current);
    }
}

/// Write the plan, or leave nothing behind.
#[allow(clippy::too_many_arguments)]
fn run_plan<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    zip_len: u64,
    plan: &Plan,
    dest_dir: &Path,
    limits: Limits,
    keep_exec_bits: bool,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Extracted> {
    let dest = long_path(dest_dir);
    let created_dest = !dest.exists();
    fs::create_dir_all(&dest).map_err(|e| io_error(&dest, e))?;
    // Only folders this call created get cleaned up on failure, so refuse to mix with
    // anything already there rather than risk deleting it.
    for folder in &plan.folders {
        if dest.join(folder).exists() {
            return Err(Error::Other(format!(
                "Couldn't unpack because {} already exists in {}.",
                folder,
                display_path(&dest).display()
            )));
        }
    }
    let written = write_items(
        archive,
        zip_len,
        plan,
        &dest,
        limits,
        keep_exec_bits,
        progress,
        cancel,
    );
    match written {
        Ok(bytes) => Ok(Extracted {
            folders: plan.folders.clone(),
            skipped_files: plan.skipped.clone(),
            bytes,
        }),
        Err(error) => {
            for folder in &plan.folders {
                let path = dest.join(folder);
                if fs::remove_dir_all(&path).is_err() {
                    let _ = fs::remove_file(&path);
                }
            }
            if created_dest {
                let _ = fs::remove_dir(&dest);
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_items<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    zip_len: u64,
    plan: &Plan,
    dest: &Path,
    limits: Limits,
    keep_exec_bits: bool,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<u64> {
    let mut declared_total = 0u64;
    for item in plan.items.iter().filter(|item| !item.is_dir) {
        if item.size > limits.max_file_bytes {
            return Err(too_big_file(limits));
        }
        if over_ratio(item.size, item.compressed_size, limits) {
            return Err(zip_bomb(limits));
        }
        declared_total = declared_total.saturating_add(item.size);
    }
    if declared_total > limits.max_total_bytes {
        return Err(too_big_total(limits));
    }
    // Declared sizes can't be trusted to be honest, but they do bound the output
    // (a stream running past its own declaration is an error), so a bomb that admits
    // what it is can be turned away before it writes anything.
    if over_ratio(declared_total, zip_len, limits) {
        return Err(zip_bomb(limits));
    }

    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut made_dirs: HashSet<PathBuf> = HashSet::new();
    let mut total = 0u64;
    let mut last_report = Instant::now();
    progress(Progress {
        done: 0,
        total: Some(declared_total),
    });
    for item in &plan.items {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut path = dest.to_path_buf();
        path.extend(item.dest.split('/'));
        if item.is_dir {
            make_dirs(&path, &mut made_dirs)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            make_dirs(parent, &mut made_dirs)?;
        }
        let mut entry = archive
            .by_index(item.index)
            .map_err(|e| zip_error(e, &item.dest))?;
        let is_executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                // Case and Unicode folding beyond what we can predict land here.
                io::ErrorKind::AlreadyExists => unsafe_zip(format!(
                    "two of its files would land on the same name ({}).",
                    printable(&item.dest)
                )),
                _ => io_error(&path, e),
            })?;
        // Declared sizes are lies until proven otherwise. The one extra byte is how
        // an entry that runs over gets noticed.
        let mut reader = (&mut entry).take(limits.max_file_bytes.saturating_add(1));
        let mut file_bytes = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(e, &item.dest)),
            };
            file_bytes += count as u64;
            total += count as u64;
            if file_bytes > limits.max_file_bytes {
                return Err(too_big_file(limits));
            }
            if total > limits.max_total_bytes {
                return Err(too_big_total(limits));
            }
            if over_ratio(file_bytes, item.compressed_size, limits)
                || over_ratio(total, zip_len, limits)
            {
                return Err(zip_bomb(limits));
            }
            file.write_all(&buffer[..count])
                .map_err(|e| io_error(&path, e))?;
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                progress(Progress {
                    done: total,
                    total: Some(declared_total),
                });
                last_report = Instant::now();
            }
        }
        drop(file);
        if keep_exec_bits && is_executable {
            mark_executable(&path).map_err(|e| io_error(&path, e))?;
        }
    }
    progress(Progress {
        done: total,
        total: Some(declared_total),
    });
    Ok(total)
}

fn make_dirs(path: &Path, made: &mut HashSet<PathBuf>) -> Result<()> {
    if !made.contains(path) {
        fs::create_dir_all(path).map_err(|e| io_error(path, e))?;
        made.insert(path.to_path_buf());
    }
    Ok(())
}

fn over_ratio(written: u64, compressed: u64, limits: Limits) -> bool {
    written > limits.ratio_floor_bytes && written / compressed.max(1) > limits.max_ratio
}

fn read_error(error: io::Error, name: &str) -> Error {
    Error::Corrupt(format!("{} couldn't be read ({error})", printable(name)))
}

fn too_many_files(count: u64, limits: &Limits) -> Error {
    unsafe_zip(format!(
        "it holds {count} files, more than the {} allowed.",
        limits.max_entries
    ))
}

fn zip_bomb(limits: Limits) -> Error {
    unsafe_zip(format!(
        "it expands to over {} times its size, which is how a zip bomb behaves.",
        limits.max_ratio
    ))
}

fn too_big_file(limits: Limits) -> Error {
    unsafe_zip(format!(
        "one of its files is over the {} MB limit.",
        limits.max_file_bytes / MB
    ))
}

fn too_big_total(limits: Limits) -> Error {
    unsafe_zip(format!(
        "it unpacks to over the {} MB limit.",
        limits.max_total_bytes / MB
    ))
}

#[cfg(unix)]
pub(crate) fn mark_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
pub(crate) fn mark_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_zips {
    use super::*;
    use std::io::Cursor;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    /// Build a normal zip with the crate's writer. Names ending in `/` become folders.
    pub fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(true);
        for (name, body) in files {
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    /// An entry with whatever name and attributes a test wants, stored uncompressed.
    pub struct RawEntry {
        pub name: Vec<u8>,
        pub data: Vec<u8>,
        pub unix_mode: Option<u32>,
        pub declared_size: Option<u32>,
        pub crc_override: Option<u32>,
        pub method: u16,
        pub flags: u16,
    }

    impl RawEntry {
        pub fn new(name: impl AsRef<[u8]>, data: &[u8]) -> Self {
            RawEntry {
                name: name.as_ref().to_vec(),
                data: data.to_vec(),
                unix_mode: None,
                declared_size: None,
                crc_override: None,
                method: 0,
                flags: 0x0800,
            }
        }
    }

    pub fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Hand-assemble a zip, for names and attributes no writer API would allow.
    pub fn raw_zip(entries: &[RawEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for entry in entries {
            let offset = out.len() as u32;
            let crc = entry.crc_override.unwrap_or_else(|| crc32(&entry.data));
            let compressed = entry.data.len() as u32;
            let size = entry.declared_size.unwrap_or(compressed);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&entry.flags.to_le_bytes());
            out.extend_from_slice(&entry.method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0x21, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&compressed.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&entry.name);
            out.extend_from_slice(&entry.data);

            let made_by: u16 = if entry.unix_mode.is_some() {
                (3 << 8) | 20
            } else {
                20
            };
            let attributes = entry.unix_mode.map_or(0, |mode| mode << 16);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&made_by.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&entry.flags.to_le_bytes());
            central.extend_from_slice(&entry.method.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0x21, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&compressed.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 8]);
            central.extend_from_slice(&attributes.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(&entry.name);
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_zips::{RawEntry, crc32, raw_zip, zip_bytes};
    use super::*;
    use std::collections::BTreeMap;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    const TOC: &[u8] = b"## Interface: 16001\n## Title: Test\n";

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn zip(&self, bytes: &[u8]) -> PathBuf {
            let path = self.dir.path().join("test.zip");
            fs::write(&path, bytes).unwrap();
            path
        }

        fn dest(&self) -> PathBuf {
            self.dir.path().join("out")
        }

        fn extract(&self, bytes: &[u8], moves: &[Move]) -> Result<Extracted> {
            let zip = self.zip(bytes);
            extract(
                &zip,
                &self.dest(),
                moves,
                Limits::default(),
                &mut |_| {},
                &AtomicBool::new(false),
            )
        }

        fn extract_limited(&self, bytes: &[u8], limits: Limits) -> Result<Extracted> {
            let zip = self.zip(bytes);
            extract(
                &zip,
                &self.dest(),
                &[],
                limits,
                &mut |_| {},
                &AtomicBool::new(false),
            )
        }

        /// Every file written, as `path -> contents`.
        fn tree(&self) -> BTreeMap<String, Vec<u8>> {
            let mut out = BTreeMap::new();
            let dest = self.dest();
            if dest.exists() {
                walk(&dest, "", &mut out);
            }
            out
        }
    }

    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for item in fs::read_dir(dir).unwrap() {
            let item = item.unwrap();
            let name = item.file_name().to_string_lossy().into_owned();
            let path = format!("{prefix}{name}");
            if item.file_type().unwrap().is_dir() {
                walk(&item.path(), &format!("{path}/"), out);
            } else {
                out.insert(path, fs::read(item.path()).unwrap());
            }
        }
    }

    fn paths(fixture: &Fixture) -> Vec<String> {
        fixture.tree().into_keys().collect()
    }

    /// A valid first file, then the evil one, so an extractor that writes as it goes
    /// would leave something behind.
    fn assert_rejected(evil: impl AsRef<[u8]>) {
        let evil = evil.as_ref();
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Good/Good.toc", TOC),
            RawEntry::new("Good/core.lua", b"print()"),
            RawEntry::new(evil, b"payload"),
        ]);
        let result = fixture.extract(&bytes, &[]);
        let shown = String::from_utf8_lossy(evil);
        assert!(
            matches!(result, Err(Error::UnsafeZip(_))),
            "{shown:?} gave {result:?}"
        );
        assert!(fixture.tree().is_empty(), "{shown:?} wrote files");
    }

    #[test]
    fn path_traversal_is_rejected() {
        for name in [
            "../evil.txt",
            "../../../../evil.txt",
            "Good/../../evil.txt",
            "Good/../evil.txt",
            "..\\evil.txt",
            "Good\\..\\..\\evil.txt",
            "Good/..\\..\\evil.txt",
            "..",
            "Good/..",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn absolute_paths_and_drive_letters_are_rejected() {
        for name in [
            "/evil.txt",
            "\\evil.txt",
            "//server/share/evil.txt",
            "\\\\server\\share\\evil.txt",
            "C:/evil.txt",
            "C:\\evil.txt",
            "C:evil.txt",
            "Good/C:/evil.txt",
            "\\\\?\\C:\\evil.txt",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn nul_bytes_are_rejected() {
        assert_rejected(b"Good/evil\0.lua".as_slice());
        assert_rejected(b"Good/evil.lua\0.exe".as_slice());
    }

    #[test]
    fn colons_are_rejected_anywhere() {
        for name in [
            "Good/file.lua:hidden",
            "Good/file.lua::$DATA",
            "Good:stream/file.lua",
            "a:b",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn device_names_are_rejected_in_any_segment_with_or_without_extension() {
        for name in [
            "Good/CON",
            "Good/con",
            "Good/Con.lua",
            "Good/PRN",
            "Good/prn.txt",
            "Good/AUX",
            "Good/aux.tar.gz",
            "Good/NUL",
            "Good/nul.lua",
            "Good/COM1",
            "Good/com5.txt",
            "Good/COM9",
            "Good/LPT1",
            "Good/lpt4.log",
            "Good/LPT9",
            "CON/file.lua",
            "Good/aux/file.lua",
            "Good/nul.x/file.lua",
            "Good/CONIN$",
            "Good/COM¹",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn near_misses_of_device_names_are_fine() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Good/Good.toc", TOC),
            RawEntry::new("Good/console.lua", b"1"),
            RawEntry::new("Good/COM10.lua", b"2"),
            RawEntry::new("Good/auxiliary.lua", b"3"),
            RawEntry::new("Good/nul_handler.lua", b"4"),
            RawEntry::new("Good/lpt.lua", b"5"),
            RawEntry::new("Good/my.con.lua", b"6"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Good"]);
        assert_eq!(fixture.tree().len(), 7);
    }

    #[test]
    fn trailing_dots_and_spaces_are_rejected() {
        for name in [
            "Good/file.",
            "Good/file ",
            "Good./file.lua",
            "Good /file.lua",
            "Good/...",
            "Good/file. ",
            "Good/ . ",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn names_windows_cannot_store_are_rejected() {
        for name in [
            "Good/a<b",
            "Good/a>b",
            "Good/a\"b",
            "Good/a|b",
            "Good/a?b",
            "Good/a*b",
            "Good/tab\there",
            "Good/line\nbreak",
            "Good/del\u{7f}",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn overlong_segments_are_rejected() {
        assert_rejected(format!("Good/{}.lua", "x".repeat(300)));
    }

    #[test]
    fn paths_too_long_or_too_deep_are_refused() {
        let levels = "a/".repeat(MAX_ENTRY_DEPTH);
        let too_long = format!("Good/{}x.lua", "a-long-name-for-a-folder/".repeat(50));
        assert!(too_long.len() > MAX_ENTRY_NAME_BYTES);
        for name in [
            too_long,
            format!("Good/{levels}x.lua"),
            format!("Good\\{}x.lua", levels.replace('/', "\\")),
            // These clean away to nothing, but they still have to be walked to find that out.
            format!("Good/{}x.lua", "./".repeat(MAX_ENTRY_DEPTH)),
            format!("Good/{}x.lua", "/".repeat(MAX_ENTRY_DEPTH)),
            // The size that made planning ask for a gigabyte per name.
            format!("Good/{}x.lua", "a/".repeat(32_500)),
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn paths_at_the_limits_are_fine() {
        // Just under the depth limit, and written for real.
        let name = format!("Foo/{}x.lua", "a/".repeat(MAX_ENTRY_DEPTH - 1));
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new(&name, b"deep"),
        ]);
        fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(fixture.tree()[&name], b"deep");
        // macOS can't write a path this long once the temp folder is in front of it.
        let long = format!(
            "Foo/{}{}",
            (format!("{}/", "x".repeat(200))).repeat(4),
            "y".repeat(216)
        );
        assert_eq!(long.len(), MAX_ENTRY_NAME_BYTES);
        assert_eq!(clean_entry_path(&long).unwrap(), long);
    }

    #[test]
    fn the_folders_a_zip_would_create_count_even_when_it_does_not_list_them() {
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/a/b/c/d/x.lua", b"1"),
        ]);
        let limited = |max_folders| Limits {
            max_folders,
            ..Limits::default()
        };
        let fixture = Fixture::new();
        let result = fixture.extract_limited(&bytes, limited(4));
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("4 folders")),
            "{result:?}"
        );
        assert!(fixture.tree().is_empty());
        fixture.extract_limited(&bytes, limited(5)).unwrap();
    }

    #[test]
    fn empty_folders_count_too() {
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/a/", b""),
            RawEntry::new("Foo/b/", b""),
        ]);
        let limits = Limits {
            max_folders: 2,
            ..Limits::default()
        };
        let result = Fixture::new().extract_limited(&bytes, limits);
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("2 folders")),
            "{result:?}"
        );
    }

    /// `seen_dirs` folds case, but a case-sensitive disk makes a folder for each spelling,
    /// and 12 levels of two spellings each come to thousands of folders.
    #[test]
    fn every_spelling_of_a_folder_counts() {
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/a/1.lua", b"1"),
            RawEntry::new("Foo/A/2.lua", b"2"),
        ]);
        let limited = |max_folders| Limits {
            max_folders,
            ..Limits::default()
        };
        assert!(matches!(
            Fixture::new().extract_limited(&bytes, limited(2)),
            Err(Error::UnsafeZip(_))
        ));
        Fixture::new().extract_limited(&bytes, limited(3)).unwrap();
    }

    /// Counts what each thread has on the heap, so a test can hold planning to a budget
    /// without depending on timing or on what else the machine is doing.
    mod heap {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static LIVE: Cell<usize> = const { Cell::new(0) };
            static PEAK: Cell<usize> = const { Cell::new(0) };
        }

        fn note(grew: usize, shrank: usize) {
            let _ = LIVE.try_with(|live| {
                let now = live.get().saturating_add(grew).saturating_sub(shrank);
                live.set(now);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
            });
        }

        struct Counting;

        // Only adds counting, and only in this crate's own test build.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                note(layout.size(), 0);
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                note(layout.size(), 0);
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                note(0, layout.size());
                unsafe { System.dealloc(pointer, layout) }
            }

            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                note(size, layout.size());
                unsafe { System.realloc(pointer, layout, size) }
            }
        }

        #[global_allocator]
        static COUNTING: Counting = Counting;

        /// How far this thread's heap grew while `run` ran.
        pub fn peak_growth<T>(run: impl FnOnce() -> T) -> (T, usize) {
            let start = LIVE.with(Cell::get);
            PEAK.with(|peak| peak.set(start));
            let result = run();
            (result, PEAK.with(Cell::get).saturating_sub(start))
        }
    }

    /// One of these names has 60 parents, about 30 KB of text between them, which is what
    /// planning used to keep for each. Its own bytes are a fraction of that.
    #[test]
    fn planning_keeps_no_copy_of_each_parent_folder() {
        let deep = vec!["level-of-a-path"; 60].join("/");
        let entries: Vec<ZipEntry> = (0..2000)
            .map(|index| ZipEntry {
                index,
                path: format!("Foo/{index}/{deep}/x.lua"),
                is_dir: false,
                size: 1,
                compressed_size: 1,
            })
            .collect();
        let names: usize = entries.iter().map(|entry| entry.path.len()).sum();
        let limits = Limits {
            max_folders: usize::MAX,
            ..Limits::default()
        };
        let (plan, grew) =
            heap::peak_growth(|| plan_tree(&entries, TreeOptions::default(), &limits));
        assert_eq!(plan.unwrap().items.len(), entries.len());
        assert!(
            grew < names * 12,
            "planning {names} bytes of names took {grew} bytes"
        );
    }

    #[test]
    fn symlink_entries_are_rejected() {
        let fixture = Fixture::new();
        let mut writer = ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        writer.start_file("Good/Good.toc", options).unwrap();
        writer.write_all(TOC).unwrap();
        writer
            .add_symlink("Good/link", "../../../Windows/System32", options)
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
        assert!(fixture.tree().is_empty());

        // Same thing, from a zip no writer API would make: hidden in a dropped folder.
        let mut link = RawEntry::new("__MACOSX/link", b"/etc/passwd");
        link.unix_mode = Some(0o120777);
        let bytes = raw_zip(&[RawEntry::new("Good/Good.toc", TOC), link]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::UnsafeZip(_))
        ));
    }

    #[test]
    fn special_files_are_rejected() {
        for mode in [0o010644u32, 0o020644, 0o060644, 0o140644] {
            let fixture = Fixture::new();
            let mut special = RawEntry::new("Good/special", b"");
            special.unix_mode = Some(mode);
            let bytes = raw_zip(&[RawEntry::new("Good/Good.toc", TOC), special]);
            let result = fixture.extract(&bytes, &[]);
            assert!(
                matches!(result, Err(Error::UnsafeZip(_))),
                "{mode:o}: {result:?}"
            );
        }
    }

    #[test]
    fn regular_unix_files_and_folders_are_fine() {
        let fixture = Fixture::new();
        let mut toc = RawEntry::new("Good/Good.toc", TOC);
        toc.unix_mode = Some(0o100644);
        let mut dir = RawEntry::new("Good/Libs/", b"");
        dir.unix_mode = Some(0o040755);
        fixture.extract(&raw_zip(&[toc, dir]), &[]).unwrap();
        assert_eq!(paths(&fixture), ["Good/Good.toc"]);
        assert!(fixture.dest().join("Good/Libs").is_dir());
    }

    #[test]
    fn case_only_duplicates_are_rejected() {
        for files in [
            ["Foo/Foo.toc", "Foo/a.lua", "Foo/A.lua"],
            ["Foo/Foo.toc", "Foo/Libs/a.lua", "Foo/LIBS/a.lua"],
            ["Foo/Foo.toc", "foo/foo.toc", "foo/x.lua"],
            ["Foo/Foo.toc", "foo/x.lua", "Foo/x.lua"],
        ] {
            let fixture = Fixture::new();
            let entries: Vec<RawEntry> = files.iter().map(|n| RawEntry::new(n, TOC)).collect();
            let result = fixture.extract(&raw_zip(&entries), &[]);
            assert!(
                matches!(result, Err(Error::UnsafeZip(_))),
                "{files:?}: {result:?}"
            );
            assert!(fixture.tree().is_empty());
        }
    }

    #[test]
    fn case_duplicate_top_level_folders_are_rejected() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("foo/other.toc", TOC),
        ]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::UnsafeZip(_))
        ));
    }

    #[test]
    fn unicode_case_duplicates_are_rejected() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/ÉCLAIR.lua", b"1"),
            RawEntry::new("Foo/éclair.lua", b"2"),
        ]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::UnsafeZip(_))
        ));
    }

    #[test]
    fn a_dropped_folder_may_hold_case_duplicates() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("docs/Readme.md", b"1"),
            RawEntry::new("docs/README.md", b"2"),
        ]);
        assert_eq!(fixture.extract(&bytes, &[]).unwrap().folders, ["Foo"]);
    }

    /// Git keeps both spellings when a folder's case changed on Windows. They're one
    /// folder to the game, so they merge rather than fail an otherwise fine addon.
    #[test]
    fn folders_differing_only_by_case_merge_when_no_file_collides() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/Libs/a.lua", b"1"),
            RawEntry::new("Foo/libs/b.lua", b"2"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Foo"]);
        assert_eq!(fixture.tree().len(), 3);
    }

    #[test]
    fn exact_duplicate_names_are_rejected() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/a.lua", b"clean"),
            RawEntry::new("Foo/a.lua", b"hidden"),
        ]);
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
    }

    #[test]
    fn a_name_used_as_both_file_and_folder_is_rejected() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/thing", b"file"),
            RawEntry::new("Foo/THING/inner.lua", b"2"),
        ]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::UnsafeZip(_))
        ));
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn backslash_separated_zips_extract_normally() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo\\Foo.toc", TOC),
            RawEntry::new("Foo\\Libs\\a.lua", b"1"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Foo"]);
        assert_eq!(paths(&fixture), ["Foo/Foo.toc", "Foo/Libs/a.lua"]);
    }

    #[test]
    fn dot_segments_are_normalized() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("./Foo/Foo.toc", TOC),
            RawEntry::new("Foo/./a//b.lua", b"1"),
        ]);
        fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(paths(&fixture), ["Foo/Foo.toc", "Foo/a/b.lua"]);
    }

    #[test]
    fn zeros_past_the_per_file_limit_are_rejected() {
        let fixture = Fixture::new();
        let bytes = zip_bytes(&[("Foo/Foo.toc", TOC), ("Foo/big.bin", &[0u8; 2000])]);
        let limits = Limits {
            max_file_bytes: 1000,
            ..Limits::default()
        };
        let result = fixture.extract_limited(&bytes, limits);
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn the_total_limit_is_enforced() {
        let fixture = Fixture::new();
        let bytes = zip_bytes(&[
            ("Foo/Foo.toc", TOC),
            ("Foo/a.bin", &[0u8; 800]),
            ("Foo/b.bin", &[0u8; 800]),
        ]);
        let limits = Limits {
            max_total_bytes: 1000,
            ..Limits::default()
        };
        assert!(matches!(
            fixture.extract_limited(&bytes, limits),
            Err(Error::UnsafeZip(_))
        ));
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn the_entry_limit_is_enforced() {
        let fixture = Fixture::new();
        let bytes = zip_bytes(&[("Foo/Foo.toc", TOC), ("Foo/a", b"1"), ("Foo/b", b"2")]);
        let limits = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        assert!(matches!(
            fixture.extract_limited(&bytes, limits),
            Err(Error::UnsafeZip(_))
        ));
        let limits = Limits {
            max_entries: 3,
            ..Limits::default()
        };
        assert!(fixture.extract_limited(&bytes, limits).is_ok());
    }

    #[test]
    fn the_default_limits_stop_a_real_bomb_before_writing_it() {
        let fixture = Fixture::new();
        let bytes = zip_bytes(&[
            ("Foo/Foo.toc", TOC),
            ("Foo/bomb.bin", &vec![0u8; 80 * MB as usize]),
        ]);
        assert!((bytes.len() as u64) < 100 * KB);
        let result = fixture.extract(&bytes, &[]);
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("zip bomb")),
            "{result:?}"
        );
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn the_ratio_limit_ignores_small_files_and_normal_ratios() {
        let fixture = Fixture::new();
        let bytes = zip_bytes(&[("Foo/Foo.toc", TOC), ("Foo/zeros.bin", &[0u8; 300_000])]);
        // A silly ratio, but well under the floor.
        fixture.extract(&bytes, &[]).unwrap();

        let fixture = Fixture::new();
        let limits = Limits {
            ratio_floor_bytes: 1000,
            max_ratio: 10,
            ..Limits::default()
        };
        let result = fixture.extract_limited(&bytes, limits);
        assert!(matches!(result, Err(Error::UnsafeZip(_))));
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn a_file_longer_than_it_declares_is_rejected() {
        let fixture = Fixture::new();
        let mut liar = RawEntry::new("Foo/liar.bin", &[7u8; 5000]);
        liar.declared_size = Some(10);
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), liar]);
        let result = fixture.extract(&bytes, &[]);
        assert!(result.is_err(), "{result:?}");
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn a_declared_size_over_the_limit_is_rejected_before_writing() {
        let fixture = Fixture::new();
        let mut liar = RawEntry::new("Foo/z.bin", b"tiny");
        liar.declared_size = Some(u32::MAX);
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), liar]);
        let limits = Limits {
            max_file_bytes: 1000,
            ..Limits::default()
        };
        assert!(matches!(
            fixture.extract_limited(&bytes, limits),
            Err(Error::UnsafeZip(_))
        ));
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn entries_sharing_compressed_data_are_rejected() {
        // Two central directory records pointing at one local header: the shape of a
        // non-recursive zip bomb, in miniature.
        let mut bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/a.lua", &[1u8; 100]),
        ]);
        let eocd = bytes.len() - 22;
        let central_size =
            u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
        let central_start = eocd - central_size;
        let toc_record_len = 46 + "Foo/Foo.toc".len();
        let mut second = bytes[central_start + toc_record_len..eocd].to_vec();
        let name_at = 46;
        second[name_at..name_at + 9].copy_from_slice(b"Foo/b.lua");
        let mut tail = bytes.split_off(eocd);
        bytes.extend_from_slice(&second);
        tail[8..10].copy_from_slice(&3u16.to_le_bytes());
        tail[10..12].copy_from_slice(&3u16.to_le_bytes());
        tail[12..16].copy_from_slice(&((central_size + second.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&tail);

        let fixture = Fixture::new();
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn a_bad_checksum_is_corrupt_and_leaves_nothing() {
        let fixture = Fixture::new();
        let mut damaged = RawEntry::new("Foo/z.lua", b"contents");
        damaged.crc_override = Some(0xDEAD_BEEF);
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), damaged]);
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
        assert!(fixture.tree().is_empty());
        assert!(!fixture.dest().exists());
    }

    #[test]
    fn garbage_and_truncated_files_are_corrupt() {
        let fixture = Fixture::new();
        assert!(matches!(
            fixture.extract(b"this is not a zip", &[]),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(fixture.extract(b"", &[]), Err(Error::Corrupt(_))));
        let bytes = zip_bytes(&[("Foo/Foo.toc", TOC), ("Foo/a.lua", &[1u8; 5000])]);
        let result = fixture.extract(&bytes[..bytes.len() / 2], &[]);
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }

    #[test]
    fn rar_and_7z_files_say_what_they_are() {
        for (magic, kind) in [
            (&b"Rar!\x1a\x07\x00\xcf"[..], "RAR"),
            (b"7z\xbc\xaf\x27\x1c\x00\x04", "7-Zip"),
        ] {
            let fixture = Fixture::new();
            let result = fixture.extract(magic, &[]);
            let text = result.unwrap_err().to_string();
            assert!(text.contains(kind) && !text.contains("Try again"), "{text}");
        }
    }

    #[test]
    fn a_self_extracting_program_says_what_it_is() {
        let mut bytes = b"MZ this would be a program".to_vec();
        bytes.extend(raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC)]));
        let text = Fixture::new().extract(&bytes, &[]).unwrap_err().to_string();
        assert!(
            text.contains("self-extracting") && !text.contains("Try again"),
            "{text}"
        );
    }

    /// The catalog scan reads a zip with `Known(0)`, so anything with data in front of it
    /// looks broken there. The installer has to agree, or the two see different things.
    #[test]
    fn data_in_front_of_a_zip_is_refused() {
        let fixture = Fixture::new();
        let mut bytes = b"anything at all".to_vec();
        bytes.extend(raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC)]));
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
        assert!(fixture.tree().is_empty());
    }

    /// One file in a hand-made central record, `stored_offset` from the zip's start.
    fn central_record(name: &str, data: &[u8], stored_offset: u32) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        record.extend_from_slice(&20u16.to_le_bytes());
        record.extend_from_slice(&20u16.to_le_bytes());
        record.extend_from_slice(&0x0800u16.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&[0, 0, 0x21, 0]);
        record.extend_from_slice(&crc32(data).to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(name.len() as u16).to_le_bytes());
        record.extend_from_slice(&[0u8; 12]);
        record.extend_from_slice(&stored_offset.to_le_bytes());
        record.extend_from_slice(name.as_bytes());
        record
    }

    fn local_record(name: &str, data: &[u8]) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        record.extend_from_slice(&20u16.to_le_bytes());
        record.extend_from_slice(&0x0800u16.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&[0, 0, 0x21, 0]);
        record.extend_from_slice(&crc32(data).to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(name.len() as u16).to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(name.as_bytes());
        record.extend_from_slice(data);
        record
    }

    fn end_record(count: u16, list_size: u32, list_offset: u32) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        record.extend_from_slice(&[0u8; 4]);
        record.extend_from_slice(&count.to_le_bytes());
        record.extend_from_slice(&count.to_le_bytes());
        record.extend_from_slice(&list_size.to_le_bytes());
        record.extend_from_slice(&list_offset.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record
    }

    /// The end record's offset names a list holding `Harmless`, and its size implies a second
    /// list holding `Bartender4`. A reader that prefers the size installs what the catalog
    /// never saw.
    #[test]
    fn a_zip_with_two_file_lists_installs_the_one_the_catalog_read() {
        let harmless = local_record("Harmless/Harmless.toc", TOC);
        let shown_list = central_record("Harmless/Harmless.toc", TOC, 0);
        // Offsets in the hidden list get the distance between the two lists added to them.
        let padding = vec![0u8; shown_list.len()];
        let hidden_at = (harmless.len() + padding.len()) as u32;
        let hidden = local_record("Bartender4/Bartender4.toc", TOC);
        let hidden_list = central_record(
            "Bartender4/Bartender4.toc",
            TOC,
            hidden_at - shown_list.len() as u32,
        );

        let mut bytes = harmless;
        bytes.extend_from_slice(&padding);
        bytes.extend_from_slice(&hidden);
        let shown_at = bytes.len() as u32;
        bytes.extend_from_slice(&shown_list);
        bytes.extend_from_slice(&hidden_list);
        bytes.extend(end_record(1, hidden_list.len() as u32, shown_at));

        let fixture = Fixture::new();
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Harmless"]);
        assert_eq!(paths(&fixture), ["Harmless/Harmless.toc"]);
    }

    #[test]
    fn a_file_list_over_the_read_limit_is_refused() {
        let entries: Vec<RawEntry> = (0..2000)
            .map(|i| RawEntry::new(format!("Foo/file-{i:04}.lua"), b"1"))
            .collect();
        let limits = Limits {
            max_directory_bytes: 32 * KB,
            ..Limits::default()
        };
        let fixture = Fixture::new();
        let result = fixture.extract_limited(&raw_zip(&entries), limits);
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("list of files")),
            "{result:?}"
        );
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn the_read_limit_stops_counting_once_the_list_is_read() {
        let big = vec![7u8; 512 * KB as usize];
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/big.bin", &big),
        ]);
        let limits = Limits {
            max_directory_bytes: 128 * KB,
            ..Limits::default()
        };
        let fixture = Fixture::new();
        fixture.extract_limited(&bytes, limits).unwrap();
        assert_eq!(fixture.tree()["Foo/big.bin"], big);
    }

    /// A zip's end as zip64 writes it: the record, the locator, and a plain end record whose
    /// own fields are all marked "see the zip64 record".
    fn zip64_end(claimed: u64, list_size: u64, list_at: u64, record_at: u64) -> Vec<u8> {
        let mut end = Vec::new();
        end.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        end.extend_from_slice(&44u64.to_le_bytes());
        end.extend_from_slice(&45u16.to_le_bytes());
        end.extend_from_slice(&45u16.to_le_bytes());
        end.extend_from_slice(&[0u8; 8]);
        end.extend_from_slice(&claimed.to_le_bytes());
        end.extend_from_slice(&claimed.to_le_bytes());
        end.extend_from_slice(&list_size.to_le_bytes());
        end.extend_from_slice(&list_at.to_le_bytes());
        end.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        end.extend_from_slice(&[0u8; 4]);
        end.extend_from_slice(&record_at.to_le_bytes());
        end.extend_from_slice(&1u32.to_le_bytes());
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&[0u8; 4]);
        end.extend_from_slice(&[0xFF; 12]);
        end.extend_from_slice(&0u16.to_le_bytes());
        end
    }

    /// Turn a plain zip's end record into zip64, keeping what it says.
    fn as_zip64(mut bytes: Vec<u8>, files: u64) -> Vec<u8> {
        let eocd = bytes.len() - 22;
        let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let (list_size, list_at) = (field(eocd + 12), field(eocd + 16));
        bytes.truncate(eocd);
        let end = zip64_end(files, list_size.into(), list_at.into(), eocd as u64);
        bytes.extend(end);
        bytes
    }

    #[test]
    fn a_zip64_count_over_the_limit_is_refused_before_the_list_is_read() {
        // Enough file for the crate's own check that this many could fit.
        let padding = vec![0u8; 5000 * 46];
        let toc = RawEntry::new("Foo/Foo.toc", TOC);
        let lying = as_zip64(raw_zip(&[toc, RawEntry::new("Foo/pad", &padding)]), 5000);
        let limits = Limits {
            max_entries: 1000,
            ..Limits::default()
        };
        // Without the check, the crate sets aside room for 5,000 and then finds the list
        // damaged, which would say nothing about that room.
        let fixture = Fixture::new();
        let result = fixture.extract_limited(&lying, limits);
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("5000 files")),
            "{result:?}"
        );

        // A harmless end marker after the real one doesn't hide it.
        let mut hidden = lying;
        hidden.extend(end_record(1, 46, 0));
        let result = fixture.extract_limited(&hidden, limits);
        assert!(
            matches!(&result, Err(Error::UnsafeZip(text)) if text.contains("5000 files")),
            "{result:?}"
        );
    }

    /// How the biggest addon in the catalog is packed: too many files for the plain end
    /// record to count. Stops short of writing 65,000 files.
    #[test]
    fn a_real_zip64_list_within_the_limits_still_opens() {
        let entries: Vec<RawEntry> = (0..u32::from(u16::MAX) + 2)
            .map(|i| RawEntry::new(format!("Foo/{}/{i}", i % 64), b""))
            .collect();
        let files = entries.len() as u64;
        let fixture = Fixture::new();
        let zip = fixture.zip(&as_zip64(raw_zip(&entries), files));
        let (mut archive, _) = open_archive(&zip, &Limits::default()).unwrap();
        let read = read_entries(&mut archive, &Limits::default()).unwrap();
        assert_eq!(read.len() as u64, files);
    }

    #[test]
    fn folders_marked_only_by_their_attributes_are_folders() {
        let fixture = Fixture::new();
        let mut folder = RawEntry::new("Foo/Libs", b"");
        folder.unix_mode = Some(0o040755);
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            folder,
            RawEntry::new("Foo/Libs/a.lua", b"1"),
        ]);
        fixture.extract(&bytes, &[]).unwrap();
        assert!(fixture.dest().join("Foo/Libs/a.lua").is_file());
    }

    #[test]
    fn password_protected_entries_are_rejected() {
        let fixture = Fixture::new();
        let mut locked = RawEntry::new("Foo/secret.lua", b"xxxxxxxxxxxxxxxx");
        locked.flags |= 1;
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), locked]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::UnsafeZip(_))
        ));
    }

    #[test]
    fn unsupported_compression_is_rejected_cleanly() {
        let fixture = Fixture::new();
        let mut packed = RawEntry::new("Foo/a.lua", b"xxxx");
        packed.method = 12;
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), packed]);
        let result = fixture.extract(&bytes, &[]);
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn junk_is_dropped_silently() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("__MACOSX/Foo/._Foo.toc", b"x"),
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/core.lua", b"keep"),
            RawEntry::new("Foo/.git/config", b"x"),
            RawEntry::new("Foo/.github/workflows/ci.yml", b"x"),
            RawEntry::new("Foo/.gitignore", b"x"),
            RawEntry::new("Foo/.DS_Store", b"x"),
            RawEntry::new("Foo/.ds_store", b"x"),
            RawEntry::new("Foo/Thumbs.db", b"x"),
            RawEntry::new("Foo/sub/THUMBS.DB", b"x"),
            RawEntry::new("Foo/desktop.ini", b"x"),
            RawEntry::new("Foo/Icon\r", b"x"),
            RawEntry::new("Foo/__MACOSX/inner", b"x"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Foo"]);
        assert!(extracted.skipped_files.is_empty());
        assert_eq!(paths(&fixture), ["Foo/Foo.toc", "Foo/core.lua"]);
    }

    #[test]
    fn executables_are_skipped_and_reported_and_never_written() {
        let names = [
            "Foo/a.exe",
            "Foo/b.DLL",
            "Foo/c.scr",
            "Foo/d.com",
            "Foo/e.msi",
            "Foo/f.bat",
            "Foo/g.cmd",
            "Foo/h.ps1",
            "Foo/i.sh",
            "Foo/j.jar",
            "Foo/k.vbs",
            "Foo/l.dylib",
            "Foo/m.so",
            "Foo/n.lnk",
            "Foo/Tool.app/Contents/MacOS/tool",
            "Foo/deep/er/x.Exe",
        ];
        let mut entries = vec![
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/fine.lua", b"ok"),
        ];
        entries.extend(names.iter().map(|n| RawEntry::new(n, b"MZ")));
        let fixture = Fixture::new();
        let extracted = fixture.extract(&raw_zip(&entries), &[]).unwrap();
        assert_eq!(paths(&fixture), ["Foo/Foo.toc", "Foo/fine.lua"]);
        let mut expected: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        expected.sort();
        assert_eq!(extracted.skipped_files, expected);
    }

    #[test]
    fn scripts_shortcuts_and_disk_images_are_skipped_and_reported() {
        let mut names: Vec<String> = BLOCKED_FILE_EXTENSIONS
            .iter()
            .flat_map(|ext| {
                [
                    format!("Foo/lure.{ext}"),
                    format!("Foo/deep/er/LURE.{}", ext.to_ascii_uppercase()),
                ]
            })
            .collect();
        let mut entries = vec![
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/fine.lua", b"ok"),
        ];
        entries.extend(names.iter().map(|n| RawEntry::new(n, b"payload")));
        let fixture = Fixture::new();
        let extracted = fixture.extract(&raw_zip(&entries), &[]).unwrap();
        assert_eq!(paths(&fixture), ["Foo/Foo.toc", "Foo/fine.lua"]);
        names.sort();
        assert_eq!(extracted.skipped_files, names);
    }

    #[test]
    fn what_the_game_loads_is_never_held_back() {
        let kept = [
            "toc", "lua", "xml", "blp", "tga", "ogg", "mp3", "wav", "ttf", "otf", "m2", "skin",
            "anim", "bone", "phys", "wmo", "bls", "txt", "md", "json", "png", "jpg", "html",
        ];
        let mut entries = vec![RawEntry::new("Foo/Foo.toc", TOC)];
        entries.extend(
            kept.iter()
                .map(|ext| RawEntry::new(format!("Foo/media/file.{ext}"), b"1")),
        );
        let fixture = Fixture::new();
        let extracted = fixture.extract(&raw_zip(&entries), &[]).unwrap();
        assert!(extracted.skipped_files.is_empty());
        assert_eq!(fixture.tree().len(), kept.len() + 1);
    }

    #[test]
    fn a_folder_named_like_a_blocked_file_type_is_still_a_folder() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo.Terminal/Foo.Terminal.toc", TOC),
            RawEntry::new("Foo.Terminal/Libs/Bar.Command/bar.lua", b"1"),
            RawEntry::new("Foo.Terminal/Empty.Reg/", b""),
            RawEntry::new("Foo.Py/Foo.Py.toc", TOC),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Foo.Py", "Foo.Terminal"]);
        assert!(extracted.skipped_files.is_empty());
        assert!(
            fixture
                .dest()
                .join("Foo.Terminal/Libs/Bar.Command/bar.lua")
                .is_file()
        );
        assert!(fixture.dest().join("Foo.Terminal/Empty.Reg").is_dir());
    }

    #[test]
    fn executables_in_dropped_folders_are_not_reported() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("build/release.sh", b"#!"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert!(extracted.skipped_files.is_empty());
    }

    #[test]
    fn a_lookalike_extension_is_not_blocked() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/shell.lua", b"1"),
            RawEntry::new("Foo/com", b"2"),
            RawEntry::new("Foo/app/thing.lua", b"3"),
            RawEntry::new("Foo/js", b"4"),
            RawEntry::new("Foo/zip/thing.py.lua", b"5"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert!(extracted.skipped_files.is_empty());
        assert_eq!(fixture.tree().len(), 6);
    }

    #[test]
    fn each_top_level_folder_with_a_toc_becomes_an_addon() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("DBM-Core/DBM-Core_Forever.toc", TOC),
            RawEntry::new("DBM-Core/core.lua", b"1"),
            RawEntry::new("!BugGrabber/!BugGrabber.toc", TOC),
            RawEntry::new("!BugGrabber/Libs/x/y.lua", b"2"),
            RawEntry::new("images/logo.png", b"3"),
            RawEntry::new("OpenMe/readme.txt", b"4"),
            RawEntry::new("README.md", b"5"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["!BugGrabber", "DBM-Core"]);
        assert_eq!(
            paths(&fixture),
            [
                "!BugGrabber/!BugGrabber.toc",
                "!BugGrabber/Libs/x/y.lua",
                "DBM-Core/DBM-Core_Forever.toc",
                "DBM-Core/core.lua"
            ]
        );
        assert_eq!(
            fixture
                .dest()
                .join("DBM-Core/core.lua")
                .metadata()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_toc_deeper_than_the_folder_root_does_not_count() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Bar/sub/deeper/Bar.toc", TOC),
        ]);
        assert_eq!(fixture.extract(&bytes, &[]).unwrap().folders, ["Foo"]);
        assert!(!fixture.dest().join("Bar").exists());
    }

    #[test]
    fn a_folder_named_like_a_toc_does_not_count() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc/readme.txt", b"1")]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::NoAddonInZip)
        ));
    }

    #[test]
    fn appledouble_toc_files_do_not_count() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[RawEntry::new("Foo/._Foo.toc", b"x")]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::NoAddonInZip)
        ));
    }

    #[test]
    fn one_wrapper_folder_is_unwrapped() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Bundle-1.2/", b""),
            RawEntry::new("Bundle-1.2/README.md", b"1"),
            RawEntry::new("Bundle-1.2/AddonA/", b""),
            RawEntry::new("Bundle-1.2/AddonA/AddonA.toc", TOC),
            RawEntry::new("Bundle-1.2/AddonB/AddonB_Forever.toc", TOC),
            RawEntry::new("Bundle-1.2/AddonB/libs/x.lua", b"2"),
            RawEntry::new("Bundle-1.2/docs/index.md", b"3"),
            RawEntry::new("images/x.png", b"4"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["AddonA", "AddonB"]);
        assert_eq!(
            paths(&fixture),
            [
                "AddonA/AddonA.toc",
                "AddonB/AddonB_Forever.toc",
                "AddonB/libs/x.lua"
            ]
        );
    }

    #[test]
    fn two_wrappers_are_not_unwrapped() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("W1/A/A.toc", TOC),
            RawEntry::new("W2/B/B.toc", TOC),
        ]);
        assert!(matches!(
            fixture.extract(&bytes, &[]),
            Err(Error::NoAddonInZip)
        ));
    }

    #[test]
    fn a_real_addon_folder_beside_a_lookalike_wrapper_wins() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Main/Main.toc", TOC),
            RawEntry::new("Extras/Optional/Optional.toc", TOC),
        ]);
        assert_eq!(fixture.extract(&bytes, &[]).unwrap().folders, ["Main"]);
    }

    #[test]
    fn root_level_tocs_name_the_folder_with_the_suffix_removed() {
        for (toc, folder) in [
            ("MyAddon.toc", "MyAddon"),
            ("MyAddon_Forever.toc", "MyAddon"),
            ("MyAddon-Forever.toc", "MyAddon"),
            ("MyAddon_Camelot.toc", "MyAddon"),
            ("MyAddon-camelot.TOC", "MyAddon"),
            ("MyAddon_Mainline.toc", "MyAddon"),
            ("MyAddon_Vanilla.toc", "MyAddon"),
            ("MyAddon-Classic.toc", "MyAddon"),
            ("My_Addon.toc", "My_Addon"),
        ] {
            let fixture = Fixture::new();
            let bytes = raw_zip(&[
                RawEntry::new(toc, TOC),
                RawEntry::new("core.lua", b"1"),
                RawEntry::new("Libs/x.lua", b"2"),
            ]);
            let extracted = fixture.extract(&bytes, &[]).unwrap();
            assert_eq!(extracted.folders, [folder], "{toc}");
            assert!(fixture.dest().join(folder).join(toc).is_file(), "{toc}");
            assert!(
                fixture.dest().join(folder).join("Libs/x.lua").is_file(),
                "{toc}"
            );
        }
    }

    #[test]
    fn root_tocs_with_several_flavors_share_one_folder() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo.toc", TOC),
            RawEntry::new("Foo_Forever.toc", TOC),
            RawEntry::new("Foo_Vanilla.toc", TOC),
            RawEntry::new("Other_Forever.toc", TOC),
        ]);
        // `Foo` and `Other` both have a `_Forever` TOC. Alphabetical keeps it predictable.
        assert_eq!(fixture.extract(&bytes, &[]).unwrap().folders, ["Foo"]);
    }

    #[test]
    fn a_root_toc_with_an_unsafe_name_is_not_usable() {
        for toc in ["CON_Forever.toc", "_Forever.toc", ".toc"] {
            let fixture = Fixture::new();
            let bytes = raw_zip(&[RawEntry::new(toc, TOC)]);
            let result = fixture.extract(&bytes, &[]);
            assert!(
                matches!(result, Err(Error::NoAddonInZip) | Err(Error::UnsafeZip(_))),
                "{toc}: {result:?}"
            );
            assert!(fixture.tree().is_empty());
        }
    }

    #[test]
    fn a_source_archive_folder_takes_the_name_of_its_toc() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Questie-master/", b""),
            RawEntry::new("Questie-master/Questie.toc", TOC),
            RawEntry::new("Questie-master/Questie_Forever.toc", TOC),
            RawEntry::new("Questie-master/Questie_Vanilla.toc", TOC),
            RawEntry::new("Questie-master/Database/db.lua", b"1"),
            RawEntry::new("Questie-master/upload.sh", b"#!"),
        ]);
        let extracted = fixture.extract(&bytes, &[]).unwrap();
        assert_eq!(extracted.folders, ["Questie"]);
        assert_eq!(extracted.skipped_files, ["Questie/upload.sh"]);
        assert!(fixture.dest().join("Questie/Questie_Forever.toc").is_file());
        assert!(fixture.dest().join("Questie/Database/db.lua").is_file());
        assert!(!fixture.dest().join("Questie-master").exists());
    }

    #[test]
    fn folders_are_only_renamed_when_nothing_else_can_work() {
        for (files, expected) in [
            // A TOC already fits the folder, in one of the ways the client accepts.
            (vec!["Foo/Foo.toc"], vec!["Foo"]),
            (vec!["Foo/foo_VANILLA.toc"], vec!["Foo"]),
            (vec!["Foo_Forever/Foo_Forever.toc"], vec!["Foo_Forever"]),
            (
                vec!["Foo-Bar/Foo-Bar.toc", "Foo-Bar/Other.toc"],
                vec!["Foo-Bar"],
            ),
            // Several names, so no telling which one the author meant.
            (
                vec!["Repo-main/A.toc", "Repo-main/B.toc"],
                vec!["Repo-main"],
            ),
            // More than one folder isn't the shape of a source archive.
            (vec!["One/X.toc", "Two/Y.toc"], vec!["One", "Two"]),
            // The TOC's name would be no safer than the folder's.
            (vec!["Repo-main/CON_Forever.toc"], vec!["Repo-main"]),
        ] {
            let fixture = Fixture::new();
            let entries: Vec<RawEntry> = files.iter().map(|n| RawEntry::new(n, TOC)).collect();
            assert_eq!(
                fixture.extract(&raw_zip(&entries), &[]).unwrap().folders,
                expected,
                "{files:?}"
            );
        }
    }

    #[test]
    fn moves_are_never_second_guessed() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[RawEntry::new("w/Addon/Other.toc", TOC)]);
        assert_eq!(
            fixture
                .extract(&bytes, &[mv("*/Addon", "Addon")])
                .unwrap()
                .folders,
            ["Addon"]
        );
    }

    #[test]
    fn root_toc_and_addon_folders_can_coexist() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Main.toc", TOC),
            RawEntry::new("Libs/x.lua", b"1"),
            RawEntry::new("Main_Options/Main_Options.toc", TOC),
        ]);
        assert_eq!(
            fixture.extract(&bytes, &[]).unwrap().folders,
            ["Main", "Main_Options"]
        );
        assert!(fixture.dest().join("Main/Libs/x.lua").is_file());
    }

    #[test]
    fn no_addon_folders_means_no_addon_in_zip() {
        for files in [
            vec![],
            vec!["readme.txt"],
            vec!["Docs/readme.txt", "src/core.lua"],
            vec!["__MACOSX/Foo/Foo.toc"],
            vec!["setup.exe"],
        ] {
            let fixture = Fixture::new();
            let entries: Vec<RawEntry> = files.iter().map(|n| RawEntry::new(n, b"x")).collect();
            let result = fixture.extract(&raw_zip(&entries), &[]);
            assert!(
                matches!(result, Err(Error::NoAddonInZip)),
                "{files:?}: {result:?}"
            );
            assert!(fixture.tree().is_empty());
        }
    }

    fn mv(from: &str, to: &str) -> Move {
        Move {
            from: from.into(),
            to: to.into(),
        }
    }

    fn source_archive() -> Vec<u8> {
        raw_zip(&[
            RawEntry::new("Repo-v1.2.3+build/", b""),
            RawEntry::new("Repo-v1.2.3+build/README.md", b"docs"),
            RawEntry::new("Repo-v1.2.3+build/Addon/", b""),
            RawEntry::new("Repo-v1.2.3+build/Addon/Addon.toc", TOC),
            RawEntry::new("Repo-v1.2.3+build/Addon/core.lua", b"core"),
            RawEntry::new("Repo-v1.2.3+build/Addon/Plugin/Plugin.toc", TOC),
            RawEntry::new("Repo-v1.2.3+build/Addon/Plugin/plugin.lua", b"plugin"),
            RawEntry::new("Repo-v1.2.3+build/tools/build.sh", b"#!"),
        ])
    }

    #[test]
    fn moves_map_prefixes_with_a_wildcard_first_segment() {
        let fixture = Fixture::new();
        let moves = [mv("*/Addon", "Addon")];
        let extracted = fixture.extract(&source_archive(), &moves).unwrap();
        assert_eq!(extracted.folders, ["Addon"]);
        assert_eq!(
            paths(&fixture),
            [
                "Addon/Addon.toc",
                "Addon/Plugin/Plugin.toc",
                "Addon/Plugin/plugin.lua",
                "Addon/core.lua"
            ]
        );
    }

    #[test]
    fn the_longest_move_wins() {
        let fixture = Fixture::new();
        let moves = [mv("*/Addon", "Addon"), mv("*/Addon/Plugin", "Addon_Plugin")];
        let extracted = fixture.extract(&source_archive(), &moves).unwrap();
        assert_eq!(extracted.folders, ["Addon", "Addon_Plugin"]);
        assert_eq!(
            paths(&fixture),
            [
                "Addon/Addon.toc",
                "Addon/core.lua",
                "Addon_Plugin/Plugin.toc",
                "Addon_Plugin/plugin.lua"
            ]
        );
    }

    #[test]
    fn a_literal_prefix_beats_a_wildcard_of_the_same_length() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[RawEntry::new("x/A/A.toc", TOC)]);
        let moves = [mv("*/A", "Wild"), mv("x/A", "Literal")];
        assert_eq!(
            fixture.extract(&bytes, &moves).unwrap().folders,
            ["Literal"]
        );
    }

    #[test]
    fn moves_match_whole_segments_only() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Wrap/AddonExtra/AddonExtra.toc", TOC),
            RawEntry::new("Wrap/Addon/Addon.toc", TOC),
        ]);
        let extracted = fixture.extract(&bytes, &[mv("*/Addon", "Addon")]).unwrap();
        assert_eq!(extracted.folders, ["Addon"]);
        assert_eq!(paths(&fixture), ["Addon/Addon.toc"]);
    }

    #[test]
    fn a_wildcard_alone_maps_the_wrapper_itself() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Details-v12/Details.toc", TOC),
            RawEntry::new("Details-v12/core/a.lua", b"1"),
        ]);
        let extracted = fixture.extract(&bytes, &[mv("*", "Details")]).unwrap();
        assert_eq!(extracted.folders, ["Details"]);
        assert_eq!(
            paths(&fixture),
            ["Details/Details.toc", "Details/core/a.lua"]
        );
    }

    #[test]
    fn a_wildcard_matches_exactly_one_segment() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[RawEntry::new("a/b/Addon/Addon.toc", TOC)]);
        let result = fixture.extract(&bytes, &[mv("*/Addon", "Addon")]);
        assert!(matches!(result, Err(Error::NoAddonInZip)));
    }

    #[test]
    fn moved_folders_without_a_toc_are_dropped() {
        let fixture = Fixture::new();
        let moves = [mv("*/Addon", "Addon"), mv("*/tools", "Tools")];
        let extracted = fixture.extract(&source_archive(), &moves).unwrap();
        assert_eq!(extracted.folders, ["Addon"]);
        assert!(!fixture.dest().join("Tools").exists());
    }

    #[test]
    fn moves_that_match_nothing_mean_no_addon() {
        let fixture = Fixture::new();
        let result = fixture.extract(&source_archive(), &[mv("*/Missing", "Missing")]);
        assert!(matches!(result, Err(Error::NoAddonInZip)));
    }

    #[test]
    fn moves_are_validated_like_paths() {
        for to in [
            "../evil",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "C:evil",
            "CON",
            "trailing.",
            "/abs",
        ] {
            let fixture = Fixture::new();
            let result = fixture.extract(&source_archive(), &[mv("*/Addon", to)]);
            assert!(
                matches!(result, Err(Error::UnsafeZip(_))),
                "{to:?}: {result:?}"
            );
            assert!(fixture.tree().is_empty());
        }
    }

    #[test]
    fn moves_that_collide_are_rejected() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("w/a/Addon.toc", TOC),
            RawEntry::new("w/b/Addon.toc", TOC),
        ]);
        let moves = [mv("*/a", "Addon"), mv("*/b", "Addon")];
        assert!(matches!(
            fixture.extract(&bytes, &moves),
            Err(Error::UnsafeZip(_))
        ));
        assert!(fixture.tree().is_empty());
    }

    #[test]
    fn moves_still_drop_junk_and_block_executables() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("w/Addon/Addon.toc", TOC),
            RawEntry::new("w/Addon/.github/ci.yml", b"x"),
            RawEntry::new("w/Addon/tool.exe", b"MZ"),
        ]);
        let extracted = fixture.extract(&bytes, &[mv("*/Addon", "Addon")]).unwrap();
        assert_eq!(paths(&fixture), ["Addon/Addon.toc"]);
        assert_eq!(extracted.skipped_files, ["Addon/tool.exe"]);
    }

    #[test]
    fn very_deep_trees_extract_past_the_260_character_limit() {
        let fixture = Fixture::new();
        let deep: Vec<String> = (0..25)
            .map(|i| format!("segment-{i:02}-of-a-long-path-name"))
            .collect();
        let name = format!("Foo/{}/deep.lua", deep.join("/"));
        assert!(name.len() > 700);
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new(&name, b"deep"),
        ]);
        fixture.extract(&bytes, &[]).unwrap();

        let mut path = long_path(&fixture.dest());
        path.extend(name.split('/'));
        assert_eq!(fs::read(&path).unwrap(), b"deep");
    }

    #[cfg(windows)]
    #[test]
    fn long_paths_use_the_verbatim_prefix() {
        assert_eq!(
            long_path(Path::new(r"C:\Games\WoW")),
            Path::new(r"\\?\C:\Games\WoW")
        );
        assert_eq!(
            long_path(Path::new("C:/Games/./x/../WoW")),
            Path::new(r"\\?\C:\Games\WoW")
        );
        assert_eq!(
            long_path(Path::new(r"\\?\C:\Games")),
            Path::new(r"\\?\C:\Games")
        );
        assert_eq!(
            long_path(Path::new(r"\\nas\games\WoW")),
            Path::new(r"\\?\UNC\nas\games\WoW")
        );
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Games")),
            Path::new(r"C:\Games")
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\nas\games\WoW")),
            Path::new(r"\\nas\games\WoW")
        );
        assert_eq!(display_path(Path::new(r"C:\Games")), Path::new(r"C:\Games"));
    }

    #[test]
    fn cancelling_leaves_nothing_behind() {
        let fixture = Fixture::new();
        let zip = fixture.zip(&zip_bytes(&[
            ("Foo/Foo.toc", TOC),
            ("Foo/a.lua", &[1u8; 100]),
        ]));
        let cancel = AtomicBool::new(true);
        let result = extract(
            &zip,
            &fixture.dest(),
            &[],
            Limits::default(),
            &mut |_| {},
            &cancel,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(!fixture.dest().exists());
    }

    #[test]
    fn cancelling_from_the_progress_callback_stops_it() {
        let fixture = Fixture::new();
        let big = vec![1u8; 500_000];
        let zip = fixture.zip(&zip_bytes(&[
            ("Foo/Foo.toc", TOC),
            ("Foo/a.bin", &big),
            ("Foo/b.bin", &big),
        ]));
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let result = extract(
            &zip,
            &fixture.dest(),
            &[],
            Limits::default(),
            &mut |_| {
                calls += 1;
                cancel.store(true, Ordering::Relaxed);
            },
            &cancel,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(calls >= 1);
        assert!(!fixture.dest().exists());
    }

    #[test]
    fn an_existing_folder_in_the_destination_is_not_touched() {
        let fixture = Fixture::new();
        let keep = fixture.dest().join("Foo");
        fs::create_dir_all(&keep).unwrap();
        fs::write(keep.join("mine.txt"), b"mine").unwrap();
        let bytes = zip_bytes(&[("Foo/Foo.toc", TOC)]);
        assert!(fixture.extract(&bytes, &[]).is_err());
        assert_eq!(fs::read(keep.join("mine.txt")).unwrap(), b"mine");
    }

    #[test]
    fn progress_and_byte_counts_add_up() {
        let fixture = Fixture::new();
        let zip = fixture.zip(&zip_bytes(&[
            ("Foo/Foo.toc", b"abcd"),
            ("Foo/a.bin", &[1u8; 96]),
        ]));
        let mut reports = Vec::new();
        let extracted = extract(
            &zip,
            &fixture.dest(),
            &[],
            Limits::default(),
            &mut |p| reports.push(p),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(extracted.bytes, 100);
        assert_eq!(
            reports.first().unwrap(),
            &Progress {
                done: 0,
                total: Some(100)
            }
        );
        assert_eq!(
            reports.last().unwrap(),
            &Progress {
                done: 100,
                total: Some(100)
            }
        );
    }

    #[test]
    fn empty_folders_in_an_addon_are_kept() {
        let fixture = Fixture::new();
        let bytes = raw_zip(&[
            RawEntry::new("Foo/Foo.toc", TOC),
            RawEntry::new("Foo/Media/", b""),
        ]);
        fixture.extract(&bytes, &[]).unwrap();
        assert!(fixture.dest().join("Foo/Media").is_dir());
    }

    #[test]
    fn non_utf8_cp437_names_extract() {
        let fixture = Fixture::new();
        let mut entry = RawEntry::new(b"Foo/caf\x82.lua".as_slice(), b"1");
        entry.flags = 0;
        let bytes = raw_zip(&[RawEntry::new("Foo/Foo.toc", TOC), entry]);
        fixture.extract(&bytes, &[]).unwrap();
        assert!(paths(&fixture).contains(&"Foo/café.lua".to_string()));
    }

    #[test]
    fn extract_tree_keeps_folders_without_tocs_and_applies_the_same_checks() {
        let fixture = Fixture::new();
        let zip = fixture.zip(&raw_zip(&[
            RawEntry::new("Config.wtf", b"cfg"),
            RawEntry::new("Account/NAME/bindings.wtf", b"b"),
            RawEntry::new("Account/NAME/.DS_Store", b"x"),
            RawEntry::new("Account/tool.exe", b"MZ"),
        ]));
        let extracted = extract_tree(
            &zip,
            &fixture.dest(),
            Limits::default(),
            TreeOptions::default(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(extracted.folders, ["Account", "Config.wtf"]);
        assert_eq!(extracted.skipped_files, ["Account/tool.exe"]);
        assert_eq!(paths(&fixture), ["Account/NAME/bindings.wtf", "Config.wtf"]);

        for evil in ["../evil", "C:/evil", "a/CON", "a:b"] {
            let fixture = Fixture::new();
            let zip = fixture.zip(&raw_zip(&[RawEntry::new(evil, b"x")]));
            let result = extract_tree(
                &zip,
                &fixture.dest(),
                Limits::default(),
                TreeOptions::default(),
                &mut |_| {},
                &AtomicBool::new(false),
            );
            assert!(matches!(result, Err(Error::UnsafeZip(_))), "{evil}");
        }
    }

    #[test]
    fn extract_tree_can_allow_executables_for_our_own_bundle() {
        let fixture = Fixture::new();
        let mut binary = RawEntry::new("Feeling.app/Contents/MacOS/feeling", b"\x7fELF");
        binary.unix_mode = Some(0o100755);
        let mut plain = RawEntry::new("Feeling.app/Contents/Info.plist", b"<plist/>");
        plain.unix_mode = Some(0o100644);
        let zip = fixture.zip(&raw_zip(&[binary, plain]));
        let options = TreeOptions {
            allow_executables: true,
            keep_exec_bits: true,
        };
        let extracted = extract_tree(
            &zip,
            &fixture.dest(),
            Limits::default(),
            options,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(extracted.folders, ["Feeling.app"]);
        assert!(extracted.skipped_files.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let binary = fixture.dest().join("Feeling.app/Contents/MacOS/feeling");
            assert_eq!(
                binary.metadata().unwrap().permissions().mode() & 0o111,
                0o111
            );
            let plain = fixture.dest().join("Feeling.app/Contents/Info.plist");
            assert_eq!(plain.metadata().unwrap().permissions().mode() & 0o111, 0);
        }
    }

    #[test]
    fn folder_name_check() {
        for good in [
            "Foo",
            "!BugGrabber",
            "DBM-Core",
            "Foo.Bar",
            ".hidden",
            "Über",
        ] {
            assert!(is_safe_folder_name(good), "{good}");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "C:", "CON", "foo.", "foo ", "a*b",
        ] {
            assert!(!is_safe_folder_name(bad), "{bad}");
        }
    }

    #[test]
    fn printable_shortens_and_cleans_untrusted_names() {
        let long = "x".repeat(200);
        assert_eq!(printable(&long).chars().count(), 81);
        assert_eq!(printable("a\r\nb"), "a??b");
    }
}
