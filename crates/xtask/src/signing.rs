//! Signing with the secret key, shared by `xtask` and the small `xtask-sign`. It only uses
//! minisign, so `xtask-sign` can be built without any of the crates the rest of xtask needs.

use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use minisign::SecretKeyBox;

pub const SECRET_KEY_VAR: &str = "MINISIGN_SECRET_KEY";

/// Sign `file` with the key text in [`SECRET_KEY_VAR`].
pub fn sign_from_env(file: &Path) -> Result<PathBuf, String> {
    let secret_key = env::var(SECRET_KEY_VAR).map_err(|_| {
        format!("{SECRET_KEY_VAR} isn't set. It should hold the secret key file's contents.")
    })?;
    sign_file(file, &secret_key)
}

/// Sign `file` with the key text and write `<file>.minisig` beside it.
pub fn sign_file(file: &Path, secret_key: &str) -> Result<PathBuf, String> {
    // Errors here never include the key, only that it couldn't be read.
    let secret = SecretKeyBox::from_string(secret_key.trim())
        .and_then(SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|_| format!("{SECRET_KEY_VAR} isn't a usable secret key without a password."))?;
    let data = fs::read(file).map_err(|e| format!("Couldn't read {}: {e}", file.display()))?;
    let name = file
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let trusted_comment = format!("timestamp:{}\tfile:{name}", unix_now());
    let signature = minisign::sign(
        None,
        &secret,
        Cursor::new(data),
        Some(&trusted_comment),
        None,
    )
    .map_err(|e| format!("Couldn't sign {}: {e}", file.display()))?;
    let mut path = file.as_os_str().to_os_string();
    path.push(".minisig");
    let path = PathBuf::from(path);
    fs::write(&path, signature.into_string())
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
