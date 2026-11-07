//! Signed app updates through GitHub Releases.
//!
//! The signature over `latest.json` is what makes everything else trustworthy: it's
//! checked over the exact downloaded bytes before they're even parsed, and that file
//! then vouches for the SHA-256 of the update itself. No step here is silent. The app
//! shows the version and notes first and only calls [`apply`] when the player says so.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use flate2::read::GzDecoder;
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::extract::{Limits, TreeOptions, display_path, extract_tree, mark_executable};
use crate::install::{rename_with_retry, sha256_file};
use crate::net::{Http, Progress};

/// Every app update must be signed by the matching secret key, which is kept outside this
/// repository. `xtask verify-latest` checks a release against this constant before it ships.
pub const UPDATE_PUBLIC_KEY: &str = "RWTry6DjjVV6kzdzadGwwGvK/6eIadTv0T2n4LcEjBKvDYyk5jsXXfcb";

/// A plain redirect to the newest release, so it doesn't touch GitHub's REST rate limit.
const RELEASE_BASE_URL: &str =
    "https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
/// Only for assets the signed file doesn't give a size for, and for unpacked binaries.
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const TEMP_NAME: &str = ".wow-is-a-feeling-update";

/// `latest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    /// Unix seconds.
    #[serde(default)]
    pub date: i64,
    pub assets: BTreeMap<String, ManifestAsset>,
}

/// One downloadable build. `kind` stays a string so an old app can still read a manifest
/// that lists a kind invented after it shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestAsset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// A Windows executable, swapped in by renames beside the running one.
    Exe,
    /// One gzipped binary, swapped with self-replace.
    BinGz,
    /// Replaces the file at `$APPIMAGE`.
    AppImage,
    /// A zipped `.app`, which replaces the whole running bundle.
    AppZip,
}

impl AssetKind {
    fn parse(kind: &str) -> Option<AssetKind> {
        match kind {
            "exe" => Some(AssetKind::Exe),
            "bin.gz" => Some(AssetKind::BinGz),
            "appimage" => Some(AssetKind::AppImage),
            "app.zip" => Some(AssetKind::AppZip),
            _ => None,
        }
    }
}

/// The build for this platform, as vouched for by the signed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAsset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub kind: AssetKind,
}

/// An update worth offering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub date: i64,
    pub asset: UpdateAsset,
}

/// An update on disk, ready for [`relaunch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub kind: AssetKind,
    pub path: PathBuf,
}

/// Is there something newer for this platform? Errors, including every network failure,
/// are safe to ignore: the player just doesn't hear about an update today.
pub fn check(http: &Http, current_version: &str) -> Result<Option<UpdateInfo>> {
    check_at(
        http,
        RELEASE_BASE_URL,
        UPDATE_PUBLIC_KEY,
        current_version,
        asset_key(),
    )
}

/// Check a signed `latest.json` and parse it, in that order. Public so the release
/// tooling can prove its output passes the very check the app runs.
pub fn verified_manifest(json: &[u8], signature: &[u8], public_key: &str) -> Result<Manifest> {
    let key = PublicKey::from_base64(public_key).map_err(|_| Error::BadSignature)?;
    let signature = std::str::from_utf8(signature).map_err(|_| Error::BadSignature)?;
    let signature = Signature::decode(signature).map_err(|_| Error::BadSignature)?;
    key.verify(json, &signature, false)
        .map_err(|_| Error::BadSignature)?;
    serde_json::from_slice(json).map_err(|e| Error::Parse {
        what: "the update information".into(),
        detail: e.to_string(),
    })
}

/// The `assets` key for this build. A Linux build inside an AppImage takes the AppImage,
/// so an update stays the same kind of install it replaces.
pub fn asset_key() -> Option<&'static str> {
    asset_key_for(
        env::consts::OS,
        env::consts::ARCH,
        running_appimage().is_some(),
    )
}

/// The AppImage this program is running from. The AppImage runtime exports `$APPIMAGE` and
/// `$APPDIR`, and whatever it starts inherits them, so a bare binary launched from a terminal
/// inside some other AppImage sees that one's. Treating those as ours would replace that
/// other program. Only our own file being inside `$APPDIR` shows they're about us.
fn running_appimage() -> Option<PathBuf> {
    confirmed_appimage(
        &env::var_os("APPIMAGE")?,
        &env::var_os("APPDIR")?,
        &env::current_exe().ok()?,
    )
}

fn confirmed_appimage(appimage: &OsStr, appdir: &OsStr, exe: &Path) -> Option<PathBuf> {
    let appimage = PathBuf::from(appimage);
    let is_file =
        appimage.is_absolute() && fs::symlink_metadata(&appimage).is_ok_and(|meta| meta.is_file());
    // The runtime builds `$APPDIR` from `$TMPDIR` as it stands, links and all.
    let exe_inside = match (fs::canonicalize(appdir), fs::canonicalize(exe)) {
        (Ok(appdir), Ok(exe)) => exe.starts_with(appdir),
        _ => false,
    };
    (is_file && exe_inside).then_some(appimage)
}

