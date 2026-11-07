//! The last check before anything is published. One bad addon is dropped with a
//! warning, because a single odd repository shouldn't stop the whole catalog from
//! updating. A catalog that looks broken as a whole is refused outright.

use std::collections::{BTreeMap, BTreeSet};

use feeling_core::catalog::{Addon, CATALOG_SCHEMA, CATEGORIES, Catalog, PackGroup, Release};

use crate::curated::{Curated, HandPicked, PackSpec};
use crate::model::{Held, MAX_DEPS};
use crate::text::fold_name;
use crate::zipscan::MAX_FOLDERS;

/// The most the published file may hold: about ten times today's, and room for some
/// twelve thousand ordinary entries. Past it, a catalog takes real memory on every player's
/// machine just to sit there.
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

const DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A folder name that's safe to create under `Interface/AddOns` on every OS.
pub fn is_safe_folder(name: &str) -> bool {
    let bad_char = |c: char| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    };
    !name.is_empty()
        && name.len() <= 120
        && !name.starts_with(['.', ' '])
        && !name.ends_with(['.', ' '])
        && !name.chars().any(bad_char)
        && !DEVICE_NAMES.contains(
            &name
                .split('.')
                .next()
                .unwrap_or(name)
                .to_ascii_lowercase()
                .as_str(),
        )
}

fn is_https(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        !rest.is_empty() && !rest.starts_with('/') && !url.contains(char::is_whitespace)
    })
}

fn release_problem(release: &Release, folders: &[String]) -> Option<String> {
    if release.version.trim().is_empty() {
        return Some("a release has no version".to_string());
    }
    if !is_https(&release.url) {
        return Some(format!("release url isn't https: {}", release.url));
    }
    for moved in &release.moves {
        if moved.from.split('/').any(|part| part == "..") {
            return Some(format!("a move escapes the archive: {}", moved.from));
        }
        if !folders.contains(&moved.to) {
            return Some(format!("a move targets an unlisted folder: {}", moved.to));
        }
    }
    None
}

/// Why this addon can't be published, or `None` when it's fine.
pub fn addon_problem(addon: &Addon) -> Option<String> {
    if addon.id.is_empty() || !addon.id.contains(':') {
        return Some("bad id".to_string());
    }
    if addon.name.trim().is_empty() {
        return Some("no name".to_string());
    }
    if !CATEGORIES.iter().any(|(id, _)| *id == addon.category) {
        return Some(format!("unknown category {}", addon.category));
    }
    if addon.stable.is_none() && addon.beta.is_none() {
        return Some("no usable release".to_string());
    }
    if addon.folders.is_empty() {
        return Some("no folders".to_string());
    }
    if addon.folders.len() > MAX_FOLDERS {
        return Some(format!("too many folders ({})", addon.folders.len()));
    }
    if addon.deps.len() > MAX_DEPS {
        return Some(format!("too many dependencies ({})", addon.deps.len()));
    }
    if let Some(bad) = addon.folders.iter().find(|f| !is_safe_folder(f)) {
        return Some(format!("unsafe folder name {bad:?}"));
    }
    let lower: BTreeSet<String> = addon
        .folders
        .iter()
        .map(|f| f.to_ascii_lowercase())
        .collect();
    if lower.len() != addon.folders.len() {
        return Some("folder names that differ only by case".to_string());
    }
    addon
        .stable
        .iter()
        .chain(addon.beta.iter())
        .find_map(|release| release_problem(release, &addon.folders))
}

/// What the gate goes by besides the catalog itself.
pub struct GateContext<'a> {
    pub curated: &'a Curated,
    pub picks: &'a HandPicked,
    /// The last published catalog.
    pub previous: Option<&'a Catalog>,
    /// Lowercased ids a source gave a clear answer about this run, whatever the answer was.
    pub answered: &'a BTreeSet<String>,
    /// A maintainer looked and says a smaller catalog is right.
    pub accept_smaller: bool,
}

/// A catalog that's fit to publish, and what it took to get there.
#[derive(Debug)]
pub struct Passed {
    pub catalog: Catalog,
    pub warnings: Vec<String>,
    /// Entries the gate itself kept out, for `catalog-held.json`.
    pub held: Vec<Held>,
}

