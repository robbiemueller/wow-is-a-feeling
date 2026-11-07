//! Replays of the real published data through the real merge, collision and gate code,
//! so a change to any of them can be checked against what's actually out there. They're
//! ignored by default because they need that data:
//!
//! ```text
//! git show origin/catalog:catalog.json > data/catalog.json
//! git show origin/catalog:catalog-state.json > data/catalog-state.json
//! CATALOG_REPLAY_DIR=data cargo test -p feeling-catalog replay -- --ignored --nocapture
//! ```
//!
//! Nothing here touches the network. The scan cache stands in for the scans.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use feeling_core::catalog::{Addon, Catalog, Release, Source};

use crate::build::{Published, Scanned, publish, settle};
use crate::curated::{self, Curated, HandPicked, Seeds};
use crate::gate::GateContext;
use crate::model::Candidate;
use crate::state::State;

const NOW: i64 = 1_794_100_000;
const DAY: i64 = 24 * 60 * 60;

struct Real {
    seeds: Seeds,
    curated: Curated,
    previous: Catalog,
    state: State,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load() -> Real {
    let dir = std::env::var("CATALOG_REPLAY_DIR").expect(
        "point CATALOG_REPLAY_DIR at a folder holding the two files from the catalog branch",
    );
    let dir = Path::new(&dir);
    let previous = std::fs::read(dir.join("catalog.json")).unwrap();
    let root = repo_root().join("catalog");
    let real = Real {
        seeds: curated::load_seeds(&root.join("seeds.toml")).unwrap(),
        curated: curated::load_curated(&root.join("curated.toml")).unwrap(),
        previous: serde_json::from_slice(&previous).unwrap(),
        state: State::load(&dir.join("catalog-state.json")),
    };
    assert!(!real.state.repos.is_empty(), "the state file didn't load");
    real
}

/// What the scans would hand over if every repository answered exactly what it did
/// the last time it was read.
fn scanned(state: &State) -> Scanned {
    let mut repos: Vec<Candidate> = state
        .repos
        .values()
        .filter_map(|record| record.entry.clone())
        .collect();
    let known: BTreeSet<String> = repos
        .iter()
        .map(|c| c.addon.id.to_ascii_lowercase())
        .collect();
    repos.extend(
        state
            .sources
            .values()
            .filter_map(|source| source.entry.clone())
            .filter(|c| !known.contains(&c.addon.id.to_ascii_lowercase())),
    );
    Scanned {
        repos,
        wowi: state
            .wowi
            .values()
            .filter_map(|listing| listing.entry.clone())
            .collect(),
        direct: state
            .direct
            .values()
            .filter_map(|direct| direct.entry.clone())
            .collect(),
    }
}

/// A repository as a stranger would set it up: a release zip and a TOC, nothing more.
fn repo(name: &str, title: &str, folders: &[&str], downloads: u64) -> Candidate {
    let addon = Addon {
        id: format!("gh:{name}"),
        name: title.into(),
        author: name.split('/').next().unwrap().into(),
        source: Source::Github { repo: name.into() },
        website: Some(format!("https://github.com/{name}")),
        stable: Some(Release {
            version: "1.0".into(),
            url: format!("https://github.com/{name}/releases/download/v1/a.zip"),
            size: 1000,
            date: NOW,
            sha256: Some("a".repeat(64)),
            ..Release::default()
        }),
        folders: folders.iter().map(|f| f.to_string()).collect(),
        downloads,
        ..Addon::default()
    };
    Candidate {
        addon,
        ..Candidate::default()
    }
}

fn with_wowi_id(mut candidate: Candidate, id: &str) -> Candidate {
    candidate.addon.ids.wowi = Some(id.into());
    candidate
}

struct Outcome {
    published: Published,
    picks: HandPicked,
}

impl Outcome {
    fn listed(&self, id: &str) -> Option<&Addon> {
        self.published.catalog.addons.iter().find(|a| a.id == id)
    }

