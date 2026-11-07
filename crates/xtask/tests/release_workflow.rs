//! The release workflow only runs when a tag is pushed, and a mistake in it can reach every
//! player, so these tests read the real workflow files.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow(name: &str) -> String {
    fs::read_to_string(repo_root().join(".github/workflows").join(name)).unwrap()
}

/// Every workflow file, by name.
fn all_workflows() -> Vec<(String, String)> {
    let dir = repo_root().join(".github/workflows");
    let mut workflows: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(path).unwrap())
        })
        .collect();
    workflows.sort();
    assert!(workflows.len() >= 4, "{workflows:?}");
    workflows
}

/// What a line says after `uses: `, with or without the `- ` that starts a step.
fn uses_value(line: &str) -> Option<&str> {
    line.trim_start()
        .trim_start_matches("- ")
        .strip_prefix("uses: ")
}

#[test]
fn every_action_is_pinned_to_a_full_commit_with_its_version_noted() {
    let mut pinned = 0;
    for (name, text) in all_workflows() {
        for value in text.lines().filter_map(uses_value) {
            let (reference, version) = value
                .split_once(" # ")
                .unwrap_or_else(|| panic!("{name}: `{value}` doesn't say which version it is"));
            let (_, commit) = reference
                .split_once('@')
                .unwrap_or_else(|| panic!("{name}: `{value}` isn't pinned"));
            assert!(
                commit.len() == 40
                    && commit
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{name}: `{value}` is pinned to something that can move"
            );
            assert!(!version.trim().is_empty(), "{name}: `{value}`");
            pinned += 1;
        }
    }
    assert!(pinned >= 10, "only found {pinned} actions");
}

/// The action's master branch has no default toolchain, and exits with an error without one.
#[test]
fn every_rust_toolchain_step_names_its_toolchain() {
    for (name, text) in all_workflows() {
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            if uses_value(line).is_some_and(|value| value.starts_with("dtolnay/rust-toolchain@")) {
                let inputs = lines[at + 1..]
                    .iter()
                    .take_while(|next| !next.trim_start().starts_with("- "));
                assert!(
                    inputs
                        .into_iter()
                        .any(|input| input.trim_start().starts_with("toolchain: ")),
                    "{name}, line {}",
                    at + 1
                );
            }
        }
    }
}