/// Over the size limit. The youngest and then the largest go first, never anything
/// hand-picked: a size limit that stops the whole build would let a stranger stop it too.
/// Only hand-picked entries being too big is a refusal, and that needs a person anyway.
fn trim_to_size(
    catalog: &mut Catalog,
    picks: &HandPicked,
    limit: usize,
    warnings: &mut Vec<String>,
) -> Result<Vec<Held>, String> {
    let size_of = |addon: &Addon| serde_json::to_vec(addon).map_or(0, |bytes| bytes.len());
    let sizes: Vec<usize> = catalog.addons.iter().map(size_of).collect();
    let mut total: usize = sizes.iter().sum();
    if total <= limit {
        return Ok(Vec::new());
    }
    let largest = sizes
        .iter()
        .enumerate()
        .max_by_key(|(_, size)| **size)
        .map_or(String::new(), |(at, size)| {
            format!("{} at {size} bytes", catalog.addons[at].id)
        });
    warnings.push(format!(
        "the catalog came to {total} bytes, over the limit of {limit}. The largest entry is {largest}."
    ));

    let mut order: Vec<usize> = (0..catalog.addons.len())
        .filter(|at| !picks.contains(&catalog.addons[*at].id))
        .collect();
    order.sort_by_key(|at| {
        (
            std::cmp::Reverse(catalog.addons[*at].created),
            std::cmp::Reverse(sizes[*at]),
        )
    });
    let mut dropped = BTreeSet::new();
    for at in order {
        if total <= limit {
            break;
        }
        total -= sizes[at];
        dropped.insert(at);
    }
    if total > limit {
        return Err(format!(
            "the catalog is {total} bytes with only hand-picked entries left, over the limit of {limit}. The largest entry is {largest}. Not publishing."
        ));
    }

    let mut held = Vec::with_capacity(dropped.len());
    let mut at = 0;
    catalog.addons.retain(|addon| {
        let drop = dropped.contains(&at);
        at += 1;
        if drop {
            held.push(Held::new(
                addon,
                "the catalog went over its size limit, and newer and larger entries go first"
                    .to_string(),
            ));
        }
        !drop
    });
    Ok(held)
}

/// Whether this catalog lost so much, with no explanation, that something must be wrong.
/// Only entries nobody answered for count: a repository that's gone, or that a maintainer
/// excluded, explains itself, and counting those would let a flood of junk that gets
/// deleted freeze the catalog for everyone. Hand-picked entries count however they went,
/// because a stranger can't make them vanish, and an outage or a bad answer from a source would.
fn shrink_problem(catalog: &Catalog, context: &GateContext<'_>) -> Option<String> {
    let previous = context.previous?;
    if context.accept_smaller {
        return None;
    }
    let listed: BTreeSet<String> = catalog
        .addons
        .iter()
        .map(|a| a.id.to_ascii_lowercase())
        .collect();
    let missing = || {
        previous
            .addons
            .iter()
            .filter(|a| !listed.contains(&a.id.to_ascii_lowercase()))
    };
    let unexplained = missing()
        .filter(|a| {
            !context.answered.contains(&a.id.to_ascii_lowercase())
                && !context.curated.excludes_id(&a.id)
        })
        .count();
    if unexplained * 2 > previous.addons.len() {
        return Some(format!(
            "{unexplained} of the last catalog's {} addons are missing and no source said why, so something is wrong. Not publishing.",
            previous.addons.len()
        ));
    }
    let picked = |a: &&Addon| context.picks.contains(&a.id);
    let picked_before = previous.addons.iter().filter(picked).count();
    let picked_missing = missing().filter(picked).count();
    if picked_missing * 2 > picked_before {
        return Some(format!(
            "{picked_missing} of the last catalog's {picked_before} hand-picked addons are missing, so something is wrong. Not publishing."
        ));
    }
    None
}