    fn reason(&self, id: &str) -> Option<&str> {
        let row = self.published.held.iter().find(|h| h.id == id)?;
        Some(row.reason.as_str())
    }
}

fn run(
    real: &Real,
    extra: Vec<Candidate>,
    previous: &Catalog,
    edit: impl FnOnce(&mut Scanned),
) -> Result<Outcome, String> {
    let picks = HandPicked::new(&real.seeds, &real.curated);
    let mut input = scanned(&real.state);
    input.repos.extend(extra);
    edit(&mut input);
    let settled = settle(input, &real.curated, &picks, Some(previous), NOW);
    let dir = tempfile::tempdir().unwrap();
    let published = publish(
        settled.candidates,
        settled.held,
        &GateContext {
            curated: &real.curated,
            picks: &picks,
            previous: Some(previous),
            answered: &BTreeSet::new(),
            accept_smaller: false,
        },
        dir.path(),
    )?;
    Ok(Outcome { published, picks })
}

fn without_changes(_: &mut Scanned) {}

/// Everything that identifies an entry for a player, without the color that shifts when
/// the list grows by one.
fn fingerprint(addon: &Addon) -> String {
    format!(
        "{}|{:?}|{}|{:?}|{:?}",
        addon.name, addon.folders, addon.downloads, addon.ids, addon.stable
    )
}

#[test]
#[ignore = "needs the real catalog data, see the top of this file"]
fn replay_real_data_with_nobody_attacking() {
    let real = load();
    let outcome = run(&real, vec![], &real.previous, without_changes).unwrap();
    let new = &outcome.published.catalog;

    let before: BTreeMap<String, &Addon> = real
        .previous
        .addons
        .iter()
        .map(|a| (a.id.to_ascii_lowercase(), a))
        .collect();
    let after: BTreeMap<String, &Addon> = new
        .addons
        .iter()
        .map(|a| (a.id.to_ascii_lowercase(), a))
        .collect();
    let missing: Vec<&String> = before
        .keys()
        .filter(|id| !after.contains_key(*id))
        .collect();
    let added: Vec<&String> = after
        .keys()
        .filter(|id| !before.contains_key(*id))
        .collect();
    println!(
        "entries: {} before, {} after, {} missing, {} added",
        before.len(),
        after.len(),
        missing.len(),
        added.len()
    );
    println!("missing: {missing:?}");
    println!("added: {added:?}");

    let mut changes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (id, old) in &before {
        let Some(addon) = after.get(id) else { continue };
        let mut note = |what: &'static str, text: String| {
            changes
                .entry(what)
                .or_default()
                .push(format!("{id}: {text}"));
        };
        if old.name != addon.name {
            note("name", format!("{:?} -> {:?}", old.name, addon.name));
        }
        if old.folders != addon.folders {
            note(
                "folders",
                format!("{:?} -> {:?}", old.folders, addon.folders),
            );
        }
        if old.downloads != addon.downloads {
            note(
                "downloads",
                format!("{} -> {}", old.downloads, addon.downloads),
            );
        }
        if old.ids != addon.ids {
            note("ids", format!("{:?} -> {:?}", old.ids, addon.ids));
        }
        if old.tier != addon.tier {
            note("tier", format!("{} -> {}", old.tier, addon.tier));
        }
        if old.category != addon.category {
            note(
                "category",
                format!("{} -> {}", old.category, addon.category),
            );
        }
        if old.stable != addon.stable || old.beta != addon.beta {
            note("release", String::new());
        }
        if old.deps != addon.deps {
            note("deps", format!("{:?} -> {:?}", old.deps, addon.deps));
        }
    }
    for (what, list) in &changes {
        println!("changed {what}: {}", list.len());
        for line in list {
            println!("  {line}");
        }
    }

    println!("held: {}", outcome.published.held.len());
    for row in &outcome.published.held {
        println!(
            "  {}: {} {:?}: {}",
            row.id, row.name, row.folders, row.reason
        );
    }
    let trusted = new.addons.iter().filter(|a| a.trusted).count();
    let baseline = new
        .addons
        .iter()
        .filter(|a| a.created == crate::merge::BASELINE_CREATED)
        .count();
    let picked = new
        .addons
        .iter()
        .filter(|a| outcome.picks.contains(&a.id))
        .count();
    println!(
        "trusted {trusted}, baseline age {baseline}, hand-picked {picked}, of {}",
        new.addons.len()
    );
    println!(
        "gaps {}, featured {}, kits {}",
        new.gaps.len(),
        new.featured.len(),
        new.kits.len()
    );
    println!(
        "size: {} bytes raw, {} gzipped",
        outcome.published.raw, outcome.published.gz
    );

