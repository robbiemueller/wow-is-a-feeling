//! Release chores: signing keys, signatures and `latest.json`. Run with
//! `cargo run -p xtask -- <command>`.
//!
//! The secret key never appears in any output, and `sign` only takes it from the
//! environment, so it can live in a CI secret and never touch a command line. CI itself
//! signs with `xtask-sign` instead, so that far fewer crates get to run beside the key.

mod check_catalog;
mod signing;

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use feeling_core::selfupdate::{self, UPDATE_PUBLIC_KEY};
use minisign::KeyPair;
use serde_json::json;
use sha2::{Digest, Sha256};
use signing::{sign_from_env, unix_now};

const USAGE: &str = "\
Release chores for WoW is a Feeling.

  keygen <secret-key-file>
      Write a new secret key (no password) and print the PUBLIC key only.
      Refuses to overwrite an existing file.
  sign <file>
      Write <file>.minisig, using the key file's contents from MINISIGN_SECRET_KEY.
  make-latest --version <v> --notes-file <file> --assets-dir <dir> --base-url <url>
      Write <dir>/latest.json with the sha256 and size of each known asset found.
  verify-latest <latest.json>
      Check <latest.json>.minisig against the public key built into the app.
  check-catalog <catalog.json> <how-many> <max-megabytes>
      Check a catalog's references, and really install a sample of it into temp folders.
";