/// Download and verify an update, then swap it in. Nothing is touched until the bytes
/// on disk match the signed SHA-256. Call [`relaunch`] next, then exit.
pub fn apply(
    http: &Http,
    update: &UpdateInfo,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Applied> {
    let target = match update.asset.kind {
        AssetKind::Exe | AssetKind::BinGz => running_exe()?,
        AssetKind::AppImage => running_appimage().ok_or_else(|| {
            Error::Other(
                "This update is for the AppImage, but the app isn't running as one.".into(),
            )
        })?,
        AssetKind::AppZip => env::current_exe()
            .ok()
            .and_then(|exe| bundle_of(&exe))
            .ok_or_else(|| Error::Other("Couldn't find the app bundle to update.".into()))?,
    };
    apply_to(
        http,
        &update.asset,
        &target,
        &|new| replace_running(new, &target),
        progress,
        cancel,
    )
}

/// The program to replace. On Windows that's the real file behind any link, because
/// renaming a link away would leave the real program old, and the swap has to stay on the
/// real file's drive. On Unix, self-replace follows links itself.
fn running_exe() -> Result<PathBuf> {
    let exe = env::current_exe().map_err(|e| Error::io("the running program", e))?;
    if cfg!(windows) {
        real_program_path(&exe).map_err(|e| Error::io(&exe, e))
    } else {
        Ok(exe)
    }
}

fn real_program_path(exe: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(exe).map(|real| display_path(&real))
}

#[cfg(windows)]
fn replace_running(new: &Path, exe: &Path) -> io::Result<()> {
    swap_running_exe(new, exe, &|old| self_replace::self_delete_at(old))
}

/// Copies, then renames over the target in one step, so it can't leave a gap.
#[cfg(not(windows))]
fn replace_running(new: &Path, _exe: &Path) -> io::Result<()> {
    self_replace::self_replace(new)
}

/// Swap a new program in for the running one, so a working program stays at `exe` whatever
/// goes wrong. Windows won't overwrite a program that's running but will let it be renamed,
/// so it steps aside and the new one takes its name. The new one is already beside it, so
/// these are plain renames: no disk space needed, and retries for a scanner holding a file.
/// Deleting the old one is put off until the new one is in place, and it's fine if that fails.
#[cfg(any(windows, test))]
fn swap_running_exe(
    new: &Path,
    exe: &Path,
    throw_away: &dyn Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut old = exe.as_os_str().to_owned();
    old.push(".old");
    let old = PathBuf::from(old);
    rename_with_retry(exe, &old)?;
    if let Err(error) = rename_with_retry(new, exe) {
        return match put_back(&old, exe) {
            Ok(()) => Err(error),
            Err(back) => {
                log::error!("Couldn't swap in an update ({error}) or undo it ({back})");
                Err(io::Error::other(format!(
                    "the update couldn't go in ({error}) and the old program couldn't go back \
                     ({back}). Nothing was deleted, so rename {} to {} to get it back",
                    old.display(),
                    exe.display()
                )))
            }
        };
    }
    if let Err(error) = throw_away(&old) {
        log::warn!("Couldn't delete the old program: {error}");
    }
    Ok(())
}

/// By whichever way works. A rename needs nothing holding the file open against it, a hard
/// link doesn't mind that, and a copy only needs the disk space.
#[cfg(any(windows, test))]
fn put_back(old: &Path, exe: &Path) -> io::Result<()> {
    rename_with_retry(old, exe)
        .or_else(|_| fs::hard_link(old, exe))
        .or_else(|_| fs::copy(old, exe).map(drop))
}

/// Start the new version and return, so the caller can exit.
pub fn relaunch(applied: Applied) -> Result<()> {
    relaunch_command(&applied)
        .spawn()
        .map(drop)
        .map_err(|e| Error::io(applied.path, e))
}

fn relaunch_command(applied: &Applied) -> Command {
    match applied.kind {
        AssetKind::AppZip => {
            let mut command = Command::new("open");
            command.arg("-n").arg(&applied.path);
            command
        }
        _ => Command::new(&applied.path),
    }
}

fn check_at(
    http: &Http,
    base_url: &str,
    public_key: &str,
    current_version: &str,
    key: Option<&str>,
) -> Result<Option<UpdateInfo>> {
    let json = http.get_bytes(&format!("{base_url}/latest.json"), MAX_MANIFEST_BYTES)?;
    let signature = http.get_bytes(
        &format!("{base_url}/latest.json.minisig"),
        MAX_SIGNATURE_BYTES,
    )?;
    let manifest = verified_manifest(&json, &signature, public_key)?;
    Ok(offer(manifest, current_version, key))
}

fn offer(mut manifest: Manifest, current_version: &str, key: Option<&str>) -> Option<UpdateInfo> {
    if !is_newer(&manifest.version, current_version) {
        return None;
    }
    let asset = manifest.assets.remove(key?)?;
    Some(UpdateInfo {
        asset: UpdateAsset {
            kind: AssetKind::parse(&asset.kind)?,
            url: asset.url,
            sha256: asset.sha256,
            size: asset.size,
        },
        version: manifest.version,
        notes: manifest.notes,
        date: manifest.date,
    })
}

fn asset_key_for(os: &str, arch: &str, in_appimage: bool) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("windows-x64"),
        ("linux", "x86_64") if in_appimage => Some("linux-x64-appimage"),
        ("linux", "x86_64") => Some("linux-x64-bin"),
        ("macos", "x86_64" | "aarch64") => Some("macos-universal"),
        _ => None,
    }
}

/// Numeric comparison, so `0.10.0` beats `0.9.0`. A release beats its own pre-releases,
/// and pre-releases of one version count as equal, because this only ever has to answer
/// "is this worth offering?". Anything that doesn't parse is never newer.
fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(candidate), Some(current)) = (parse_version(candidate), parse_version(current))
    else {
        return false;
    };
    let width = candidate.0.len().max(current.0.len());
    let padded = |parts: &[u64]| -> Vec<u64> {
        parts
            .iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(width)
            .collect()
    };
    match padded(&candidate.0).cmp(&padded(&current.0)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => !candidate.1 && current.1,
    }
}

/// The numbers of a version, and whether it's a pre-release.
fn parse_version(text: &str) -> Option<(Vec<u64>, bool)> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let text = text.split('+').next().unwrap_or(text);
    let (numbers, pre_release) = match text.split_once('-') {
        Some((numbers, _)) => (numbers, true),
        None => (text, false),
    };
    let parts: Option<Vec<u64>> = numbers.split('.').map(|part| part.parse().ok()).collect();
    Some((parts?, pre_release))
}

/// The `.app` a running binary lives in.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

/// Everything after picking the target. `replace_running` swaps the running program in the
/// real app, and a fake target in tests.
fn apply_to(
    http: &Http,
    asset: &UpdateAsset,
    target: &Path,
    replace_running: &dyn Fn(&Path) -> io::Result<()>,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Applied> {
    let dir = target.parent().ok_or_else(|| {
        Error::Other(format!("Couldn't find the folder of {}.", target.display()))
    })?;
    // Beside the target, so the final rename stays on one volume.
    let download = dir.join(TEMP_NAME);
    let max_bytes = if asset.size > 0 {
        asset.size
    } else {
        MAX_BINARY_BYTES
    };
    let result = http
        .download(&asset.url, &download, progress, cancel, max_bytes)
        .and_then(|_| verify_download(&download, asset, cancel))
        .and_then(|()| swap_in(asset.kind, &download, target, replace_running, cancel));
    let _ = fs::remove_file(&download);
    result?;
    Ok(Applied {
        kind: asset.kind,
        path: target.to_path_buf(),
    })
}

fn verify_download(path: &Path, asset: &UpdateAsset, cancel: &AtomicBool) -> Result<()> {
    let size = fs::metadata(path).map_err(|e| Error::io(path, e))?.len();
    if asset.size > 0 && size != asset.size {
        return Err(Error::Corrupt(format!(
            "it's {size} bytes, but it should be {}",
            asset.size
        )));
    }
    let actual = sha256_file(path, cancel)?;
    if !asset.sha256.is_empty() && actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        Ok(())
    } else {
        Err(Error::Corrupt(
            "its checksum doesn't match the signed update information".into(),
        ))
    }
}

