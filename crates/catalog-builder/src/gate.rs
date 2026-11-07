//! The last check before anything is published. One bad addon is dropped with a
//! warning, because a single odd repository shouldn't stop the whole catalog from
//! updating. A catalog that looks broken as a whole is refused outright.

use std::collections::BTreeSet;

use feeling_core::catalog::{Addon, CATALOG_SCHEMA, CATEGORIES, Catalog, Release};

use crate::curated::GapSpec;
use crate::text::normalize;

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

/// Check everything and fix what can be fixed. `gaps` carries the hand-written
/// gap list, and `previous` is how many addons the last published catalog had.
/// `Err` means don't publish.
pub fn run_gate(
    mut catalog: Catalog,
    gaps: &[GapSpec],
    previous: Option<usize>,
) -> Result<(Catalog, Vec<String>), String> {
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

    if let Some(previous) = previous
        && catalog.addons.len() * 2 < previous
    {
        return Err(format!(
            "the new catalog has {} addons and the last one had {previous}, so something is wrong. Not publishing.",
            catalog.addons.len()
        ));
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
        let unrelated = gaps
            .iter()
            .find(|g| g.name == gap.name)
            .map_or(&[][..], |g| g.unrelated.as_slice());
        let wanted = normalize(&gap.name);
        let present = catalog
            .addons
            .iter()
            .find(|a| normalize(&a.name) == wanted && !unrelated.contains(&a.id));
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
    Ok((catalog, warnings))
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::{Gap, Kit, Move, Release, Source};

    use super::*;

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

    fn pass(c: Catalog) -> (Catalog, Vec<String>) {
        run_gate(c, &[], None).unwrap()
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
        assert!(run_gate(c, &[], None).is_err());
    }

    #[test]
    fn fewer_than_half_the_previous_addons_is_refused() {
        let three = || {
            catalog(vec![
                addon("gh:a/1", "A"),
                addon("gh:a/2", "B"),
                addon("gh:a/3", "C"),
            ])
        };
        assert!(
            run_gate(three(), &[], Some(6)).is_ok(),
            "exactly half is allowed"
        );
        let error = run_gate(three(), &[], Some(7)).unwrap_err();
        assert!(error.contains("Not publishing"), "{error}");
        assert!(run_gate(three(), &[], Some(0)).is_ok());
        assert!(run_gate(three(), &[], None).is_ok());
    }

    #[test]
    fn an_empty_catalog_is_refused_even_without_a_previous_one() {
        assert!(run_gate(catalog(vec![]), &[], None).is_err());
        let all_bad = catalog(vec![Addon {
            id: "x".into(),
            ..Addon::default()
        }]);
        assert!(run_gate(all_bad, &[], None).is_err());
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

    #[test]
    fn a_gap_naming_a_present_addon_is_dropped_unless_declared_unrelated() {
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
        let (out, warnings) = pass(build());
        assert!(out.gaps.is_empty());
        assert!(warnings[0].contains("in the catalog now"), "{warnings:?}");

        let spec = GapSpec {
            name: "ForeverPlus".into(),
            unrelated: vec!["gh:7hedubz/ForeverPlus".into()],
            ..GapSpec::default()
        };
        let (out, warnings) = run_gate(build(), &[spec], None).unwrap();
        assert_eq!(out.gaps.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