/// Puts each `[[packs]]` group on the addon it's for, with the ids as the catalog spells them,
/// and drops ids the catalog doesn't list and groups left with none. These groups are the only
/// ones an addon has: nothing an addon's own files say becomes one.
fn attach_packs(catalog: &mut Catalog, specs: &[PackSpec], warnings: &mut Vec<String>) {
    let listed: BTreeMap<String, String> = catalog
        .addons
        .iter()
        .map(|addon| (addon.id.to_ascii_lowercase(), addon.id.clone()))
        .collect();
    let mut groups: BTreeMap<String, Vec<PackGroup>> = BTreeMap::new();
    for spec in specs {
        let name = spec.name.trim();
        let Some(addon) = listed.get(&spec.addon.to_ascii_lowercase()) else {
            warnings.push(format!(
                "packs for {}: it isn't in the catalog, dropped its {name} group",
                spec.addon
            ));
            continue;
        };
        let mut ids = Vec::with_capacity(spec.ids.len());
        for id in &spec.ids {
            match listed.get(&id.to_ascii_lowercase()) {
                Some(listed) => ids.push(listed.clone()),
                None => warnings.push(format!(
                    "packs {name} for {addon}: {id} isn't in the catalog, dropped it"
                )),
            }
        }
        if ids.is_empty() {
            warnings.push(format!(
                "packs {name} for {addon}: nothing left, dropped the group"
            ));
            continue;
        }
        groups
            .entry(addon.to_ascii_lowercase())
            .or_default()
            .push(PackGroup {
                name: name.to_string(),
                needed: spec.needed,
                ids,
                met_by: Vec::new(),
            });
    }
    for addon in &mut catalog.addons {
        addon.packs = groups
            .remove(&addon.id.to_ascii_lowercase())
            .unwrap_or_default();
    }
}