fn swap_in(
    kind: AssetKind,
    download: &Path,
    target: &Path,
    replace_running: &dyn Fn(&Path) -> io::Result<()>,
    cancel: &AtomicBool,
) -> Result<()> {
    match kind {
        AssetKind::Exe => replace_running(download).map_err(|e| Error::io(target, e)),
        AssetKind::BinGz => {
            let unpacked = download.with_extension("bin");
            let result = gunzip(download, &unpacked)
                .and_then(|()| replace_running(&unpacked).map_err(|e| Error::io(target, e)));
            let _ = fs::remove_file(&unpacked);
            result
        }
        AssetKind::AppImage => {
            mark_executable(download).map_err(|e| Error::io(download, e))?;
            rename_with_retry(download, target).map_err(|e| Error::io(target, e))
        }
        AssetKind::AppZip => swap_app_bundle(download, target, cancel),
    }
}

fn gunzip(source: &Path, dest: &Path) -> Result<()> {
    let file = File::open(source).map_err(|e| Error::io(source, e))?;
    let mut reader = GzDecoder::new(file).take(MAX_BINARY_BYTES + 1);
    let mut output = File::create(dest).map_err(|e| Error::io(dest, e))?;
    let copied = io::copy(&mut reader, &mut output).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof => {
            Error::Corrupt("the compressed update is damaged".into())
        }
        _ => Error::io(dest, e),
    })?;
    if copied > MAX_BINARY_BYTES {
        return Err(Error::Corrupt(
            "the update unpacks to more than it should".into(),
        ));
    }
    output.flush().map_err(|e| Error::io(dest, e))
}