    assert!(missing.is_empty(), "{missing:?}");
    assert!(added.is_empty(), "{added:?}");
    assert_eq!(new.gaps.len(), real.curated.gaps.len(), "every gap stays");
    assert_eq!(new.featured.len(), real.previous.featured.len());
    assert!(
        new.addons.iter().all(|a| a.trusted),
        "all of today is established"
    );
    let wrong_tiers = changes.get("tier").map_or(0, Vec::len);
    assert!(wrong_tiers < 10, "tiers moved: {wrong_tiers}");
}

/// One attack: what goes in, and what must be true of what comes out.
fn check(
    real: &Real,
    baseline: &BTreeMap<String, String>,
    title: &str,
    extra: Vec<Candidate>,
    verdict: impl FnOnce(&Outcome) -> String,
) {
    let outcome = run(real, extra, &real.previous, without_changes).unwrap();
    let moved: Vec<&String> = baseline
        .iter()
        .filter(|(id, print)| {
            outcome
                .listed(id)
                .is_none_or(|addon| fingerprint(addon) != **print)
        })
        .map(|(id, _)| id)
        .collect();
    println!("{title}: {}", verdict(&outcome));
    println!("    real entries changed or gone: {}", moved.len());
    assert!(moved.is_empty(), "{title} changed real entries: {moved:?}");
}

fn held_because(outcome: &Outcome, id: &str) -> String {
    outcome
        .reason(id)
        .map(|reason| format!("{id} held, {reason}"))
        .unwrap_or_else(|| panic!("{id} should have been held"))
}

fn harmless(outcome: &Outcome, id: &str) -> String {
    let addon = outcome
        .listed(id)
        .unwrap_or_else(|| panic!("{id} should be listed under its own name"));
    assert!(!addon.trusted, "{id} must not be picked by the app");
    assert!(addon.ids.is_empty(), "{id} keeps no ids");
    assert!(addon.downloads < 100_000, "{id} borrowed counts");
    format!(
        "{id} listed as itself, not trusted, no ids, {} downloads",
        addon.downloads
    )
}

