//! Signs one file with the key in `MINISIGN_SECRET_KEY`, and can do nothing else.
//!
//! This is the only program the release workflow runs with the key present. A crate linked
//! into a program can read that program's environment, so CI builds this one by itself:
//! `cargo build -p xtask --no-default-features --bin xtask-sign` compiles minisign's crates
//! and none of the others.

mod signing;

use std::env;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [file] = args.as_slice() else {
        eprintln!(
            "Usage: xtask-sign <file>\nWrites <file>.minisig, using the key file's contents from {}.",
            signing::SECRET_KEY_VAR
        );
        return ExitCode::FAILURE;
    };
    match signing::sign_from_env(Path::new(file)) {
        Ok(signature) => {
            println!("Wrote {}", signature.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
