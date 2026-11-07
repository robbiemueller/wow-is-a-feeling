//! Checks a catalog the way a player would find it: every reference resolves, and a sample
//! of entries really install, through the real installer, into a temp folder.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use feeling_core::catalog::{Addon, CATEGORIES, Catalog, Channel, PackGroup, Source};
use feeling_core::deps::BIGGEST_UNASKED_DOWNLOAD;
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
        problems.extend(github_id_problem(addon));
        problems.extend(formerly_problems(addon));
        for group in &addon.packs {
            if group.name.trim().is_empty() {
                problems.push(format!("{} has a group of packs with no name", addon.id));
            }
            if group.ids.is_empty() {
                problems.push(format!(
                    "{}'s {} packs name no addons",
                    addon.id, group.name
                ));
            }
            problems.extend(met_by_problems(addon, group));
        }
        for release in [&addon.stable, &addon.beta].into_iter().flatten() {
            if !net::is_public_url(&release.url) {
                problems.push(format!(
                    "{} has a link the app refuses: {}",
                    addon.id, release.url
                ));
            } else if !is_listed_download_host(&release.url) {
                problems.push(format!(
                    "{} downloads from a site the README doesn't list: {}",
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
    for addon in &catalog.addons {
        for group in &addon.packs {
            for id in &group.ids {
                check(&format!("{}'s {} packs", addon.id, group.name), id);
            }
        }
    }
    problems.extend(formerly_shipped_elsewhere(catalog));
    problems.extend(big_packs_reviewed(catalog));
    problems
}

/// Apps up to 0.1.2 install any reviewed entry another addon requires by themselves, whatever
/// its size, so a pack over 100 MB, any id some entry's pack groups name, is never reviewed.
fn big_packs_reviewed(catalog: &Catalog) -> Vec<String> {
    let packs: BTreeSet<String> = catalog
        .addons
        .iter()
        .flat_map(|addon| addon.packs.iter())
        .flat_map(|group| group.ids.iter())
        .map(|id| id.to_ascii_lowercase())
        .collect();
    catalog
        .addons
        .iter()
        .filter(|addon| addon.reviewed && packs.contains(&addon.id.to_ascii_lowercase()))
        .filter_map(|addon| {
            let size = [&addon.stable, &addon.beta]
                .into_iter()
                .flatten()
                .map(|release| release.size)
                .max()?;
            (size > BIGGEST_UNASKED_DOWNLOAD).then(|| {
                format!(
                    "{} is a pack of {size} bytes and reviewed, so apps up to 0.1.2 would install it by themselves",
                    addon.id
                )
            })
        })
        .collect()
}

/// A former main folder vouches that an installed copy with it is this addon, so it can never
/// be a folder another entry ships: that folder is the other addon's.
fn formerly_shipped_elsewhere(catalog: &Catalog) -> Vec<String> {
    let mut problems = Vec::new();
    for addon in &catalog.addons {
        for folder in &addon.formerly {
            let other = catalog.addons.iter().find(|other| {
                other.id != addon.id
                    && other
                        .folders
                        .iter()
                        .any(|f| f.to_lowercase() == folder.to_lowercase())
            });
            if let Some(other) = other {
                problems.push(format!(
                    "{} lists {folder} as a former main folder, but {} ships it",
                    addon.id, other.id
                ));
            }
        }
    }
    problems
}

/// A `gh:` id is a repository, `gh:owner/repo`, or one of its release lines,
/// `gh:owner/repo/<line>`. A line is lowercase letters, digits and dashes, because shared
/// addon lists carry it, and it comes from the plain repository.
fn github_id_problem(addon: &Addon) -> Option<String> {
    let rest = addon.id.strip_prefix("gh:")?;
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [_, _] => None,
        [owner, repo, line] => {
            let plain_line = !line.is_empty()
                && line
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !plain_line {
                return Some(format!(
                    "{} has a line part that isn't lowercase letters, digits and dashes",
                    addon.id
                ));
            }
            let expected = format!("{owner}/{repo}");
            match &addon.source {
                Source::Github { repo } if *repo == expected => None,
                _ => Some(format!(
                    "{} is a line of {expected}, but its source isn't that repository",
                    addon.id
                )),
            }
        }
        _ => Some(format!(
            "{} isn't gh:owner/repo or gh:owner/repo/<line>",
            addon.id
        )),
    }
}

/// The most former main folders an entry keeps, as the builder has it.
const MAX_FORMERLY: usize = 8;
/// The most `met_by` prefixes a pack group takes, as the builder has it.
const MAX_MET_BY: usize = 8;

/// The app counts an installed copy whose main folder is in `formerly` as this addon, so the
/// list is short, plain folder names, and never the main folder the entry has now.
fn formerly_problems(addon: &Addon) -> Vec<String> {
    let id = &addon.id;
    let mut problems = Vec::new();
    if addon.formerly.len() > MAX_FORMERLY {
        problems.push(format!(
            "{id} has {} former main folders, more than {MAX_FORMERLY}",
            addon.formerly.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for folder in &addon.formerly {
        if !is_folder_name(folder) {
            problems.push(format!(
                "{id} has a former main folder that can't be a folder name: {folder:?}"
            ));
        }
        if !seen.insert(folder.to_ascii_lowercase()) {
            problems.push(format!("{id} lists {folder} as a former main folder twice"));
        }
        if addon
            .folders
            .first()
            .is_some_and(|main| main.eq_ignore_ascii_case(folder))
        {
            problems.push(format!(
                "{id} lists its main folder {folder} as a former one"
            ));
        }
    }
    problems
}

/// The app counts any folder starting with one of a group's `met_by` prefixes as meeting it,
/// so an empty or odd prefix could meet it for anything.
fn met_by_problems(addon: &Addon, group: &PackGroup) -> Vec<String> {
    let mut problems = Vec::new();
    if group.met_by.len() > MAX_MET_BY {
        problems.push(format!(
            "{}'s {} packs have {} met_by prefixes, more than {MAX_MET_BY}",
            addon.id,
            group.name,
            group.met_by.len()
        ));
    }
    for prefix in &group.met_by {
        if !is_folder_name(prefix) {
            problems.push(format!(
                "{}'s {} packs have a met_by prefix that can't start a folder name: {prefix:?}",
                addon.id, group.name
            ));
        }
        // The addon's own folders would meet its own group, and hide that nothing else does.
        let lower = prefix.to_lowercase();
        if let Some(own) = addon
            .folders
            .iter()
            .find(|folder| folder.to_lowercase().starts_with(&lower))
        {
            problems.push(format!(
                "{}'s {} packs have a met_by prefix, {prefix}, that its own folder {own} matches",
                addon.id, group.name
            ));
        }
    }
    problems
}

/// A name that's safe for one folder under AddOns on every OS, by the same rule the builder's
/// gate uses, so a catalog that came some other way is held to it too.
fn is_folder_name(name: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let bad_char = |c: char| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    };
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 120
        && !name.starts_with(['.', ' '])
        && !name.ends_with(['.', ' '])
        && !name.chars().any(bad_char)
        && !DEVICES.contains(&stem.as_str())
}

/// The download sites README.md lists under "What it connects to". A catalog that sends
/// players anywhere else would make that list untrue, so it fails this check.
const LISTED_DOWNLOAD_HOSTS: [&str; 5] = [
    "github.com",
    "githubusercontent.com",
    "wowinterface.com",
    "mmoui.com",
    "foreverdb.net",
];

fn is_listed_download_host(url: &str) -> bool {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    LISTED_DOWNLOAD_HOSTS
        .iter()
        .any(|listed| host == *listed || host.ends_with(&format!(".{listed}")))
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

#[cfg(test)]
mod tests {
    use feeling_core::catalog::Release;

    use super::*;

    fn entry(id: &str, repo: &str) -> Addon {
        Addon {
            id: id.into(),
            name: "Spoken".into(),
            category: "quests".into(),
            source: Source::Github { repo: repo.into() },
            folders: vec!["Spoken".into()],
            stable: Some(Release {
                version: "v1".into(),
                url: format!("https://github.com/{repo}/releases/download/v1/a.zip"),
                ..Release::default()
            }),
            ..Addon::default()
        }
    }

    fn catalog(addons: Vec<Addon>) -> Catalog {
        Catalog {
            addons,
            ..Catalog::default()
        }
    }

    const SPOKEN: &str = "rusty-key/spoken-wow";

    #[test]
    fn a_former_main_folder_must_not_be_another_entrys_folder() {
        let mut renamed = entry("gh:a/new", "a/new");
        renamed.folders = vec!["NewName".into()];
        renamed.formerly = vec!["SPOKEN".into()];
        let spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        let problems = static_problems(&catalog(vec![renamed, spoken]));
        assert!(
            problems.iter().any(|p| p
                == "gh:a/new lists SPOKEN as a former main folder, but gh:rusty-key/spoken-wow ships it"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_met_by_prefix_must_not_match_its_own_addons_folders() {
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        let mut quests = entry("gh:rusty-key/spoken-wow/quests-audio", SPOKEN);
        quests.folders = vec!["SpokenQuestsAudioAlliance".into()];
        spoken.packs = vec![PackGroup {
            name: "Quest voices".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
            met_by: vec!["Spoken".into()],
        }];
        let problems = static_problems(&catalog(vec![spoken, quests]));
        assert!(
            problems.iter().any(|p| p.contains(
                "packs have a met_by prefix, Spoken, that its own folder Spoken matches"
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn a_healthy_addon_with_packs_has_no_problems() {
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        spoken.packs = vec![PackGroup {
            name: "Quest voices".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
            met_by: Vec::new(),
        }];
        let quests = entry("gh:rusty-key/spoken-wow/quests-audio", SPOKEN);
        let problems = static_problems(&catalog(vec![spoken, quests]));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn packs_must_name_listed_addons_and_have_a_name() {
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        spoken.packs = vec![
            PackGroup {
                name: "Quest voices".into(),
                needed: true,
                ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
                met_by: Vec::new(),
            },
            PackGroup {
                name: " ".into(),
                needed: false,
                ids: vec![],
                met_by: Vec::new(),
            },
        ];
        let problems = static_problems(&catalog(vec![spoken]));
        assert!(
            problems.iter().any(|p| p.contains(
                "Quest voices packs points at gh:rusty-key/spoken-wow/quests-audio, which isn't listed"
            )),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("with no name")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("name no addons")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_release_lines_id_is_plain_and_comes_from_its_repository() {
        for (id, repo, wanted) in [
            (
                "gh:rusty-key/spoken-wow/Quests-Audio",
                SPOKEN,
                "isn't lowercase",
            ),
            (
                "gh:rusty-key/spoken-wow/quests_audio",
                SPOKEN,
                "isn't lowercase",
            ),
            ("gh:rusty-key/spoken-wow/", SPOKEN, "isn't lowercase"),
            (
                "gh:rusty-key/spoken-wow/quests-audio",
                "rusty-key/spoken-wow/quests-audio",
                "its source isn't",
            ),
            (
                "gh:rusty-key/spoken-wow/quests-audio",
                "rusty-key/other",
                "its source isn't",
            ),
            ("gh:a/b/c/d", "a/b", "isn't gh:owner/repo"),
            ("gh:alone", "alone", "isn't gh:owner/repo"),
        ] {
            let problems = static_problems(&catalog(vec![entry(id, repo)]));
            assert!(
                problems.iter().any(|p| p.contains(wanted)),
                "{id}: {problems:?}"
            );
        }
        let mut listing = entry("wowi:7", SPOKEN);
        listing.source = Source::Wowi { id: 7 };
        listing.stable.as_mut().unwrap().url =
            "https://cdn.wowinterface.com/downloads/getfile.php?id=7".into();
        assert!(static_problems(&catalog(vec![listing])).is_empty());
    }

    #[test]
    fn former_main_folders_are_short_plain_names_and_never_the_main_one() {
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        spoken.formerly = vec!["SpokenOld".into(), "Spoken_Legacy".into()];
        assert!(static_problems(&catalog(vec![spoken.clone()])).is_empty());
        for (formerly, wanted) in [
            (vec!["../evil".to_string()], "can't be a folder name"),
            (vec![String::new()], "can't be a folder name"),
            (vec!["CON".into()], "can't be a folder name"),
            (vec!["Old".into(), "OLD".into()], "twice"),
            (vec!["spoken".into()], "its main folder"),
            (
                (0..=MAX_FORMERLY).map(|n| format!("Old{n}")).collect(),
                "more than 8",
            ),
        ] {
            spoken.formerly = formerly;
            let problems = static_problems(&catalog(vec![spoken.clone()]));
            assert!(
                problems.iter().any(|p| p.contains(wanted)),
                "{:?}: {problems:?}",
                spoken.formerly
            );
        }
    }

    #[test]
    fn met_by_prefixes_are_safe_folder_name_starts_and_few() {
        let quests = entry("gh:rusty-key/spoken-wow/quests-audio", SPOKEN);
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        let with = |met_by: Vec<String>| PackGroup {
            name: "Quest voices".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
            met_by,
        };
        spoken.packs = vec![with(vec!["SpokenQuestsAudio".into()])];
        let fine = static_problems(&catalog(vec![spoken.clone(), quests.clone()]));
        assert!(fine.is_empty(), "{fine:?}");
        for (met_by, wanted) in [
            (vec![String::new()], "can't start a folder name"),
            (vec!["Spoken/Quests".into()], "can't start a folder name"),
            (vec![" Spoken".into()], "can't start a folder name"),
            (
                (0..=MAX_MET_BY).map(|n| format!("Pack{n}")).collect(),
                "more than 8",
            ),
        ] {
            spoken.packs = vec![with(met_by)];
            let problems = static_problems(&catalog(vec![spoken.clone(), quests.clone()]));
            assert!(
                problems.iter().any(|p| p.contains(wanted)),
                "{:?}: {problems:?}",
                spoken.packs[0].met_by
            );
        }
    }

    #[test]
    fn a_pack_over_100_mb_is_never_reviewed() {
        let pack_of = |size: u64, reviewed: bool| {
            let mut quests = entry("gh:rusty-key/spoken-wow/quests-audio", SPOKEN);
            quests.folders = vec!["SpokenQuestsAudioAlliance".into()];
            quests.stable.as_mut().unwrap().size = size;
            quests.trusted = true;
            quests.reviewed = reviewed;
            quests
        };
        let mut spoken = entry("gh:rusty-key/spoken-wow", SPOKEN);
        spoken.reviewed = true;
        spoken.stable.as_mut().unwrap().size = 150_000_000;
        spoken.packs = vec![PackGroup {
            name: "Quest voices".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
            met_by: vec!["SpokenQuestsAudio".into()],
        }];
        let check = |pack: Addon| static_problems(&catalog(vec![spoken.clone(), pack]));
        let problems = check(pack_of(1_479_966_451, true));
        assert_eq!(
            problems,
            [
                "gh:rusty-key/spoken-wow/quests-audio is a pack of 1479966451 bytes and reviewed, so apps up to 0.1.2 would install it by themselves"
            ]
        );
        assert!(check(pack_of(1_479_966_451, false)).is_empty());
        assert!(
            check(pack_of(BIGGEST_UNASKED_DOWNLOAD, true)).is_empty(),
            "100 MB is fine"
        );
        let mut beta = pack_of(10, true);
        beta.beta = beta.stable.clone();
        beta.beta.as_mut().unwrap().size = 157_070_901;
        assert_eq!(check(beta).len(), 1, "a big beta counts too");
    }

    #[test]
    fn only_the_download_sites_the_readme_lists_pass() {
        for url in [
            "https://github.com/a/b/releases/download/v1/a.zip",
            "https://codeload.github.com/a/b/zip/refs/tags/v1",
            "https://cdn.wowinterface.com/downloads/getfile.php?id=1",
            "https://cdn-wow.mmoui.com/x.zip",
            "https://foreverdb.net/downloads/ForeverDB.zip",
        ] {
            assert!(is_listed_download_host(url), "{url}");
        }
        for url in [
            "https://example.com/a.zip",
            "https://github.com.evil.example/a.zip",
            "https://notgithub.com/a.zip",
            "https://foreverdb.net.example/a.zip",
        ] {
            assert!(!is_listed_download_host(url), "{url}");
        }
    }
}