#[test]
#[ignore = "needs the real catalog data, see the top of this file"]
fn replay_real_data_with_each_attack() {
    let real = load();
    let clean = run(&real, vec![], &real.previous, without_changes).unwrap();
    let baseline: BTreeMap<String, String> = clean
        .published
        .catalog
        .addons
        .iter()
        .map(|a| (a.id.clone(), fingerprint(a)))
        .collect();

    check(
        &real,
        &baseline,
        "zebra/TomTom claiming the listing's id",
        vec![with_wowi_id(
            repo("zebra/TomTom", "TomTom", &["TomTom"], 1),
            "7032",
        )],
        |o| held_because(o, "gh:zebra/TomTom"),
    );
    check(
        &real,
        &baseline,
        "zebra/TomTom with the same folder and no id",
        vec![repo("zebra/TomTom", "TomTom", &["TomTom"], 4_000_000)],
        |o| held_because(o, "gh:zebra/TomTom"),
    );
    check(
        &real,
        &baseline,
        "zebra/CoolBags claiming TomTom's id under its own name",
        vec![with_wowi_id(
            repo("zebra/CoolBags", "Cool Bags", &["CoolBags"], 5),
            "7032",
        )],
        |o| harmless(o, "gh:zebra/CoolBags"),
    );
    check(
        &real,
        &baseline,
        "0-free-account claiming BigWigs's id, sorting before the real repo",
        vec![with_wowi_id(
            repo("0-free-account/Bw", "Bw Helper", &["BwHelper"], 1),
            "5086",
        )],
        |o| harmless(o, "gh:0-free-account/Bw"),
    );
    check(
        &real,
        &baseline,
        "TitanFixes claiming Titan Panel's id",
        vec![with_wowi_id(
            repo("zebra/TitanFixes", "Titan Fixes", &["TitanFixes"], 1),
            "8092",
        )],
        |o| harmless(o, "gh:zebra/TitanFixes"),
    );
    check(
        &real,
        &baseline,
        "A Questie look-alike with a Cyrillic e",
        vec![repo(
            "zebra/Questie-A",
            "Qu\u{0435}stie",
            &["QuestieX"],
            4_000_000,
        )],
        |o| held_because(o, "gh:zebra/Questie-A"),
    );
    check(
        &real,
        &baseline,
        "A Questie look-alike with the plain name and its own folder",
        vec![repo("zebra/Questie-B", "Questie", &["QuestieX"], 1)],
        |o| held_because(o, "gh:zebra/Questie-B"),
    );
    check(
        &real,
        &baseline,
        "A title of Deadly Boss Mods, the name a hand edit gave the real one",
        vec![repo("zebra/Dbm", "Deadly Boss Mods", &["DbmX"], 1)],
        |o| held_because(o, "gh:zebra/Dbm"),
    );
    check(
        &real,
        &baseline,
        "A gap claim for ElvUI",
        vec![repo("free-account/ElvUI", "ElvUI", &["ElvUI"], 150_000)],
        |o| {
            assert!(o.published.catalog.gaps.iter().any(|g| g.name == "ElvUI"));
            held_because(o, "gh:free-account/ElvUI")
        },
    );
    check(
        &real,
        &baseline,
        "A gap claim for Leatrix Plus",
        vec![repo(
            "free-account/LP",
            "Leatrix Plus",
            &["Leatrix_Plus"],
            1,
        )],
        |o| {
            assert!(
                o.published
                    .catalog
                    .gaps
                    .iter()
                    .any(|g| g.name == "Leatrix Plus")
            );
            held_because(o, "gh:free-account/LP")
        },
    );
    check(
        &real,
        &baseline,
        "Another name shipping Leatrix_Plus as a side folder",
        vec![repo(
            "free-account/Tweaks",
            "Better Tweaks",
            &["Tweaks", "Leatrix_Plus"],
            1,
        )],
        |o| held_because(o, "gh:free-account/Tweaks"),
    );
    check(
        &real,
        &baseline,
        "A seeded repo's folder with no entry yet (OmniCC)",
        vec![repo(
            "free-account/OmniCC",
            "Cooldown Numbers",
            &["OmniCC"],
            1,
        )],
        |o| held_because(o, "gh:free-account/OmniCC"),
    );
    check(
        &real,
        &baseline,
        "A repo evicting Prat, under its name",
        vec![repo(
            "free-account/Prat",
            "Prat 3.0",
            &["Prat-3.0"],
            700_000,
        )],
        |o| held_because(o, "gh:free-account/Prat"),
    );
    check(
        &real,
        &baseline,
        "A repo evicting Prat, under another name",
        vec![repo(
            "free-account/Other",
            "Totally Unrelated",
            &["Prat-3.0"],
            700_000,
        )],
        |o| held_because(o, "gh:free-account/Other"),
    );
    check(
        &real,
        &baseline,
        "A repo evicting WIM",
        vec![repo("free-account/WIM", "WIM", &["WIM"], 1)],
        |o| held_because(o, "gh:free-account/WIM"),
    );
    check(
        &real,
        &baseline,
        "A one-download Details claim",
        vec![repo("free-account/Details", "Details", &["Details"], 1)],
        |o| held_because(o, "gh:free-account/Details"),
    );
    check(
        &real,
        &baseline,
        "A copy of a repo that isn't seeded, with more downloads (PeaversCommons)",
        vec![repo(
            "zebra/PeaversCommons",
            "PeaversCommons",
            &["PeaversCommons"],
            900_000,
        )],
        |o| held_because(o, "gh:zebra/PeaversCommons"),
    );
    check(
        &real,
        &baseline,
        "A copy of oUF, which a listing's id used to go to",
        vec![repo("Aardvark/oUF", "oUF", &["oUF"], 900_000)],
        |o| held_because(o, "gh:Aardvark/oUF"),
    );
    check(
        &real,
        &baseline,
        "Installing over another addon by shipping its folder (CoolPins and Questie)",
        vec![repo(
            "zebra/CoolPins",
            "Cool Pins",
            &["CoolPins", "Questie"],
            1,
        )],
        |o| held_because(o, "gh:zebra/CoolPins"),
    );

    // Two more that need the last catalog to say something different.
    let sleeper_id = "gh:paradosi/ForeverQuestItemBar";
    let target_id = "gh:peavers-warcraft/PeaversCommons";
    let picks = HandPicked::new(&real.seeds, &real.curated);
    assert!(!picks.contains(sleeper_id) && !picks.contains(target_id));
    assert!(clean.listed(sleeper_id).is_some() && clean.listed(target_id).is_some());
    let mut aged = real.previous.clone();
    for addon in &mut aged.addons {
        addon.created = match addon.id.as_str() {
            id if id == sleeper_id => NOW - 120 * DAY,
            id if id == target_id => NOW - 30 * DAY,
            _ => 0,
        };
    }
    let retitled = run(&real, vec![], &aged, |input| {
        let entry = input
            .repos
            .iter_mut()
            .find(|c| c.addon.id == sleeper_id)
            .unwrap();
        entry.addon.name = "PeaversCommons".into();
        entry.addon.folders = vec!["PeaversCommons".into()];
    })
    .unwrap();
    println!(
        "A sleeper older than the real addon, retitled to it: {}",
        held_because(&retitled, sleeper_id)
    );
    assert!(retitled.listed(target_id).is_some());

    let sideways = run(&real, vec![], &aged, |input| {
        let entry = input
            .repos
            .iter_mut()
            .find(|c| c.addon.id == sleeper_id)
            .unwrap();
        entry.addon.folders.push("PeaversCommons".into());
    })
    .unwrap();
    println!(
        "A sleeper older than the real addon, adding its folder on the side: {}",
        held_because(&sideways, sleeper_id)
    );
    assert!(sideways.listed(target_id).is_some());

    let fresh_title = run(&real, vec![], &aged, |input| {
        let entry = input
            .repos
            .iter_mut()
            .find(|c| c.addon.id == sleeper_id)
            .unwrap();
        entry.addon.name = "Something Entirely New".into();
    })
    .unwrap();
    let addon = fresh_title.listed(sleeper_id).unwrap();
    assert_eq!(addon.created, NOW);
    assert!(!addon.trusted);
    println!("A sleeper retitled to a name nobody has: listed, age reset, not trusted");

    // A flood: many repositories, each with its own name.
    let flood: Vec<Candidate> = (0..60)
        .map(|at| {
            repo(
                &format!("flood/R{at}"),
                &format!("Junk {at}"),
                &[&format!("Junk{at}")],
                1,
            )
        })
        .collect();
    let flooded = run(&real, flood, &real.previous, without_changes).unwrap();
    let listed = flooded
        .published
        .catalog
        .addons
        .iter()
        .filter(|a| a.id.starts_with("gh:flood/"))
        .count();
    let capped = flooded
        .published
        .held
        .iter()
        .filter(|h| h.id.starts_with("gh:flood/"))
        .count();
    println!("One owner with 60 new repositories: {listed} listed, none trusted, {capped} held");
    assert_eq!((listed, capped), (40, 20));

    // The freeze: a catalog doubled by junk, which then gets deleted.
    let mut doubled = real.previous.clone();
    let junk: Vec<Addon> = (0..real.previous.addons.len() + 1)
        .map(|at| {
            repo(
                &format!("account{}/R{at}", at / 25),
                &format!("Junk {at}"),
                &[&format!("Junk{at}")],
                1,
            )
            .addon
        })
        .collect();
    let answered: BTreeSet<String> = junk.iter().map(|a| a.id.to_ascii_lowercase()).collect();
    doubled.addons.extend(junk);
    let attempt = |answered: &BTreeSet<String>| {
        let picks = HandPicked::new(&real.seeds, &real.curated);
        let settled = settle(
            scanned(&real.state),
            &real.curated,
            &picks,
            Some(&doubled),
            NOW,
        );
        let dir = tempfile::tempdir().unwrap();
        publish(
            settled.candidates,
            settled.held,
            &GateContext {
                curated: &real.curated,
                picks: &picks,
                previous: Some(&doubled),
                answered,
                accept_smaller: false,
            },
            dir.path(),
        )
        .map(|published| published.catalog.addons.len())
    };
    let flooded_then_deleted = attempt(&answered).unwrap();
    println!(
        "A catalog doubled by junk that's then deleted, with GitHub saying so: published {flooded_then_deleted} addons"
    );
    let unanswered = attempt(&BTreeSet::new()).unwrap_err();
    println!("The same loss with no source saying why: refused, {unanswered}");

    // Losing the scan cache: a few seeds answer and everything else was never asked about.
    let lost = run(&real, vec![], &real.previous, |input| {
        input.repos.truncate(300);
        input.wowi.clear();
    });
    match lost {
        Ok(_) => panic!("a lost scan cache must not publish"),
        Err(problem) => {
            println!("Losing the scan cache, with 300 repositories left: refused, {problem}")
        }
    }
}