/// Published file name, then its `latest.json` key and kind.
const ASSETS: [(&str, &str, &str); 4] = [
    ("wow-is-a-feeling-windows-x64.exe", "windows-x64", "exe"),
    (
        "wow-is-a-feeling-linux-x64.AppImage",
        "linux-x64-appimage",
        "appimage",
    ),
    ("wow-is-a-feeling-linux-x64.gz", "linux-x64-bin", "bin.gz"),
    (
        "wow-is-a-feeling-macos-universal.zip",
        "macos-universal",
        "app.zip",
    ),
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["keygen", path] => keygen(Path::new(path)).map(|public_key| {
            println!("{public_key}");
        }),
        ["sign", file] => sign_from_env(Path::new(file))
            .map(|signature| println!("Wrote {}", signature.display())),
        ["make-latest", options @ ..] => parse_make_latest(options)
            .and_then(|options| make_latest(&options))
            .map(|latest| println!("Wrote {}", latest.display())),
        ["verify-latest", file] => {
            verify_latest(Path::new(file), UPDATE_PUBLIC_KEY).map(|summary| println!("{summary}"))
        }
        ["check-catalog", file, how_many, max_megabytes] => {
            match (how_many.parse(), max_megabytes.parse()) {
                (Ok(how_many), Ok(max_megabytes)) => {
                    check_catalog::run(Path::new(file), how_many, max_megabytes)
                        .map(|summary| println!("{summary}"))
                }
                _ => Err(USAGE.to_string()),
            }
        }
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Write a new secret key to `path` and return the public key. Owner-only on Unix.
fn keygen(path: &Path) -> Result<String, String> {
    let pair = KeyPair::generate_unencrypted_keypair()
        .map_err(|e| format!("Couldn't generate a key: {e}"))?;
    let secret = pair
        .sk
        .to_box(None)
        .map_err(|e| format!("Couldn't encode the key: {e}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("Couldn't create {}: {e}", path.display()))?;
    file.write_all(&secret.to_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    Ok(pair.pk.to_base64())
}

/// Check a signed `latest.json` the way the app will, so a release signed with the wrong
/// key fails here rather than on every player's machine.
fn verify_latest(file: &Path, public_key: &str) -> Result<String, String> {
    let json = fs::read(file).map_err(|e| format!("Couldn't read {}: {e}", file.display()))?;
    let mut signature_path = file.as_os_str().to_os_string();
    signature_path.push(".minisig");
    let signature_path = PathBuf::from(signature_path);
    let signature = fs::read(&signature_path)
        .map_err(|e| format!("Couldn't read {}: {e}", signature_path.display()))?;
    let manifest = selfupdate::verified_manifest(&json, &signature, public_key)
        .map_err(|e| format!("{} failed verification: {e}", file.display()))?;
    if manifest.assets.is_empty() {
        return Err(format!("{} lists no downloads.", file.display()));
    }
    Ok(format!(
        "{} verifies: version {}, {} downloads",
        file.display(),
        manifest.version,
        manifest.assets.len()
    ))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MakeLatest {
    version: String,
    notes_file: PathBuf,
    assets_dir: PathBuf,
    base_url: String,
}

fn parse_make_latest(args: &[&str]) -> Result<MakeLatest, String> {
    let mut parsed = MakeLatest::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value.\n\n{USAGE}"))?;
        match *flag {
            "--version" => parsed.version = value.to_string(),
            "--notes-file" => parsed.notes_file = PathBuf::from(value),
            "--assets-dir" => parsed.assets_dir = PathBuf::from(value),
            "--base-url" => parsed.base_url = value.to_string(),
            _ => return Err(format!("Unknown option {flag}.\n\n{USAGE}")),
        }
    }
    let complete = !parsed.version.is_empty()
        && !parsed.notes_file.as_os_str().is_empty()
        && !parsed.assets_dir.as_os_str().is_empty()
        && !parsed.base_url.is_empty();
    if complete {
        Ok(parsed)
    } else {
        Err(format!("make-latest needs all four options.\n\n{USAGE}"))
    }
}

/// Describe every known asset present in the folder. No assets at all is an error,
/// because publishing an empty `latest.json` would strand everyone on the old version.
fn make_latest(options: &MakeLatest) -> Result<PathBuf, String> {
    let notes = fs::read_to_string(&options.notes_file)
        .map_err(|e| format!("Couldn't read {}: {e}", options.notes_file.display()))?;
    let base_url = options.base_url.trim_end_matches('/');
    let mut assets = BTreeMap::new();
    for (file_name, key, kind) in ASSETS {
        let path = options.assets_dir.join(file_name);
        if !path.is_file() {
            continue;
        }
        let (sha256, size) = hash_file(&path)?;
        assets.insert(
            key,
            json!({ "url": format!("{base_url}/{file_name}"), "sha256": sha256, "size": size, "kind": kind }),
        );
    }
    if assets.is_empty() {
        return Err(format!(
            "No release assets found in {}.",
            options.assets_dir.display()
        ));
    }
    let latest = json!({
        "version": options.version,
        "notes": notes.trim(),
        "date": unix_now(),
        "assets": assets,
    });
    let path = options.assets_dir.join("latest.json");
    let text = serde_json::to_string_pretty(&latest).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    Ok(path)
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let error = |e: std::io::Error| format!("Couldn't read {}: {e}", path.display());
    let mut file = File::open(path).map_err(error)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(error)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((sha256, size))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::signing::sign_file;
    use super::*;

    fn options(dir: &Path) -> MakeLatest {
        let notes = dir.join("notes.md");
        fs::write(&notes, "  What changed, in plain words.\n\n").unwrap();
        MakeLatest {
            version: "0.2.0".into(),
            notes_file: notes,
            assets_dir: dir.to_path_buf(),
            base_url: "https://example.test/download/".into(),
        }
    }

    #[test]
    fn make_latest_describes_only_the_assets_it_finds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("wow-is-a-feeling-windows-x64.exe"), b"exe").unwrap();
        fs::write(dir.path().join("wow-is-a-feeling-linux-x64.gz"), b"gzip").unwrap();
        fs::write(dir.path().join("something-else.exe"), b"ignored").unwrap();

        let latest = make_latest(&options(dir.path())).unwrap();
        assert_eq!(latest, dir.path().join("latest.json"));
        let value: serde_json::Value = serde_json::from_slice(&fs::read(latest).unwrap()).unwrap();
        assert_eq!(value["version"], "0.2.0");
        assert_eq!(value["notes"], "What changed, in plain words.");
        assert!(value["date"].as_u64().unwrap() > 1_700_000_000);

        let assets = value["assets"].as_object().unwrap();
        assert_eq!(assets.len(), 2);
        let exe = &assets["windows-x64"];
        assert_eq!(
            exe["url"],
            "https://example.test/download/wow-is-a-feeling-windows-x64.exe"
        );
        assert_eq!(exe["kind"], "exe");
        assert_eq!(exe["size"], 3);
        let expected: String = Sha256::digest(b"exe")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(exe["sha256"], expected);
        assert_eq!(assets["linux-x64-bin"]["kind"], "bin.gz");
    }

    #[test]
    fn every_asset_name_maps_to_the_right_key_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        for (name, _, _) in ASSETS {
            fs::write(dir.path().join(name), name).unwrap();
        }
        let latest = make_latest(&options(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(latest).unwrap()).unwrap();
        for (key, kind) in [
            ("windows-x64", "exe"),
            ("linux-x64-appimage", "appimage"),
            ("linux-x64-bin", "bin.gz"),
            ("macos-universal", "app.zip"),
        ] {
            assert_eq!(value["assets"][key]["kind"], kind, "{key}");
        }
    }

    /// Every `wow-is-a-feeling-...` file name in a text.
    fn release_files(text: &str) -> BTreeSet<String> {
        text.match_indices("wow-is-a-feeling-")
            .map(|(at, _)| {
                text[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
                    .collect::<String>()
            })
            .filter(|name| {
                [".exe", ".gz", ".AppImage", ".zip", ".dmg"]
                    .iter()
                    .any(|extension| name.ends_with(extension))
            })
            .collect()
    }

    /// These names live in five places, and if they drift an update or a download link breaks
    /// for everyone on that platform, which nothing else would catch before a real release.
    #[test]
    fn asset_names_agree_everywhere_they_appear() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let read = |path: &str| fs::read_to_string(root.join(path)).unwrap();

        // What the release publishes, and what its build jobs actually make.
        let workflow = read(".github/workflows/release.yml");
        let (gather, rest): (Vec<&str>, Vec<&str>) = workflow
            .lines()
            .partition(|line| line.trim_start().starts_with("take "));
        let published = release_files(&gather.join("\n"));
        assert_eq!(published.len(), 6, "{published:?}");
        assert_eq!(release_files(&rest.join("\n")), published);

        // What xtask describes in latest.json.
        for (name, _, _) in ASSETS {
            assert!(published.contains(name), "{name} isn't published");
        }

        // What the landing page and the README link to.
        let site = release_files(&read("site/index.html"));
        assert!(site.len() >= 3, "{site:?}");
        for name in site.iter().chain(&release_files(&read("README.md"))) {
            assert!(
                published.contains(name),
                "{name} is linked to, not published"
            );
        }

        // What the app asks for.
        let selfupdate = read("crates/core/src/selfupdate.rs");
        for (_, key, kind) in ASSETS {
            assert!(selfupdate.contains(&format!("\"{key}\"")), "{key}");
            assert!(selfupdate.contains(&format!("\"{kind}\"")), "{kind}");
        }
        if let Some(wanted) = selfupdate::asset_key() {
            assert!(ASSETS.iter().any(|(_, key, _)| *key == wanted), "{wanted}");
        }
    }

    #[test]
    fn make_latest_with_no_assets_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            make_latest(&options(dir.path()))
                .unwrap_err()
                .contains("No release assets")
        );
        assert!(!dir.path().join("latest.json").exists());
    }

    #[test]
    fn make_latest_options_are_parsed_and_required() {
        let args = [
            "--version",
            "1",
            "--notes-file",
            "n",
            "--assets-dir",
            "d",
            "--base-url",
            "u",
        ];
        let parsed = parse_make_latest(&args).unwrap();
        assert_eq!(parsed.version, "1");
        assert_eq!(parsed.assets_dir, PathBuf::from("d"));
        assert!(parse_make_latest(&args[..6]).is_err());
        assert!(parse_make_latest(&["--version"]).is_err());
        assert!(parse_make_latest(&["--what", "x"]).is_err());
    }

    fn signed_manifest(dir: &Path) -> (PathBuf, String) {
        let key_path = dir.join("k.minisign.key");
        let public_key = keygen(&key_path).unwrap();
        let secret_key = fs::read_to_string(&key_path).unwrap();
        let file = dir.join("latest.json");
        let manifest = json!({
            "version": "1.2.3",
            "notes": "",
            "date": 1,
            "assets": {
                "windows-x64": {
                    "url": "https://example.com/a.exe",
                    "sha256": "00",
                    "size": 1,
                    "kind": "exe"
                }
            }
        });
        fs::write(&file, manifest.to_string()).unwrap();
        sign_file(&file, &secret_key).unwrap();
        (file, public_key)
    }

    #[test]
    fn a_manifest_signed_with_the_matching_key_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let (file, public_key) = signed_manifest(dir.path());
        let summary = verify_latest(&file, &public_key).unwrap();
        assert!(summary.contains("version 1.2.3"), "{summary}");
    }

    #[test]
    fn a_secret_key_with_windows_line_endings_still_signs() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.minisign.key");
        let public_key = keygen(&key_path).unwrap();
        let secret_key = fs::read_to_string(&key_path).unwrap().replace('\n', "\r\n");
        let file = dir.path().join("latest.json");
        let manifest = json!({"version": "1.2.3", "notes": "", "date": 1, "assets": {
            "windows-x64": {"url": "https://example.com/a.exe", "sha256": "00", "size": 1, "kind": "exe"}
        }});
        fs::write(&file, manifest.to_string()).unwrap();
        sign_file(&file, &secret_key).unwrap();
        verify_latest(&file, &public_key).unwrap();
    }

    #[test]
    fn a_manifest_signed_with_another_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (file, _) = signed_manifest(dir.path());
        assert!(verify_latest(&file, UPDATE_PUBLIC_KEY).is_err());
    }

    #[test]
    fn a_changed_manifest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (file, public_key) = signed_manifest(dir.path());
        let changed = fs::read_to_string(&file).unwrap().replace("1.2.3", "9.9.9");
        fs::write(&file, changed).unwrap();
        assert!(verify_latest(&file, &public_key).is_err());
    }

    #[test]
    fn keygen_never_overwrites_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.minisign.key");
        keygen(&path).unwrap();
        let first = fs::read(&path).unwrap();
        assert!(keygen(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), first);
    }

    #[test]
    fn signing_refuses_nonsense_keys_without_repeating_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("latest.json");
        fs::write(&file, b"{}").unwrap();
        let error = sign_file(&file, "sekrit-not-a-key").unwrap_err();
        assert!(!error.contains("sekrit"), "{error}");
        assert!(!dir.path().join("latest.json.minisig").exists());
    }

    #[test]
    fn signing_refuses_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.minisign.key");
        keygen(&key_path).unwrap();
        let key = fs::read_to_string(&key_path).unwrap();
        assert!(sign_file(&dir.path().join("missing.json"), &key).is_err());
    }
}