/// Unpack next to the running `.app`, move the old bundle aside, move the new one in,
/// and only then delete the old one. A failure at any point leaves the old bundle running.
fn swap_app_bundle(zip: &Path, app: &Path, cancel: &AtomicBool) -> Result<()> {
    let parent = app
        .parent()
        .ok_or_else(|| Error::Other(format!("Couldn't find the folder of {}.", app.display())))?;
    let staging = parent.join(format!("{TEMP_NAME}-app"));
    let aside = app.with_extension("app.old");
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&aside);
    let options = TreeOptions {
        allow_executables: true,
        keep_exec_bits: true,
        ..TreeOptions::default()
    };
    let result = extract_tree(
        zip,
        &staging,
        Limits::default(),
        options,
        &mut |_| {},
        cancel,
    )
    .and_then(|extracted| {
        let mut bundles = extracted
            .folders
            .iter()
            .filter(|name| name.ends_with(".app"));
        match (bundles.next(), bundles.next()) {
            (Some(bundle), None) => Ok(staging.join(bundle)),
            _ => Err(Error::Corrupt("it doesn't hold exactly one app".into())),
        }
    })
    .and_then(|new_app| {
        rename_with_retry(app, &aside).map_err(|e| Error::io(app, e))?;
        if let Err(e) = rename_with_retry(&new_app, app) {
            let _ = rename_with_retry(&aside, app);
            return Err(Error::io(app, e));
        }
        // The update has happened. An old bundle that won't delete is only clutter.
        if let Err(e) = fs::remove_dir_all(&aside) {
            log::warn!("Couldn't delete the old app bundle: {e}");
        }
        Ok(())
    });
    let _ = fs::remove_dir_all(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::test_zips::zip_bytes;
    use crate::install::fault::{self, Plan};
    use crate::install::to_hex;
    use crate::net::test_server::{respond, serve};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    struct Keys {
        public: String,
        secret: minisign::SecretKey,
    }

    fn generate_keys() -> Keys {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        Keys {
            public: pair.pk.to_base64(),
            secret: pair.sk,
        }
    }

    impl Keys {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            minisign::sign(
                None,
                &self.secret,
                Cursor::new(data),
                Some("timestamp:1\tfile:latest.json"),
                None,
            )
            .unwrap()
            .into_string()
            .into_bytes()
        }
    }

    fn sha_hex(data: &[u8]) -> String {
        to_hex(&Sha256::digest(data))
    }

    fn manifest_json(version: &str) -> Vec<u8> {
        format!(
            r#"{{
              "version": "{version}",
              "notes": "What changed, in plain words.",
              "date": 1794000000,
              "assets": {{
                "windows-x64": {{ "url": "https://example.test/a.exe", "sha256": "aa", "size": 123, "kind": "exe" }},
                "linux-x64-appimage": {{ "url": "https://example.test/a.AppImage", "sha256": "bb", "size": 123, "kind": "appimage" }},
                "linux-x64-bin": {{ "url": "https://example.test/a.gz", "sha256": "cc", "size": 123, "kind": "bin.gz" }},
                "macos-universal": {{ "url": "https://example.test/a.zip", "sha256": "dd", "size": 123, "kind": "app.zip" }}
              }}
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_good_signature_gives_the_parsed_manifest() {
        let keys = generate_keys();
        let json = manifest_json("1.2.3");
        let manifest = verified_manifest(&json, &keys.sign(&json), &keys.public).unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.date, 1794000000);
        assert_eq!(manifest.assets.len(), 4);
    }

    #[test]
    fn a_tampered_manifest_is_rejected() {
        let keys = generate_keys();
        let json = manifest_json("1.2.3");
        let signature = keys.sign(&json);
        let mut tampered = json.clone();
        let at = tampered.windows(5).position(|w| w == b"1.2.3").unwrap();
        tampered[at] = b'9';
        assert!(matches!(
            verified_manifest(&tampered, &signature, &keys.public),
            Err(Error::BadSignature)
        ));
        let mut extended = json.clone();
        extended.push(b' ');
        assert!(matches!(
            verified_manifest(&extended, &signature, &keys.public),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let (keys, other) = (generate_keys(), generate_keys());
        let json = manifest_json("1.2.3");
        let signature = keys.sign(&json);
        assert!(matches!(
            verified_manifest(&json, &signature, &other.public),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn garbage_keys_and_signatures_are_rejected() {
        let keys = generate_keys();
        let json = manifest_json("1.2.3");
        let signature = keys.sign(&json);
        for bad_key in ["", "not base64!", "RWQ="] {
            assert!(matches!(
                verified_manifest(&json, &signature, bad_key),
                Err(Error::BadSignature)
            ));
        }
        for bad_signature in [&b""[..], b"untrusted comment: x", &[0xFF, 0xFE, 0x00]] {
            assert!(matches!(
                verified_manifest(&json, bad_signature, &keys.public),
                Err(Error::BadSignature)
            ));
        }
    }

    #[test]
    fn a_tampered_trusted_comment_is_rejected() {
        let keys = generate_keys();
        let json = manifest_json("1.2.3");
        let signature = String::from_utf8(keys.sign(&json)).unwrap();
        let tampered = signature.replace("timestamp:1", "timestamp:2");
        assert_ne!(signature, tampered);
        assert!(matches!(
            verified_manifest(&json, tampered.as_bytes(), &keys.public),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn verification_happens_before_parsing() {
        let keys = generate_keys();
        let junk = b"this isn't json at all";
        // Garbage with a bad signature is a signature failure, never a parse attempt.
        let bad_signature = keys.sign(b"something else");
        assert!(matches!(
            verified_manifest(junk, &bad_signature, &keys.public),
            Err(Error::BadSignature)
        ));
        // Signed garbage gets as far as the parser.
        let signed = keys.sign(junk);
        assert!(matches!(
            verified_manifest(junk, &signed, &keys.public),
            Err(Error::Parse { .. })
        ));
    }

    #[test]
    fn the_update_key_is_valid_and_refuses_signatures_from_other_keys() {
        let keys = generate_keys();
        let json = manifest_json("1.2.3");
        assert!(PublicKey::from_base64(UPDATE_PUBLIC_KEY).is_ok());
        assert!(matches!(
            verified_manifest(&json, &keys.sign(&json), UPDATE_PUBLIC_KEY),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn version_comparison() {
        for (candidate, current) in [
            ("0.10.0", "0.9.0"),
            ("1.0.0", "0.99.99"),
            ("0.2.0", "0.1.9"),
            ("1.0.1", "1.0"),
            ("1.0.0.1", "1.0.0"),
            ("v1.2.0", "1.1.0"),
            ("1.2.0", "V1.1.0"),
            ("1.0.0", "1.0.0-beta"),
            ("1.0.0", "1.0.0-rc.1"),
            ("1.1.0-beta", "1.0.0"),
            ("1.0.0+build5", "1.0.0-rc1"),
            ("18446744073709551615.0", "1.0"),
        ] {
            assert!(
                is_newer(candidate, current),
                "{candidate} should beat {current}"
            );
        }
        for (candidate, current) in [
            ("0.9.0", "0.10.0"),
            ("1.0.0", "1.0.0"),
            ("1.0", "1.0.0"),
            ("1.0.0", "1.0"),
            ("v1.0.0", "1.0.0"),
            ("1.0.0-beta", "1.0.0"),
            ("1.0.0-rc.2", "1.0.0-rc.1"),
            ("1.0.0+build5", "1.0.0"),
            ("0.1.0", "1.0.0"),
            ("garbage", "1.0.0"),
            ("1.0.0", "garbage"),
            ("", "1.0.0"),
            ("1..0", "0.1"),
            ("1.x.0", "0.1"),
            ("99999999999999999999999.0", "1.0"),
        ] {
            assert!(
                !is_newer(candidate, current),
                "{candidate} shouldn't beat {current}"
            );
        }
    }

    #[test]
    fn asset_keys_by_platform() {
        assert_eq!(
            asset_key_for("windows", "x86_64", false),
            Some("windows-x64")
        );
        assert_eq!(
            asset_key_for("linux", "x86_64", false),
            Some("linux-x64-bin")
        );
        assert_eq!(
            asset_key_for("linux", "x86_64", true),
            Some("linux-x64-appimage")
        );
        assert_eq!(
            asset_key_for("macos", "x86_64", false),
            Some("macos-universal")
        );
        assert_eq!(
            asset_key_for("macos", "aarch64", false),
            Some("macos-universal")
        );
        assert_eq!(asset_key_for("windows", "aarch64", false), None);
        assert_eq!(asset_key_for("linux", "aarch64", false), None);
        assert_eq!(asset_key_for("freebsd", "x86_64", false), None);
    }

    #[test]
    fn offers_pick_the_right_asset_and_only_when_newer() {
        let manifest: Manifest = serde_json::from_slice(&manifest_json("1.0.0")).unwrap();
        let info = offer(manifest.clone(), "0.9.0", Some("linux-x64-bin")).unwrap();
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.notes, "What changed, in plain words.");
        assert_eq!(info.date, 1794000000);
        assert_eq!(info.asset.kind, AssetKind::BinGz);
        assert_eq!(info.asset.url, "https://example.test/a.gz");

        assert!(offer(manifest.clone(), "1.0.0", Some("linux-x64-bin")).is_none());
        assert!(offer(manifest.clone(), "2.0.0", Some("linux-x64-bin")).is_none());
        assert!(offer(manifest.clone(), "0.9.0", Some("plan9-mips")).is_none());
        assert!(offer(manifest, "0.9.0", None).is_none());
    }

    #[test]
    fn unknown_kinds_and_fields_from_the_future_are_tolerated() {
        let json = br#"{
            "version": "2.0.0", "future": true,
            "assets": {
                "windows-x64": { "url": "u", "sha256": "s", "size": 1, "kind": "exe", "extra": 1 },
                "new-platform": { "url": "u", "sha256": "s", "size": 1, "kind": "dmg" }
            }
        }"#;
        let manifest: Manifest = serde_json::from_slice(json).unwrap();
        assert!(offer(manifest.clone(), "1.0.0", Some("windows-x64")).is_some());
        assert!(offer(manifest, "1.0.0", Some("new-platform")).is_none());
    }

    fn release_server(json: Vec<u8>, signature: Vec<u8>) -> crate::net::test_server::Server {
        serve(move |request, stream| match request.path.as_str() {
            "/latest.json" => respond(stream, "200 OK", &[], &json),
            "/latest.json.minisig" => respond(stream, "200 OK", &[], &signature),
            _ => respond(stream, "404 Not Found", &[], b""),
        })
    }

    #[test]
    fn check_offers_a_signed_newer_release() {
        let keys = generate_keys();
        let json = manifest_json("0.2.0");
        let server = release_server(json.clone(), keys.sign(&json));
        let http = Http::allowing_loopback();
        let key = Some("windows-x64");

        let info = check_at(&http, &server.base, &keys.public, "0.1.0", key)
            .unwrap()
            .unwrap();
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.asset.kind, AssetKind::Exe);

        assert_eq!(
            check_at(&http, &server.base, &keys.public, "0.2.0", key).unwrap(),
            None
        );
        assert_eq!(
            check_at(&http, &server.base, &keys.public, "0.3.0", key).unwrap(),
            None
        );
    }

    #[test]
    fn check_rejects_tampering_end_to_end() {
        let keys = generate_keys();
        let signature = keys.sign(&manifest_json("0.2.0"));
        let server = release_server(manifest_json("9.9.9"), signature);
        let result = check_at(
            &Http::allowing_loopback(),
            &server.base,
            &keys.public,
            "0.1.0",
            Some("windows-x64"),
        );
        assert!(matches!(result, Err(Error::BadSignature)), "{result:?}");
    }

    #[test]
    fn check_failures_are_plain_errors() {
        let keys = generate_keys();
        let server = serve(|request, stream| {
            if request.path == "/latest.json" {
                respond(stream, "200 OK", &[], b"{}");
            } else {
                respond(stream, "404 Not Found", &[], b"");
            }
        });
        let result = check_at(
            &Http::allowing_loopback(),
            &server.base,
            &keys.public,
            "0.1.0",
            Some("windows-x64"),
        );
        assert!(
            matches!(result, Err(Error::HttpStatus { status: 404, .. })),
            "{result:?}"
        );

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let result = check_at(
            &Http::allowing_loopback(),
            &format!("http://127.0.0.1:{port}"),
            &keys.public,
            "0.1.0",
            Some("windows-x64"),
        );
        assert!(matches!(result, Err(Error::Offline { .. })), "{result:?}");
    }

    #[test]
    fn check_refuses_an_oversized_manifest() {
        let keys = generate_keys();
        let huge = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];
        let server = release_server(huge.clone(), keys.sign(&huge));
        let result = check_at(
            &Http::allowing_loopback(),
            &server.base,
            &keys.public,
            "0.1.0",
            Some("windows-x64"),
        );
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }

    /// A fake installed app: a folder with a target file in it, and a server for the update.
    struct FakeApp {
        dir: tempfile::TempDir,
        target: PathBuf,
    }

    impl FakeApp {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("wow-is-a-feeling");
            fs::write(&target, b"original binary").unwrap();
            FakeApp { dir, target }
        }

        fn only_file_is_target(&self) {
            let names: Vec<_> = fs::read_dir(self.dir.path())
                .unwrap()
                .flatten()
                .map(|i| i.file_name())
                .collect();
            assert_eq!(
                names,
                ["wow-is-a-feeling"],
                "stray files left beside the target"
            );
        }
    }

    fn asset_for(url: String, body: &[u8], kind: AssetKind) -> UpdateAsset {
        UpdateAsset {
            url,
            sha256: sha_hex(body),
            size: body.len() as u64,
            kind,
        }
    }

    fn file_server(body: Vec<u8>) -> crate::net::test_server::Server {
        serve(move |_, stream| respond(stream, "200 OK", &[], &body))
    }

    /// What self-replace would do to the running binary, aimed at the fake target.
    fn replace_fake(target: &Path) -> impl Fn(&Path) -> io::Result<()> + '_ {
        move |new| fs::copy(new, target).map(drop)
    }

    fn never_replace(_: &Path) -> io::Result<()> {
        panic!("the swap must not run for a bad download");
    }

    #[test]
    fn an_exe_update_swaps_verified_bytes_in() {
        let app = FakeApp::new();
        let body = b"new binary".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a.exe", &body, AssetKind::Exe);
        let mut last = None;
        let applied = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &replace_fake(&app.target),
            &mut |p| last = Some(p),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            applied,
            Applied {
                kind: AssetKind::Exe,
                path: app.target.clone()
            }
        );
        assert_eq!(fs::read(&app.target).unwrap(), body);
        assert_eq!(
            last,
            Some(Progress {
                done: 10,
                total: Some(10)
            })
        );
        app.only_file_is_target();
    }

    #[test]
    fn a_bad_sha256_aborts_with_the_original_untouched() {
        let app = FakeApp::new();
        let body = b"evil binary".to_vec();
        let server = file_server(body.clone());
        for kind in [
            AssetKind::Exe,
            AssetKind::BinGz,
            AssetKind::AppImage,
            AssetKind::AppZip,
        ] {
            let mut asset = asset_for(server.base.clone() + "/a", &body, kind);
            asset.sha256 = sha_hex(b"what was signed");
            let result = apply_to(
                &Http::allowing_loopback(),
                &asset,
                &app.target,
                &never_replace,
                &mut |_| {},
                &AtomicBool::new(false),
            );
            assert!(
                matches!(result, Err(Error::Corrupt(_))),
                "{kind:?}: {result:?}"
            );
            assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
            app.only_file_is_target();
        }
    }

    #[test]
    fn an_empty_signed_digest_is_refused() {
        let app = FakeApp::new();
        let body = b"anything".to_vec();
        let server = file_server(body.clone());
        let mut asset = asset_for(server.base.clone() + "/a", &body, AssetKind::Exe);
        asset.sha256 = String::new();
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &never_replace,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
    }

    #[test]
    fn a_wrong_size_aborts_with_the_original_untouched() {
        let app = FakeApp::new();
        let body = b"new binary".to_vec();
        let server = file_server(body.clone());
        for size in [3, 1000] {
            let mut asset = asset_for(server.base.clone() + "/a", &body, AssetKind::Exe);
            asset.size = size;
            let result = apply_to(
                &Http::allowing_loopback(),
                &asset,
                &app.target,
                &never_replace,
                &mut |_| {},
                &AtomicBool::new(false),
            );
            assert!(
                matches!(result, Err(Error::Corrupt(_))),
                "{size}: {result:?}"
            );
            assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
            app.only_file_is_target();
        }
    }

    #[test]
    fn a_failed_swap_reports_and_cleans_up() {
        let app = FakeApp::new();
        let body = b"new binary".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a", &body, AssetKind::Exe);
        let refuse = |_: &Path| -> io::Result<()> { Err(io::Error::other("locked")) };
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &refuse,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        app.only_file_is_target();
    }

    fn delete(old: &Path) -> io::Result<()> {
        fs::remove_file(old)
    }

    /// A program file and a verified new one beside it.
    fn app_with_update() -> (FakeApp, PathBuf) {
        let app = FakeApp::new();
        let new = app.dir.path().join(TEMP_NAME);
        fs::write(&new, b"new binary").unwrap();
        (app, new)
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_swap_puts_the_new_program_in_and_only_then_throws_the_old_one_away() {
        let (app, new) = app_with_update();
        let seen = Mutex::new(Vec::new());
        let throw_away = |old: &Path| {
            // By now the new one has to be in place, and the old one still intact.
            *seen.lock().unwrap() = vec![fs::read(&app.target)?, fs::read(old)?];
            fs::remove_file(old)
        };
        swap_running_exe(&new, &app.target, &throw_away).unwrap();
        assert_eq!(fs::read(&app.target).unwrap(), b"new binary");
        assert_eq!(
            *seen.lock().unwrap(),
            [&b"new binary"[..], b"original binary"]
        );
        app.only_file_is_target();
    }

    #[test]
    fn failing_to_throw_the_old_program_away_is_only_clutter() {
        let (app, new) = app_with_update();
        let refuse = |_: &Path| -> io::Result<()> { Err(io::Error::other("no helper allowed")) };
        swap_running_exe(&new, &app.target, &refuse).unwrap();
        assert_eq!(fs::read(&app.target).unwrap(), b"new binary");
    }

    #[test]
    fn when_the_new_program_wont_go_in_the_old_one_goes_straight_back() {
        let (app, new) = app_with_update();
        fault::set(Plan {
            fail_at: vec![1],
            ..Plan::default()
        });
        let result = swap_running_exe(&new, &app.target, &delete);
        fault::take();
        assert!(result.is_err());
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        assert_eq!(
            names_in(app.dir.path()),
            [TEMP_NAME, "wow-is-a-feeling"],
            "only the download is left to clean up"
        );
    }

    #[test]
    fn when_the_old_program_wont_rename_back_it_gets_there_another_way() {
        let (app, new) = app_with_update();
        fault::set(Plan {
            fail_at: vec![1, 2],
            ..Plan::default()
        });
        let result = swap_running_exe(&new, &app.target, &delete);
        fault::take();
        assert!(result.is_err());
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
    }

    #[test]
    fn when_nothing_can_put_it_back_the_error_says_where_it_is() {
        // A folder takes a rename, and refuses a hard link and a copy.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("program");
        fs::create_dir(&target).unwrap();
        let new = dir.path().join(TEMP_NAME);
        fs::write(&new, b"new").unwrap();
        fault::set(Plan {
            fail_at: vec![1, 2],
            ..Plan::default()
        });
        let text = swap_running_exe(&new, &target, &delete)
            .unwrap_err()
            .to_string();
        fault::take();
        let old = dir.path().join("program.old");
        assert!(old.is_dir(), "nothing may be deleted");
        assert!(text.contains(&*old.to_string_lossy()), "{text}");
        assert!(text.contains("rename"), "{text}");
    }

    #[test]
    fn a_leftover_from_an_earlier_update_doesnt_get_in_the_way() {
        let (app, new) = app_with_update();
        fs::write(app.dir.path().join("wow-is-a-feeling.old"), b"even older").unwrap();
        swap_running_exe(&new, &app.target, &delete).unwrap();
        assert_eq!(fs::read(&app.target).unwrap(), b"new binary");
        app.only_file_is_target();
    }

    #[test]
    fn the_real_swap_through_a_real_download_leaves_only_the_new_program() {
        let app = FakeApp::new();
        let body = b"new binary".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a.exe", &body, AssetKind::Exe);
        apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &|new| swap_running_exe(new, &app.target, &delete),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(&app.target).unwrap(), body);
        app.only_file_is_target();
    }

    #[test]
    fn the_real_swap_failing_after_a_real_download_leaves_only_the_old_program() {
        let app = FakeApp::new();
        let body = b"new binary".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a.exe", &body, AssetKind::Exe);
        let swap = |new: &Path| {
            // The download has had its rename by now, so count from here.
            fault::set(Plan {
                fail_at: vec![1],
                ..Plan::default()
            });
            let result = swap_running_exe(new, &app.target, &delete);
            fault::take();
            result
        };
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &swap,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        app.only_file_is_target();
    }

    /// A scanner or sync tool with the download open and not sharing it, past the retries.
    #[cfg(windows)]
    #[test]
    fn a_held_download_fails_the_swap_with_the_old_program_back_in_place() {
        use std::os::windows::fs::OpenOptionsExt;

        let (app, new) = app_with_update();
        let held = File::options().read(true).share_mode(0).open(&new).unwrap();
        let result = swap_running_exe(&new, &app.target, &delete);
        drop(held);
        assert!(result.is_err());
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        assert_eq!(names_in(app.dir.path()), [TEMP_NAME, "wow-is-a-feeling"]);
    }

    /// Not a test of its own: the test below runs this inside a copy of this test program,
    /// so that the file being swapped is one that's really running.
    #[cfg(windows)]
    #[test]
    #[ignore = "only runs as a child of the test below"]
    fn swap_this_running_program() {
        let Some(new) = env::var_os("FEELING_SWAP_TO") else {
            return;
        };
        let should_fail = env::var_os("FEELING_SWAP_FAILS").is_some();
        if should_fail {
            fault::set(Plan {
                fail_at: vec![1],
                ..Plan::default()
            });
        }
        let exe = running_exe().unwrap();
        let result = replace_running(Path::new(&new), &exe);
        fault::take();
        assert_eq!(result.is_err(), should_fail, "{result:?}");
    }

    /// The swap with nothing faked but the failure: a running program, the real way of getting
    /// rid of its old file, and the result launched afterward.
    #[cfg(windows)]
    #[test]
    fn a_running_program_really_gets_replaced_and_really_comes_back() {
        let run_swap = |exe: &Path, new: &Path, fails: bool| {
            let mut child = Command::new(exe);
            child
                .args(["--exact", "selfupdate::tests::swap_this_running_program"])
                .args(["--ignored", "--nocapture"])
                .env("FEELING_SWAP_TO", new);
            if fails {
                child.env("FEELING_SWAP_FAILS", "1");
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        let launches = |exe: &Path| {
            Command::new(exe)
                .arg("--list")
                .output()
                .unwrap()
                .status
                .success()
        };
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("running.exe");
        let new = dir.path().join(TEMP_NAME);
        fs::copy(env::current_exe().unwrap(), &exe).unwrap();
        let mut new_bytes = fs::read(&exe).unwrap();
        // Windows ignores what's past the end of a program, so this still runs.
        new_bytes.extend_from_slice(b"the new version");
        fs::write(&new, &new_bytes).unwrap();

        let old_size = fs::metadata(&exe).unwrap().len();
        run_swap(&exe, &new, true);
        assert_eq!(fs::metadata(&exe).unwrap().len(), old_size);
        assert!(
            new.exists(),
            "a failed swap leaves the download for its caller"
        );
        assert!(launches(&exe), "the old program has to still work");

        run_swap(&exe, &new, false);
        assert_eq!(fs::metadata(&exe).unwrap().len(), new_bytes.len() as u64);
        assert!(!new.exists());
        assert!(launches(&exe), "the new program has to work");
    }

    /// The program stays reachable through the link, and the real file is what gets replaced.
    #[cfg(windows)]
    #[test]
    fn a_program_behind_a_link_is_replaced_where_it_really_is() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let real_exe = real_dir.join("app.exe");
        fs::write(&real_exe, b"old").unwrap();
        let link = dir.path().join("link");
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real_dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "mklink failed");

        let target = real_program_path(&link.join("app.exe")).unwrap();
        assert_eq!(
            fs::canonicalize(&target).unwrap(),
            fs::canonicalize(&real_exe).unwrap()
        );
        assert!(!target.to_string_lossy().starts_with(r"\\?\"), "{target:?}");

        let new = real_dir.join(TEMP_NAME);
        fs::write(&new, b"new").unwrap();
        swap_running_exe(&new, &target, &delete).unwrap();
        assert_eq!(fs::read(&real_exe).unwrap(), b"new");
        assert_eq!(fs::read(link.join("app.exe")).unwrap(), b"new");
        assert_eq!(names_in(&real_dir), ["app.exe"]);
    }

    /// An AppImage's folder layout, with a place for a bare binary beside it.
    struct AppImageRig {
        _dir: tempfile::TempDir,
        appimage: PathBuf,
        appdir: PathBuf,
        exe_inside: PathBuf,
        bare_exe: PathBuf,
    }

    fn appimage_rig() -> AppImageRig {
        let dir = tempfile::tempdir().unwrap();
        let appimage = dir.path().join("Feeling.AppImage");
        fs::write(&appimage, b"appimage").unwrap();
        let appdir = dir.path().join(".mount_abc");
        let exe_inside = appdir.join("usr/bin/wow-is-a-feeling");
        fs::create_dir_all(exe_inside.parent().unwrap()).unwrap();
        fs::write(&exe_inside, b"exe").unwrap();
        let bare_exe = dir.path().join("wow-is-a-feeling");
        fs::write(&bare_exe, b"exe").unwrap();
        AppImageRig {
            _dir: dir,
            appimage,
            appdir,
            exe_inside,
            bare_exe,
        }
    }

    #[test]
    fn our_own_exe_inside_appdir_means_we_are_the_appimage() {
        let rig = appimage_rig();
        assert_eq!(
            confirmed_appimage(
                rig.appimage.as_os_str(),
                rig.appdir.as_os_str(),
                &rig.exe_inside
            ),
            Some(rig.appimage.clone())
        );
    }

    /// The review's case: a bare binary started from a terminal inside Cursor.AppImage, whose
    /// variables it inherits. Believing them would rename an update over Cursor.
    #[test]
    fn another_appimages_variables_dont_make_a_bare_binary_an_appimage() {
        let rig = appimage_rig();
        assert_eq!(
            confirmed_appimage(
                rig.appimage.as_os_str(),
                rig.appdir.as_os_str(),
                &rig.bare_exe
            ),
            None
        );
        assert_eq!(fs::read(&rig.appimage).unwrap(), b"appimage");
    }

    #[test]
    fn a_doubtful_appimage_path_isnt_believed() {
        let rig = appimage_rig();
        let (appdir, exe) = (rig.appdir.as_os_str(), rig.exe_inside.as_path());
        let confirmed = |appimage: &Path| confirmed_appimage(appimage.as_os_str(), appdir, exe);
        assert_eq!(confirmed(Path::new("Feeling.AppImage")), None, "relative");
        assert_eq!(confirmed(&rig.appimage.with_extension("gone")), None);
        assert_eq!(confirmed(&rig.appdir), None, "a folder");
        assert_eq!(
            confirmed_appimage(
                rig.appimage.as_os_str(),
                rig.appdir.join("gone").as_os_str(),
                exe
            ),
            None
        );
        assert_eq!(
            confirmed_appimage(rig.appimage.as_os_str(), OsStr::new(""), exe),
            None
        );
    }

    /// The runtime builds `$APPDIR` from `$TMPDIR` without resolving links, and the path of a
    /// running program always comes back resolved.
    #[cfg(unix)]
    #[test]
    fn an_appdir_reached_through_a_link_still_counts() {
        let rig = appimage_rig();
        let linked = rig.appdir.with_file_name("linked-mount");
        std::os::unix::fs::symlink(&rig.appdir, &linked).unwrap();
        assert_eq!(
            confirmed_appimage(
                rig.appimage.as_os_str(),
                linked.as_os_str(),
                &rig.exe_inside
            ),
            Some(rig.appimage.clone())
        );
    }

    #[test]
    fn cancelling_leaves_the_original_untouched() {
        let app = FakeApp::new();
        let body = vec![1u8; 1000];
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a", &body, AssetKind::Exe);
        let cancel = AtomicBool::new(false);
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &never_replace,
            &mut |_| cancel.store(true, Ordering::Relaxed),
            &cancel,
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        app.only_file_is_target();
    }

    #[test]
    fn a_bin_gz_update_is_unpacked_then_swapped() {
        let app = FakeApp::new();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"new linux binary").unwrap();
        let gz = encoder.finish().unwrap();
        let server = file_server(gz.clone());
        let asset = asset_for(server.base.clone() + "/a.gz", &gz, AssetKind::BinGz);
        let handed_over = Mutex::new(Vec::new());
        let replace = |new: &Path| {
            *handed_over.lock().unwrap() = fs::read(new)?;
            fs::copy(new, &app.target).map(drop)
        };
        apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &replace,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(*handed_over.lock().unwrap(), b"new linux binary");
        assert_eq!(fs::read(&app.target).unwrap(), b"new linux binary");
        app.only_file_is_target();
    }

    #[test]
    fn a_signed_but_damaged_gz_aborts() {
        let app = FakeApp::new();
        let body = b"not gzip at all".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(server.base.clone() + "/a.gz", &body, AssetKind::BinGz);
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &never_replace,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
        assert_eq!(fs::read(&app.target).unwrap(), b"original binary");
        app.only_file_is_target();
    }

    #[test]
    fn an_appimage_update_replaces_the_file_with_the_exec_bit_set() {
        let app = FakeApp::new();
        let body = b"new appimage".to_vec();
        let server = file_server(body.clone());
        let asset = asset_for(
            server.base.clone() + "/a.AppImage",
            &body,
            AssetKind::AppImage,
        );
        let applied = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app.target,
            &never_replace,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(applied.path, app.target);
        assert_eq!(fs::read(&app.target).unwrap(), body);
        app.only_file_is_target();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&app.target).unwrap().permissions().mode() & 0o111,
                0o111
            );
        }
    }

    fn fake_bundle() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("WoW is a Feeling.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/MacOS/wow-is-a-feeling"), b"old").unwrap();
        fs::write(app.join("Contents/old-only.txt"), b"old").unwrap();
        (dir, app)
    }

    #[test]
    fn an_app_zip_update_replaces_the_whole_bundle() {
        let (dir, app) = fake_bundle();
        let zip = zip_bytes(&[
            (
                "WoW is a Feeling.app/Contents/MacOS/wow-is-a-feeling",
                b"new",
            ),
            ("WoW is a Feeling.app/Contents/Info.plist", b"<plist/>"),
            ("__MACOSX/WoW is a Feeling.app/._Contents", b"junk"),
        ]);
        let server = file_server(zip.clone());
        let asset = asset_for(server.base.clone() + "/a.zip", &zip, AssetKind::AppZip);
        let applied = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app,
            &never_replace,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            applied,
            Applied {
                kind: AssetKind::AppZip,
                path: app.clone()
            }
        );
        assert_eq!(
            fs::read(app.join("Contents/MacOS/wow-is-a-feeling")).unwrap(),
            b"new"
        );
        assert!(app.join("Contents/Info.plist").is_file());
        assert!(!app.join("Contents/old-only.txt").exists());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|i| i.file_name())
            .collect();
        assert_eq!(
            names,
            ["WoW is a Feeling.app"],
            "stray files beside the app"
        );
    }

    #[test]
    fn app_zips_go_through_the_same_path_safety() {
        let (_dir, app) = fake_bundle();
        let zip = zip_bytes(&[
            ("WoW is a Feeling.app/Contents/x", b"new"),
            ("../../outside.txt", b"evil"),
        ]);
        let server = file_server(zip.clone());
        let asset = asset_for(server.base.clone() + "/a.zip", &zip, AssetKind::AppZip);
        let result = apply_to(
            &Http::allowing_loopback(),
            &asset,
            &app,
            &never_replace,
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(Error::UnsafeZip(_))), "{result:?}");
        assert_eq!(
            fs::read(app.join("Contents/MacOS/wow-is-a-feeling")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn an_app_zip_without_exactly_one_app_changes_nothing() {
        for files in [
            vec![("Something/else.txt", &b"x"[..])],
            vec![("A.app/x", &b"x"[..]), ("B.app/x", &b"x"[..])],
        ] {
            let (dir, app) = fake_bundle();
            let zip = zip_bytes(&files);
            let server = file_server(zip.clone());
            let asset = asset_for(server.base.clone() + "/a.zip", &zip, AssetKind::AppZip);
            let result = apply_to(
                &Http::allowing_loopback(),
                &asset,
                &app,
                &never_replace,
                &mut |_| {},
                &AtomicBool::new(false),
            );
            assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
            assert_eq!(
                fs::read(app.join("Contents/MacOS/wow-is-a-feeling")).unwrap(),
                b"old"
            );
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|i| i.file_name())
                .collect();
            assert_eq!(names, ["WoW is a Feeling.app"]);
        }
    }

    #[test]
    fn finding_the_bundle_of_a_running_binary() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/WoW is a Feeling.app/Contents/MacOS/wow-is-a-feeling"
            )),
            Some(PathBuf::from("/Applications/WoW is a Feeling.app"))
        );
        assert_eq!(
            bundle_of(Path::new("/usr/local/bin/wow-is-a-feeling")),
            None
        );
    }

    #[test]
    fn relaunch_command_per_kind() {
        let plain = Applied {
            kind: AssetKind::Exe,
            path: PathBuf::from("/x/wow-is-a-feeling"),
        };
        assert_eq!(
            relaunch_command(&plain).get_program(),
            plain.path.as_os_str()
        );
        assert_eq!(relaunch_command(&plain).get_args().count(), 0);

        let bundle = Applied {
            kind: AssetKind::AppZip,
            path: PathBuf::from("/Applications/W.app"),
        };
        let command = relaunch_command(&bundle);
        assert_eq!(command.get_program(), "open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-n", "/Applications/W.app"]);
    }

    #[test]
    fn relaunching_something_missing_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let applied = Applied {
            kind: AssetKind::Exe,
            path: dir.path().join("not-here"),
        };
        assert!(matches!(relaunch(applied), Err(Error::Io { .. })));
    }
}
