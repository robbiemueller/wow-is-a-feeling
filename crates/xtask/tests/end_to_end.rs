//! The release pipeline against the app's own check: if the real `xtask` binary's output
//! passes `selfupdate`, then what CI publishes is what the app will accept.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use feeling_core::selfupdate::verified_manifest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn run(args: &[&str], secret_key: Option<&str>) -> Output {
    let mut command = Command::new(XTASK);
    command.args(args).env_remove("MINISIGN_SECRET_KEY");
    if let Some(key) = secret_key {
        command.env("MINISIGN_SECRET_KEY", key);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Make a key with the binary and return its public half and the secret file's text.
fn keygen(dir: &Path) -> (String, String) {
    let key_file = dir.join("test.minisign.key");
    let output = run(&["keygen", key_file.to_str().unwrap()], None);
    assert!(output.status.success(), "{}", text(&output.stderr));
    (
        text(&output.stdout).trim().to_string(),
        fs::read_to_string(key_file).unwrap(),
    )
}

fn release_folder(dir: &Path) -> PathBuf {
    let assets = dir.join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(
        assets.join("wow-is-a-feeling-windows-x64.exe"),
        b"windows build",
    )
    .unwrap();
    fs::write(
        assets.join("wow-is-a-feeling-linux-x64.AppImage"),
        b"linux build",
    )
    .unwrap();
    fs::write(dir.join("notes.md"), "Plain words about what changed.\n").unwrap();
    assets
}

fn make_latest(dir: &Path, assets: &Path) -> Output {
    let notes = dir.join("notes.md");
    run(
        &[
            "make-latest",
            "--version",
            "0.2.0",
            "--notes-file",
            notes.to_str().unwrap(),
            "--assets-dir",
            assets.to_str().unwrap(),
            "--base-url",
            "https://example.test/releases/download/v0.2.0",
        ],
        None,
    )
}

#[test]
fn keygen_prints_only_the_public_key() {
    let dir = tempfile::tempdir().unwrap();
    let (public_key, secret_file) = keygen(dir.path());

    assert_eq!(
        public_key.lines().count(),
        1,
        "stdout should be the public key alone"
    );
    assert!(public_key.starts_with("RW"), "{public_key}");
    for secret_line in secret_file
        .lines()
        .filter(|line| !line.starts_with("untrusted comment"))
    {
        assert!(
            !public_key.contains(secret_line.trim()),
            "the secret key reached stdout"
        );
    }
    assert_ne!(secret_file.trim(), public_key);
}

#[test]
fn sign_and_make_latest_pass_the_apps_verification() {
    let dir = tempfile::tempdir().unwrap();
    let (public_key, secret_key) = keygen(dir.path());
    let assets = release_folder(dir.path());

    let output = make_latest(dir.path(), &assets);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let latest = assets.join("latest.json");

    let output = run(&["sign", latest.to_str().unwrap()], Some(&secret_key));
    assert!(output.status.success(), "{}", text(&output.stderr));
    let printed = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        !printed.contains(secret_key.lines().nth(1).unwrap()),
        "sign printed the secret key"
    );

    let json = fs::read(&latest).unwrap();
    let signature = fs::read(assets.join("latest.json.minisig")).unwrap();
    let manifest = verified_manifest(&json, &signature, &public_key).unwrap();
    assert_eq!(manifest.version, "0.2.0");
    assert_eq!(manifest.notes, "Plain words about what changed.");
    assert_eq!(manifest.assets.len(), 2);

    let windows = &manifest.assets["windows-x64"];
    assert_eq!(windows.kind, "exe");
    assert_eq!(windows.size, 13);
    assert_eq!(
        windows.url,
        "https://example.test/releases/download/v0.2.0/wow-is-a-feeling-windows-x64.exe"
    );
    assert_eq!(windows.sha256.len(), 64);
    assert_eq!(manifest.assets["linux-x64-appimage"].kind, "appimage");

    // The signature is over the exact bytes, so one changed byte fails.
    let tampered = text(&json).replace("0.2.0", "0.2.1");
    assert!(verified_manifest(tampered.as_bytes(), &signature, &public_key).is_err());

    // And another key doesn't vouch for it.
    let other = tempfile::tempdir().unwrap();
    let (other_public_key, _) = keygen(other.path());
    assert!(verified_manifest(&json, &signature, &other_public_key).is_err());
}

#[test]
fn sign_without_the_key_explains_itself_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("latest.json");
    fs::write(&file, b"{}").unwrap();
    let output = run(&["sign", file.to_str().unwrap()], None);
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("MINISIGN_SECRET_KEY"));
    assert!(!dir.path().join("latest.json.minisig").exists());
}

#[test]
fn a_bad_key_is_refused_without_being_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("latest.json");
    fs::write(&file, b"{}").unwrap();
    let output = run(
        &["sign", file.to_str().unwrap()],
        Some("hunter2-not-really-a-key"),
    );
    assert!(!output.status.success());
    let printed = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(!printed.contains("hunter2"), "{printed}");
}

#[test]
fn unknown_commands_print_usage() {
    let output = run(&["launch-the-missiles"], None);
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("keygen"));
}