/// Check everything and fix what can be fixed. `Err` means don't publish.
pub fn run_gate(mut catalog: Catalog, context: &GateContext<'_>) -> Result<Passed, String> {
    if catalog.schema != CATALOG_SCHEMA {
        return Err(format!(
            "the catalog has schema {} instead of {CATALOG_SCHEMA}",
            catalog.schema
        ));
    }

    let mut warnings = Vec::new();
    let mut seen = BTreeSet::new();
    catalog.addons.retain_mut(|addon| {
        if !seen.insert(addon.id.to_ascii_lowercase()) {
            warnings.push(format!("{}: duplicate id, keeping the first", addon.id));
            return false;
        }
        if let Some(problem) = addon_problem(addon) {
            warnings.push(format!("{}: dropped, {problem}", addon.id));
            return false;
        }
        if addon.website.as_deref().is_some_and(|w| !is_https(w)) {
            warnings.push(format!("{}: website isn't https, removed it", addon.id));
            addon.website = None;
        }
        true
    });

    let held = trim_to_size(
        &mut catalog,
        context.picks,
        MAX_CATALOG_BYTES,
        &mut warnings,
    )?;
    if let Some(problem) = shrink_problem(&catalog, context) {
        return Err(problem);
    }
    if catalog.addons.is_empty() {
        return Err("the new catalog has no addons at all. Not publishing.".to_string());
    }

    let ids: BTreeSet<&str> = catalog.addons.iter().map(|a| a.id.as_str()).collect();
    let known = |id: &String| ids.contains(id.as_str());

    let before = catalog.featured.clone();
    catalog.featured.retain(&known);
    for id in before.iter().filter(|id| !known(id)) {
        warnings.push(format!("featured: {id} isn't in the catalog, dropped it"));
    }

    let mut unknown_in_kits = Vec::new();
    for kit in &mut catalog.kits {
        for id in kit.addons.iter().filter(|id| !known(id)) {
            unknown_in_kits.push(format!(
                "kit {}: {id} isn't in the catalog, dropped it",
                kit.name
            ));
        }
        kit.addons.retain(&known);
    }
    warnings.append(&mut unknown_in_kits);
    catalog.kits.retain(|kit| {
        let usable = kit.addons.len() >= 2;
        if !usable {
            warnings.push(format!("kit {}: too few addons left, dropped it", kit.name));
        }
        usable
    });

    let mut kept_gaps = Vec::with_capacity(catalog.gaps.len());
    for mut gap in std::mem::take(&mut catalog.gaps) {
        let unrelated = context
            .curated
            .gaps
            .iter()
            .find(|g| g.name == gap.name)
            .map_or(&[][..], |g| g.unrelated.as_slice());
        let wanted = fold_name(&gap.name);
        // A name alone is anyone's to write, so only an entry someone picked closes a gap.
        let present = catalog.addons.iter().find(|a| {
            fold_name(&a.name) == wanted
                && !unrelated.contains(&a.id)
                && context.picks.contains(&a.id)
        });
        if let Some(addon) = present {
            warnings.push(format!(
                "gap {}: {} is in the catalog now, dropped the gap",
                gap.name, addon.id
            ));
            continue;
        }
        for id in gap.alternatives.iter().filter(|id| !known(id)) {
            warnings.push(format!(
                "gap {}: alternative {id} isn't in the catalog, dropped it",
                gap.name
            ));
        }
        gap.alternatives.retain(&known);
        kept_gaps.push(gap);
    }
    catalog.gaps = kept_gaps;
    attach_packs(&mut catalog, &context.curated.packs, &mut warnings);
    Ok(Passed {
        catalog,
        warnings,
        held,
    })
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::{Gap, Kit, Move, Release, Source};

    use super::*;
    use crate::curated::{GapSpec, Seeds};

    fn addon(id: &str, name: &str) -> Addon {
        Addon {
            id: id.into(),
            name: name.into(),
            category: "misc".into(),
            source: Source::Github { repo: "a/b".into() },
            stable: Some(Release {
                version: "1.0".into(),
                url: "https://github.com/a/b/releases/download/v1/b.zip".into(),
                ..Release::default()
            }),
            folders: vec![name.replace(' ', "")],
            ..Addon::default()
        }
    }

    fn catalog(addons: Vec<Addon>) -> Catalog {
        Catalog {
            schema: CATALOG_SCHEMA,
            generated: 1,
            addons,
            ..Catalog::default()
        }
    }

    /// The gate with nothing special going on.
    fn gate(c: Catalog) -> Result<Passed, String> {
        gate_with(c, &Curated::default(), &HandPicked::default(), None, &[])
    }

    fn gate_with(
        c: Catalog,
        curated: &Curated,
        picks: &HandPicked,
        previous: Option<&Catalog>,
        answered: &[&str],
    ) -> Result<Passed, String> {
        let answered: BTreeSet<String> = answered.iter().map(|id| id.to_lowercase()).collect();
        run_gate(
            c,
            &GateContext {
                curated,
                picks,
                previous,
                answered: &answered,
                accept_smaller: false,
            },
        )
    }

    fn pass(c: Catalog) -> (Catalog, Vec<String>) {
        let passed = gate(c).unwrap();
        (passed.catalog, passed.warnings)
    }

    fn picked(repos: &[&str]) -> HandPicked {
        let seeds = Seeds {
            repos: repos.iter().map(|r| (*r).to_string()).collect(),
            wowi: vec![],
        };
        HandPicked::new(&seeds, &Curated::default())
    }

    fn numbered(count: usize) -> Catalog {
        let addons = (0..count)
            .map(|i| addon(&format!("gh:a/{i}"), &format!("N{i}")))
            .collect();
        catalog(addons)
    }

    #[test]
    fn safe_folder_names() {
        for ok in [
            "Questie",
            "!BugGrabber",
            "DBM-Core",
            "Prat-3.0",
            "LibStub.Extra",
            "Titan Panel",
            "ElvUI_Options",
        ] {
            assert!(is_safe_folder(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "trailing.",
            " spaced",
            "spaced ",
            "a/b",
            "a\\b",
            "c:",
            "C:evil",
            "CON",
            "nul.txt",
            "Lpt1",
            "a\nb",
            "wild*",
            "que?",
            "pipe|",
            "<x>",
        ] {
            assert!(!is_safe_folder(bad), "{bad:?}");
        }
        assert!(!is_safe_folder(&"x".repeat(121)));
    }

    #[test]
    fn https_check_is_strict() {
        assert!(is_https("https://github.com/a/b.zip"));
        assert!(!is_https("http://github.com/a/b.zip"));
        assert!(!is_https("https://"));
        assert!(!is_https("https:///path"));
        assert!(!is_https("ftp://x"));
        assert!(!is_https("https://x y/z"));
        assert!(!is_https(""));
    }

    #[test]
    fn a_healthy_catalog_passes_untouched() {
        let (out, warnings) = pass(catalog(vec![
            addon("gh:a/b", "Foo"),
            addon("wowi:1", "Bar"),
        ]));
        assert_eq!(out.addons.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn duplicate_ids_keep_only_the_first() {
        let mut second = addon("GH:A/B", "Second");
        second.folders = vec!["Second".into()];
        let (out, warnings) = pass(catalog(vec![addon("gh:a/b", "First"), second]));
        assert_eq!(out.addons.len(), 1);
        assert_eq!(out.addons[0].name, "First");
        assert!(warnings[0].contains("duplicate"), "{warnings:?}");
    }

    #[test]
    fn each_kind_of_broken_addon_is_dropped_with_a_reason() {
        type Breaker = Box<dyn Fn(&mut Addon)>;
        let cases: Vec<(&str, Breaker)> = vec![
            ("bad id", Box::new(|a| a.id = "nocolon".into())),
            ("no name", Box::new(|a| a.name = " ".into())),
            (
                "unknown category",
                Box::new(|a| a.category = "cooking".into()),
            ),
            ("no usable release", Box::new(|a| a.stable = None)),
            ("no folders", Box::new(|a| a.folders.clear())),
            (
                "unsafe folder",
                Box::new(|a| a.folders = vec!["../evil".into()]),
            ),
            (
                "unsafe folder",
                Box::new(|a| a.folders = vec!["dots.".into()]),
            ),
            (
                "differ only by case",
                Box::new(|a| a.folders = vec!["Foo".into(), "foo".into()]),
            ),
            (
                "isn't https",
                Box::new(|a| a.stable.as_mut().unwrap().url = "http://x.test/a.zip".into()),
            ),
            (
                "no version",
                Box::new(|a| a.stable.as_mut().unwrap().version = " ".into()),
            ),
            (
                "escapes",
                Box::new(|a| {
                    a.stable.as_mut().unwrap().moves = vec![Move {
                        from: "*/../x".into(),
                        to: "Foo".into(),
                    }];
                }),
            ),
            (
                "unlisted",
                Box::new(|a| {
                    a.stable.as_mut().unwrap().moves = vec![Move {
                        from: "*/x".into(),
                        to: "Elsewhere".into(),
                    }];
                }),
            ),
            (
                "too many folders",
                Box::new(|a| {
                    a.folders = (0..=MAX_FOLDERS).map(|i| format!("Pack{i}")).collect();
                }),
            ),
            (
                "too many dependencies",
                Box::new(|a| {
                    a.deps = (0..=MAX_DEPS).map(|i| format!("Lib{i}")).collect();
                }),
            ),
        ];
        for (want, breaker) in cases {
            let mut bad = addon("gh:a/bad", "Foo");
            breaker(&mut bad);
            let (out, warnings) = pass(catalog(vec![addon("gh:a/good", "Good"), bad]));
            assert_eq!(out.addons.len(), 1, "{want}");
            assert!(
                warnings.iter().any(|w| w.contains(want)),
                "{want}: {warnings:?}"
            );
        }
    }

    #[test]
    fn a_broken_beta_counts_too() {
        let mut bad = addon("gh:a/b", "Foo");
        bad.beta = Some(Release {
            version: "2".into(),
            url: "http://x.test/a.zip".into(),
            ..Release::default()
        });
        assert!(addon_problem(&bad).is_some());
    }

    #[test]
    fn a_beta_only_addon_is_fine() {
        let mut beta_only = addon("gh:a/b", "Foo");
        beta_only.beta = beta_only.stable.take();
        assert_eq!(addon_problem(&beta_only), None);
    }

    #[test]
    fn an_http_website_is_removed_but_the_addon_stays() {
        let mut a = addon("gh:a/b", "Foo");
        a.website = Some("http://example.test".into());
        let (out, warnings) = pass(catalog(vec![a]));
        assert_eq!(out.addons[0].website, None);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_wrong_schema_is_refused() {
        let mut c = catalog(vec![addon("gh:a/b", "Foo")]);
        c.schema = 0;
        assert!(gate(c).is_err());
    }

    fn shrink(new: Catalog, previous: &Catalog, answered: &[&str]) -> Result<Passed, String> {
        gate_with(
            new,
            &Curated::default(),
            &HandPicked::default(),
            Some(previous),
            answered,
        )
    }

    #[test]
    fn losing_more_than_half_with_no_explanation_is_refused() {
        let new = || numbered(3);
        let six = numbered(6);
        let seven = numbered(7);
        assert!(shrink(new(), &six, &[]).is_ok(), "exactly half is allowed");
        let error = shrink(new(), &seven, &[]).unwrap_err();
        assert!(error.contains("Not publishing"), "{error}");
        assert!(error.contains("4 of the last catalog's 7"), "{error}");
        assert!(shrink(new(), &numbered(0), &[]).is_ok());
        assert!(gate(new()).is_ok());
    }

    #[test]
    fn repositories_that_answered_dont_freeze_the_catalog() {
        // A flood that got listed, then deleted: GitHub said so for every one of them.
        let mut flooded = numbered(3);
        let junk: Vec<String> = (0..100).map(|i| format!("gh:flood/{i}")).collect();
        flooded
            .addons
            .extend(junk.iter().map(|id| addon(id, "Junk")));
        let answered: Vec<&str> = junk.iter().map(String::as_str).collect();
        assert!(shrink(numbered(3), &flooded, &answered).is_ok());
        assert!(
            shrink(numbered(3), &flooded, &[]).is_err(),
            "the same loss with nobody saying why"
        );
    }

    #[test]
    fn new_entries_cant_hide_a_real_loss() {
        // Everything real gone in a bad run, and a flood of new names to make up the count.
        let previous = numbered(10);
        let mut new = catalog(
            (0..50)
                .map(|i| addon(&format!("gh:flood/{i}"), &format!("Junk{i}")))
                .collect(),
        );
        new.addons.push(addon("gh:a/0", "N0"));
        assert!(shrink(new, &previous, &[]).is_err());
    }

    #[test]
    fn excluding_a_flooding_owner_explains_the_loss_by_itself() {
        let mut flooded = numbered(3);
        flooded
            .addons
            .extend((0..100).map(|i| addon(&format!("gh:flood/{i}"), "Junk")));
        let curated = Curated {
            exclude: crate::curated::Exclude {
                owners: vec!["flood".into()],
                ..Default::default()
            },
            ..Curated::default()
        };
        let result = gate_with(
            numbered(3),
            &curated,
            &HandPicked::default(),
            Some(&flooded),
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn a_source_saying_everything_is_gone_is_stopped_by_the_hand_picked_entries() {
        let repos: Vec<String> = (0..6).map(|i| format!("a/{i}")).collect();
        let seeds: Vec<&str> = repos.iter().map(String::as_str).collect();
        let picks = picked(&seeds);
        let ids: Vec<String> = repos.iter().map(|r| format!("gh:{r}")).collect();
        let answered: Vec<&str> = ids.iter().map(String::as_str).collect();
        let result = gate_with(
            numbered(2),
            &Curated::default(),
            &picks,
            Some(&numbered(6)),
            &answered,
        );
        let error = result.unwrap_err();
        assert!(error.contains("hand-picked"), "{error}");
        assert!(
            gate_with(
                numbered(3),
                &Curated::default(),
                &picks,
                Some(&numbered(6)),
                &answered
            )
            .is_ok(),
            "half of them is still allowed"
        );
    }

    #[test]
    fn a_maintainer_can_accept_a_smaller_catalog() {
        let picks = HandPicked::default();
        let answered = BTreeSet::new();
        let previous = numbered(100);
        let result = run_gate(
            numbered(1),
            &GateContext {
                curated: &Curated::default(),
                picks: &picks,
                previous: Some(&previous),
                answered: &answered,
                accept_smaller: true,
            },
        );
        assert!(result.is_ok());
    }

    #[test]
    fn an_empty_catalog_is_refused_even_without_a_previous_one() {
        assert!(gate(catalog(vec![])).is_err());
        let all_bad = catalog(vec![Addon {
            id: "x".into(),
            ..Addon::default()
        }]);
        assert!(gate(all_bad).is_err());
    }

    fn sized(id: &str, created: i64, folders: usize) -> Addon {
        Addon {
            created,
            folders: (0..folders).map(|i| format!("Pack{i:0100}")).collect(),
            ..addon(id, id)
        }
    }

    #[test]
    fn a_catalog_over_the_size_limit_loses_its_youngest_largest_entries_and_names_the_largest() {
        // Each entry here is about 26 KB, so this is well past the limit.
        let mut c = numbered(0);
        let count = MAX_CATALOG_BYTES / 26_000 + 20;
        c.addons = (0..count)
            .map(|i| sized(&format!("gh:old/{i}"), 100, 250))
            .collect();
        c.addons.push(sized("gh:young/small", 200, 1));
        c.addons.push(sized("gh:young/big", 200, 256));
        c.addons.push(sized("gh:chosen/big", 300, 256));

        let picks = picked(&["chosen/big"]);
        let passed = gate_with(c, &Curated::default(), &picks, None, &[]).unwrap();
        let bytes = serde_json::to_vec(&passed.catalog).unwrap();
        assert!(bytes.len() <= MAX_CATALOG_BYTES + 4096);
        let listed = |id: &str| passed.catalog.addons.iter().any(|a| a.id == id);
        assert!(listed("gh:chosen/big"), "hand-picked is never what goes");
        assert!(!listed("gh:young/big"), "the newest and largest went first");
        assert!(!listed("gh:young/small"));
        assert!(listed("gh:old/0"), "and only as many as it took");
        assert!(
            passed.warnings[0].contains("largest entry is gh:young/big")
                || passed.warnings[0].contains("largest entry is gh:chosen/big"),
            "{:?}",
            passed.warnings
        );
        assert!(passed.held.iter().any(|h| h.id == "gh:young/big"));
        assert!(passed.held.iter().all(|h| h.reason.contains("size limit")));
    }

    #[test]
    fn hand_picked_entries_alone_over_the_limit_stop_the_build_and_name_the_largest() {
        let mut c = catalog(vec![sized("gh:chosen/small", 1, 2)]);
        c.addons.push(sized("gh:chosen/big", 1, 256));
        let picks = picked(&["chosen/small", "chosen/big"]);
        let mut warnings = Vec::new();
        let error = trim_to_size(&mut c, &picks, 100, &mut warnings).unwrap_err();
        assert!(error.contains("gh:chosen/big"), "{error}");
        assert!(error.contains("Not publishing"), "{error}");
    }

    #[test]
    fn unknown_references_are_dropped_with_warnings() {
        let mut c = catalog(vec![addon("gh:a/b", "Foo"), addon("gh:a/c", "Bar")]);
        c.featured = vec!["gh:a/b".into(), "gh:gone/x".into()];
        c.kits = vec![
            Kit {
                name: "Good".into(),
                description: "d".into(),
                addons: vec!["gh:a/b".into(), "gh:a/c".into(), "gh:gone/x".into()],
            },
            Kit {
                name: "Hollow".into(),
                description: "d".into(),
                addons: vec!["gh:a/b".into(), "gh:gone/y".into()],
            },
        ];
        c.gaps = vec![Gap {
            name: "Leatrix Plus".into(),
            reason: "r".into(),
            alternatives: vec!["gh:a/c".into(), "gh:gone/x".into()],
            ..Gap::default()
        }];
        let (out, warnings) = pass(c);
        assert_eq!(out.featured, ["gh:a/b"]);
        assert_eq!(out.kits.len(), 1, "a kit left with one addon isn't a kit");
        assert_eq!(out.kits[0].addons, ["gh:a/b", "gh:a/c"]);
        assert_eq!(out.gaps[0].alternatives, ["gh:a/c"]);
        assert!(warnings.iter().any(|w| w.contains("featured")));
        assert!(warnings.iter().any(|w| w.contains("Hollow")));
        assert!(warnings.iter().any(|w| w.contains("alternative")));
    }

    fn group(addon: &str, name: &str, needed: bool, ids: &[&str]) -> PackSpec {
        PackSpec {
            addon: addon.into(),
            name: name.into(),
            needed,
            ids: ids.iter().map(|id| (*id).to_string()).collect(),
        }
    }

    fn spoken_and_packs() -> Catalog {
        catalog(vec![
            addon("gh:rusty-key/spoken-wow", "Spoken"),
            addon("gh:rusty-key/spoken-wow/quests-audio", "Quests Audio"),
            addon("gh:rusty-key/spoken-wow/places-audio", "Places Audio"),
        ])
    }

    fn with_packs(c: Catalog, packs: Vec<PackSpec>) -> Passed {
        let curated = Curated {
            packs,
            ..Curated::default()
        };
        gate_with(c, &curated, &HandPicked::default(), None, &[]).unwrap()
    }

    #[test]
    fn packs_go_on_their_addon_with_ids_as_the_catalog_spells_them() {
        let passed = with_packs(
            spoken_and_packs(),
            vec![
                group(
                    "GH:Rusty-Key/Spoken-WoW",
                    " Quest voices ",
                    true,
                    &["gh:rusty-key/spoken-wow/QUESTS-AUDIO"],
                ),
                group(
                    "gh:rusty-key/spoken-wow",
                    "Place voices",
                    false,
                    &["gh:rusty-key/spoken-wow/places-audio"],
                ),
            ],
        );
        assert!(passed.warnings.is_empty(), "{:?}", passed.warnings);
        let spoken = &passed.catalog.addons[0];
        assert_eq!(
            spoken.packs,
            [
                PackGroup {
                    name: "Quest voices".into(),
                    needed: true,
                    ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
                    met_by: Vec::new(),
                },
                PackGroup {
                    name: "Place voices".into(),
                    needed: false,
                    ids: vec!["gh:rusty-key/spoken-wow/places-audio".into()],
                    met_by: Vec::new(),
                },
            ],
            "in the order they were written"
        );
        assert!(
            passed.catalog.addons[1..]
                .iter()
                .all(|a| a.packs.is_empty())
        );
    }

    #[test]
    fn unlisted_pack_ids_and_the_groups_they_empty_are_dropped_with_warnings() {
        let passed = with_packs(
            spoken_and_packs(),
            vec![
                group(
                    "gh:rusty-key/spoken-wow",
                    "Quest voices",
                    true,
                    &[
                        "gh:rusty-key/spoken-wow/quests-audio-dede",
                        "gh:rusty-key/spoken-wow/quests-audio",
                    ],
                ),
                group(
                    "gh:rusty-key/spoken-wow",
                    "Boss voices",
                    false,
                    &["gh:rusty-key/spoken-wow/bosses-audio"],
                ),
                group(
                    "gh:gone/addon",
                    "Voices",
                    true,
                    &["gh:rusty-key/spoken-wow/quests-audio"],
                ),
            ],
        );
        let spoken = &passed.catalog.addons[0];
        assert_eq!(spoken.packs.len(), 1);
        assert_eq!(
            spoken.packs[0].ids,
            ["gh:rusty-key/spoken-wow/quests-audio"]
        );
        let warned = |text: &str| passed.warnings.iter().any(|w| w.contains(text));
        assert!(
            warned("quests-audio-dede isn't in the catalog"),
            "{:?}",
            passed.warnings
        );
        assert!(warned(
            "packs Boss voices for gh:rusty-key/spoken-wow: nothing left"
        ));
        assert!(warned("packs for gh:gone/addon: it isn't in the catalog"));
        assert_eq!(passed.warnings.len(), 4, "{:?}", passed.warnings);
    }

    #[test]
    fn an_entry_carries_only_the_packs_written_down_now() {
        let mut c = spoken_and_packs();
        c.addons[0].packs = vec![PackGroup {
            name: "Old".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/places-audio".into()],
            met_by: Vec::new(),
        }];
        let passed = with_packs(c, vec![]);
        assert!(passed.catalog.addons[0].packs.is_empty());
    }

    #[test]
    fn a_release_lines_id_is_a_fine_id() {
        let line = addon("gh:rusty-key/spoken-wow/quests-audio", "Quests Audio");
        assert_eq!(addon_problem(&line), None);
    }

    #[test]
    fn only_something_hand_picked_closes_a_gap_and_not_if_declared_unrelated() {
        let build = || {
            let mut c = catalog(vec![
                addon("gh:7hedubz/ForeverPlus", "Forever Plus"),
                addon("gh:a/c", "Bar"),
            ]);
            c.gaps = vec![Gap {
                name: "ForeverPlus".into(),
                reason: "r".into(),
                ..Gap::default()
            }];
            c
        };
        let picks = picked(&["7hedubz/ForeverPlus"]);
        let passed = gate_with(build(), &Curated::default(), &picks, None, &[]).unwrap();
        assert!(passed.catalog.gaps.is_empty());
        assert!(
            passed.warnings[0].contains("in the catalog now"),
            "{:?}",
            passed.warnings
        );

        let (out, warnings) = pass(build());
        assert_eq!(out.gaps.len(), 1, "a name alone doesn't close it");
        assert!(warnings.is_empty(), "{warnings:?}");

        let curated = Curated {
            gaps: vec![GapSpec {
                name: "ForeverPlus".into(),
                unrelated: vec!["gh:7hedubz/ForeverPlus".into()],
                ..GapSpec::default()
            }],
            ..Curated::default()
        };
        let passed = gate_with(build(), &curated, &picks, None, &[]).unwrap();
        assert_eq!(passed.catalog.gaps.len(), 1);
        assert!(passed.warnings.is_empty(), "{:?}", passed.warnings);
    }
}
