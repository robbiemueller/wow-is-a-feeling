//! Checks a catalog the way a player would find it: every reference resolves, and a sample
//! of entries really install, through the real installer, into a temp folder.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use feeling_core::catalog::{Addon, CATEGORIES, Catalog, Channel};
use feeling_core::install::{self, InstallRequest, Layout};
use feeling_core::net::{self, Http};
use feeling_core::scan;

/// Returns a summary, or an error if anything is wrong.
pub fn run(file: &Path, sample: usize, max_mb: u64) -> Result<String, String> {
    let bytes = fs::read(file).map_err(|e| format!("Couldn't read {}: {e}", file.display()))?;
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} isn't a catalog: {e}", file.display()))?;

    let problems = static_problems(&catalog);
    for problem in &problems {
        println!("BAD  {problem}");
    }

    let http = Http::new();
    let (picks, too_big) = pick(&catalog, sample, max_mb.saturating_mul(1024 * 1024));
    let mut failures = 0usize;
    for addon in &picks {
        match try_install(&http, addon) {
            Ok(note) => println!("ok   {}: {note}", addon.id),
            Err(why) => {
                failures += 1;
                println!("FAIL {}: {why}", addon.id);
            }
        }
    }
    let summary = format!(
        "{} addons listed, {} static problems. Installed {} of {} tried ({too_big} skipped for size).",
        catalog.addons.len(),
        problems.len(),
        picks.len() - failures,
        picks.len(),
    );
    if problems.is_empty() && failures == 0 {
        Ok(summary)
    } else {
        Err(summary)
    }
}

fn static_problems(catalog: &Catalog) -> Vec<String> {
    let mut problems = Vec::new();
    let mut ids = BTreeSet::new();
    for addon in &catalog.addons {
        if !ids.insert(addon.id.as_str()) {
            problems.push(format!("{} is listed twice", addon.id));
        }
        if addon.name.trim().is_empty() {
            problems.push(format!("{} has no name", addon.id));
        }
        if addon.folders.is_empty() {
            problems.push(format!("{} lists no folders", addon.id));
        }
        if !CATEGORIES.iter().any(|(id, _)| *id == addon.category) {
            problems.push(format!(
                "{} has an unknown category: {}",
                addon.id, addon.category
            ));
        }
        if addon.release(Channel::Stable).is_none() {
            problems.push(format!("{} has no release", addon.id));
        }
        for release in [&addon.stable, &addon.beta].into_iter().flatten() {
            if !net::is_public_url(&release.url) {
                problems.push(format!(
                    "{} has a link the app refuses: {}",
                    addon.id, release.url
                ));
            }
        }
    }
    let mut check = |list: &str, id: &str| {
        if !ids.contains(id) {
            problems.push(format!("{list} points at {id}, which isn't listed"));
        }
    };
    for id in &catalog.featured {
        check("featured", id);
    }
    for kit in &catalog.kits {
        for id in &kit.addons {
            check(&kit.name, id);
        }
    }
    for gap in &catalog.gaps {
        for id in &gap.alternatives {
            check(&gap.name, id);
        }
    }
    problems
}

/// The entries most likely to break or matter first, then an even spread of the rest.
fn pick(catalog: &Catalog, sample: usize, max_bytes: u64) -> (Vec<&Addon>, usize) {
    let by_id = |id: &String| catalog.addons.iter().find(|addon| addon.id == *id);
    let mut wanted: Vec<&Addon> = Vec::new();
    wanted.extend(catalog.addons.iter().filter(|addon| {
        !addon.id.starts_with("gh:")
            || addon
                .release(Channel::Stable)
                .is_some_and(|release| !release.moves.is_empty())
    }));
    wanted.extend(catalog.featured.iter().filter_map(by_id));
    wanted.extend(
        catalog
            .kits
            .iter()
            .flat_map(|kit| kit.addons.iter())
            .filter_map(by_id),
    );
    let mut rest: Vec<&Addon> = catalog.addons.iter().collect();
    rest.sort_by_key(|addon| std::cmp::Reverse(addon.downloads));
    let step = (rest.len() / sample.max(1)).max(1);
    wanted.extend(rest.into_iter().step_by(step));

    let mut seen = BTreeSet::new();
    let mut too_big = 0;
    let mut picks = Vec::new();
    for addon in wanted {
        if picks.len() == sample {
            break;
        }
        if !seen.insert(addon.id.as_str()) {
            continue;
        }
        if addon
            .release(Channel::Stable)
            .is_some_and(|release| release.size > max_bytes)
        {
            too_big += 1;
            continue;
        }
        picks.push(addon);
    }
    (picks, too_big)
}

fn try_install(http: &Http, addon: &Addon) -> Result<String, String> {
    let release = addon
        .release(Channel::Stable)
        .ok_or_else(|| "no release".to_string())?;
    let dir = tempfile::tempdir().map_err(|e| format!("no temp folder: {e}"))?;
    let addons_dir = dir.path().join("Interface").join("AddOns");
    fs::create_dir_all(&addons_dir).map_err(|e| format!("no temp folder: {e}"))?;
    let layout = Layout::from_addons_dir(&addons_dir).map_err(|e| e.to_string())?;
    let request = InstallRequest::for_release(&addon.id, release);
    let outcome = install::install_from_url(
        http,
        &layout,
        &request,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .map_err(|e| e.to_string())?;

    let names = |list: &[String]| -> BTreeSet<String> {
        list.iter().map(|name| name.to_ascii_lowercase()).collect()
    };
    let (found, promised) = (names(&outcome.folders), names(&addon.folders));
    if found != promised {
        return Err(format!(
            "installs {found:?} but the catalog says {promised:?}"
        ));
    }
    if outcome.forever_folders.is_empty() {
        return Err("installs, but no folder targets Forever".to_string());
    }
    let scanned = scan::scan_addons(&addons_dir).map_err(|e| e.to_string())?;
    let without_toc: Vec<&str> = scanned
        .iter()
        .filter(|folder| folder.toc_file.is_none())
        .map(|folder| folder.name.as_str())
        .collect();
    if !without_toc.is_empty() {
        return Err(format!("no TOC the game can load in {without_toc:?}"));
    }
    Ok(format!(
        "{} folders, {} for Forever, {} files left out",
        outcome.folders.len(),
        outcome.forever_folders.len(),
        outcome.skipped_files.len()
    ))
}
