//! Finds the newest Forever build in a repository's recent releases. The order
//! comes from `docs/DESIGN.md`, and each step exists because a real addon needs it:
//! `release.json` metadata, then a zip named for Forever, then the TOCs inside
//! candidate zips. `/releases/latest` alone can't be trusted, because the newest
//! release is often a build for another flavor (VuhDo and Grid2 do this).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::ZipScan;
use crate::text::{normalize, tokens, zip_stem};

/// 2026-07-01. Nothing older can target Forever, and WoWInterface uses the same line.
pub const FOREVER_EPOCH: i64 = 1_782_864_000;
/// After this many releases that were read and don't target Forever, older ones
/// aren't opened any more. Keeps a first run sane for active retail-only repos.
const MAX_NEGATIVE_READS: u32 = 4;
/// Zips opened per release when nothing in the names helps. A real release can have
/// several zips with the game in their name, such as a game client beside the addon.
const MAX_ZIPS_PER_RELEASE: usize = 5;
/// A newer release this far past the Forever one means support really was dropped,
/// rather than the author publishing per-flavor releases in close succession.
const DROPPED_AFTER_SECS: i64 = 45 * 24 * 60 * 60;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhAsset {
    pub name: String,
    pub size: u64,
    pub downloads: u64,
    pub url: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhRelease {
    pub tag: String,
    pub prerelease: bool,
    pub published: i64,
    pub assets: Vec<GhAsset>,
}

/// Network access for detection. A trait so the logic can be tested with fakes.
pub trait Probe {
    fn release_json(&mut self, asset: &GhAsset) -> Result<String, String>;
    fn read_zip(&mut self, asset: &GhAsset, hints: &[&str]) -> Result<ZipScan, String>;
}

/// Cached outcome for one release. `sig` covers its assets, so replacing a file invalidates it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseVerdict {
    pub sig: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<ZipScan>,
    /// A release for the addon's main flavor was examined and lacks Forever.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirmed_negative: bool,
    /// The release was read for the addon with this main folder, and every Forever build in
    /// it is another addon's. Read for any other addon, the same zips could be its build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_for: Option<String>,
}

impl ReleaseVerdict {
    /// Whether this verdict still answers for an entry pinned to `pin`.
    fn fits(&self, pin: Option<&str>) -> bool {
        match (&self.scan, &self.skipped_for) {
            (Some(scan), _) => ships(&scan.folders, pin),
            (None, Some(wanted)) => pin.is_some_and(|pin| pin.eq_ignore_ascii_case(wanted)),
            (None, None) => true,
        }
    }
}

/// Whether a build is still the addon an entry is pinned to: it ships the main folder the
/// last catalog listed the entry with. Folders it adds, or a new main folder among them,
/// don't matter. With no pin, any build is.
pub fn ships(folders: &[String], pin: Option<&str>) -> bool {
    pin.is_none_or(|main| {
        folders
            .iter()
            .any(|folder| folder.eq_ignore_ascii_case(main))
    })
}

/// A release with a Forever build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub tag: String,
    pub published: i64,
    pub asset: GhAsset,
    pub scan: ZipScan,
}

#[derive(Debug, Default)]
pub struct Chosen {
    pub stable: Option<Build>,
    pub beta: Option<Build>,
    /// Releases that couldn't be examined this run.
    pub errors: Vec<String>,
    pub no_longer_forever: bool,
    /// The newest release passed over because its Forever build is another addon's.
    pub skipped: Option<String>,
}

/// What `release.json` says about a release.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReleaseMeta {
    /// File names of Forever builds, the ones with libraries first.
    pub forever_files: Vec<String>,
    pub lists_mainline: bool,
}

#[derive(Deserialize)]
struct RawMeta {
    #[serde(default)]
    releases: Vec<RawMetaRelease>,
}

#[derive(Deserialize)]
struct RawMetaRelease {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    nolib: bool,
    #[serde(default)]
    metadata: Vec<RawMetaEntry>,
}

#[derive(Deserialize)]
struct RawMetaEntry {
    #[serde(default)]
    flavor: String,
    #[serde(default)]
    interface: serde_json::Value,
}

/// Parse `release.json`. It's untrusted data: only these few fields are read.
pub fn parse_release_json(text: &str) -> Result<ReleaseMeta, String> {
    let raw: RawMeta =
        serde_json::from_str(text).map_err(|e| format!("release.json doesn't parse: {e}"))?;
    let mut meta = ReleaseMeta::default();
    let mut nolib_files = Vec::new();
    for release in raw.releases {
        let mut is_forever = false;
        for entry in &release.metadata {
            let flavor = entry.flavor.to_ascii_lowercase();
            meta.lists_mainline |= matches!(flavor.as_str(), "mainline" | "retail");
            let interface_ok = entry
                .interface
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .is_some_and(feeling_core::toc::is_forever_interface);
            is_forever |= matches!(flavor.as_str(), "forever" | "camelot") || interface_ok;
        }
        if is_forever && !release.filename.is_empty() {
            if release.nolib {
                nolib_files.push(release.filename);
            } else {
                meta.forever_files.push(release.filename);
            }
        }
    }
    meta.forever_files.extend(nolib_files);
    Ok(meta)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Flavor {
    Forever,
    Other,
    Unlabeled,
}

const OTHER_FLAVORS: [&str; 18] = [
    "vanilla",
    "classic",
    "classicera",
    "era",
    "bcc",
    "tbc",
    "tbcc",
    "wrath",
    "wotlk",
    "wotlkc",
    "cata",
    "cataclysm",
    "mists",
    "mop",
    "sod",
    "hardcore",
    "anniversary",
    "pandaria",
];

pub fn flavor_of(name: &str) -> Flavor {
    let lower = name.to_ascii_lowercase();
    if lower.contains("forever") || lower.contains("camelot") {
        return Flavor::Forever;
    }
    let words = lower.split(|c: char| !c.is_ascii_alphanumeric());
    let other = words.into_iter().any(|word| {
        let word = word.trim_end_matches(|c: char| c.is_ascii_digit());
        OTHER_FLAVORS.contains(&word)
    });
    if other {
        Flavor::Other
    } else {
        Flavor::Unlabeled
    }
}

fn is_zip(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".zip")
}

fn is_nolib(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("nolib") || lower.contains("no-lib") || lower.contains("no_lib")
}

/// Alpha, beta and release candidate tags count as pre-releases even when the
/// author forgot to tick the box. Stable players shouldn't get those by accident.
pub fn is_prerelease(release: &GhRelease) -> bool {
    release.prerelease || tag_is_prerelease(&release.tag)
}

pub fn tag_is_prerelease(tag: &str) -> bool {
    tokens(tag).iter().any(|t| {
        matches!(
            t.trim_end_matches(|c: char| c.is_ascii_digit()),
            "alpha" | "beta" | "rc" | "pre" | "prerelease" | "dev" | "nightly"
        )
    })
}

/// Changes when a release's files change. The digest is what catches a file replaced
/// by another of exactly the same size, which would otherwise keep the old file's scan.
pub fn assets_signature(assets: &[GhAsset]) -> String {
    let mut parts: Vec<String> = assets
        .iter()
        .map(|a| {
            format!(
                "{}:{}:{}",
                a.name,
                a.size,
                a.sha256.as_deref().unwrap_or_default()
            )
        })
        .collect();
    parts.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.join("\n").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Decide whether one release has a Forever build, and read its TOCs if so. With a `pin`,
/// only a build that still ships that main folder counts: one release can hold several
/// addons' zips, and another addon's is passed over, never counted against the release.
/// `Err` means something failed along the way, and nothing should be cached.
pub fn classify_release(
    release: &GhRelease,
    repo_name: &str,
    allow_toc_reads: bool,
    pin: Option<&str>,
    probe: &mut dyn Probe,
) -> Result<ReleaseVerdict, String> {
    let mut verdict = ReleaseVerdict {
        sig: assets_signature(&release.assets),
        ..ReleaseVerdict::default()
    };
    let zips: Vec<&GhAsset> = release.assets.iter().filter(|a| is_zip(&a.name)).collect();
    if zips.is_empty() {
        return Ok(verdict);
    }

    let mut meta = ReleaseMeta::default();
    if let Some(file) = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("release.json"))
    {
        match parse_release_json(&probe.release_json(file)?) {
            Ok(parsed) => meta = parsed,
            Err(problem) => eprintln!("{repo_name} {}: {problem}", release.tag),
        }
    }

    let mut queue: Vec<&GhAsset> = Vec::new();
    for file in &meta.forever_files {
        queue.extend(
            zips.iter()
                .find(|a| a.name.eq_ignore_ascii_case(file))
                .copied(),
        );
    }
    let repo_norm = normalize(repo_name);
    let best_first = |a: &&GhAsset| {
        let stem = normalize(&zip_stem(&a.name));
        (
            is_nolib(&a.name),
            stem != repo_norm,
            std::cmp::Reverse(a.size),
        )
    };
    let mut named: Vec<&GhAsset> = zips
        .iter()
        .copied()
        .filter(|a| flavor_of(&a.name) == Flavor::Forever)
        .collect();
    named.sort_by_key(best_first);
    queue.extend(named.into_iter().take(MAX_ZIPS_PER_RELEASE));

    let release_is_flavored = flavor_of(&release.tag) == Flavor::Other;
    if allow_toc_reads && !release_is_flavored {
        let mut unlabeled: Vec<&GhAsset> = zips
            .iter()
            .copied()
            .filter(|a| flavor_of(&a.name) == Flavor::Unlabeled && !is_nolib(&a.name))
            .collect();
        unlabeled.sort_by_key(best_first);
        queue.extend(unlabeled.into_iter().take(MAX_ZIPS_PER_RELEASE));
    }

    let mut seen: Vec<&str> = Vec::new();
    let mut read_something = false;
    let mut passed_over = false;
    for asset in queue {
        if seen.contains(&asset.name.as_str()) {
            continue;
        }
        seen.push(&asset.name);
        let stem = zip_stem(&asset.name);
        let scan = probe.read_zip(asset, &[repo_name, &stem])?;
        if !scan.forever || scan.folders.is_empty() {
            read_something = true;
        } else if ships(&scan.folders, pin) {
            verdict.asset = Some(asset.name.clone());
            verdict.scan = Some(scan);
            return Ok(verdict);
        } else {
            passed_over = true;
        }
    }
    if passed_over {
        // Another addon's release, which says nothing about whether this one still has Forever.
        verdict.skipped_for = pin.map(str::to_string);
        return Ok(verdict);
    }
    verdict.confirmed_negative = read_something || (meta.lists_mainline && !release_is_flavored);
    Ok(verdict)
}

/// Look through a repository's releases, newest first, for its newest stable
/// Forever build and any pre-release newer than that. `cache` holds verdicts by
/// tag: hits cost nothing, and tags that are gone get pruned.
/// `pin` is the main folder the last catalog listed the entry with, when it did: a build
/// that no longer ships it is another addon, and its release is passed over, stable or not.
/// `out_of_time` stops new network work, leaving the rest to the next run.
pub fn choose_builds(
    repo_name: &str,
    releases: &[GhRelease],
    cache: &mut BTreeMap<String, ReleaseVerdict>,
    pin: Option<&str>,
    probe: &mut dyn Probe,
    out_of_time: &dyn Fn() -> bool,
) -> Chosen {
    let mut chosen = Chosen::default();
    let mut ordered: Vec<&GhRelease> = releases.iter().collect();
    ordered.sort_by_key(|r| std::cmp::Reverse(r.published));

    let mut negative_reads = 0;
    let mut newest_negative: Option<i64> = None;
    for release in ordered {
        if release.published < FOREVER_EPOCH {
            continue;
        }
        let sig = assets_signature(&release.assets);
        let cached = cache
            .get(&release.tag)
            .filter(|v| v.sig == sig && v.fits(pin));
        let verdict = match cached {
            Some(hit) => hit.clone(),
            None => {
                if out_of_time() {
                    chosen.errors.push(format!("{}: out of time", release.tag));
                    continue;
                }
                let allow_reads = negative_reads < MAX_NEGATIVE_READS;
                match classify_release(release, repo_name, allow_reads, pin, probe) {
                    Ok(fresh) => {
                        cache.insert(release.tag.clone(), fresh.clone());
                        fresh
                    }
                    Err(problem) => {
                        chosen.errors.push(format!("{}: {problem}", release.tag));
                        continue;
                    }
                }
            }
        };
        if verdict.skipped_for.is_some() {
            chosen.skipped.get_or_insert_with(|| release.tag.clone());
            continue;
        }

        let build = verdict.asset.as_ref().and_then(|name| {
            let asset = release.assets.iter().find(|a| &a.name == name)?;
            Some(Build {
                tag: release.tag.clone(),
                published: release.published,
                asset: asset.clone(),
                scan: verdict.scan.clone()?,
            })
        });
        let stable_kind = !is_prerelease(release);
        match build {
            Some(build) if stable_kind => {
                chosen.no_longer_forever =
                    newest_negative.is_some_and(|date| date - build.published > DROPPED_AFTER_SECS);
                chosen.stable = Some(build);
                break;
            }
            Some(build) => {
                if chosen.beta.is_none() {
                    chosen.beta = Some(build);
                }
            }
            None if verdict.confirmed_negative => {
                negative_reads += 1;
                if stable_kind && newest_negative.is_none() {
                    newest_negative = Some(release.published);
                }
            }
            None => {}
        }
    }
    // Keep verdicts for every release still listed, so a later run doesn't re-read them.
    cache.retain(|tag, _| releases.iter().any(|r| &r.tag == tag));
    chosen
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        release_json: HashMap<String, Result<String, String>>,
        zips: HashMap<String, Result<ZipScan, String>>,
        json_reads: Vec<String>,
        zip_reads: Vec<String>,
    }

    impl Probe for Fake {
        fn release_json(&mut self, asset: &GhAsset) -> Result<String, String> {
            self.json_reads.push(asset.url.clone());
            self.release_json
                .get(&asset.url)
                .cloned()
                .unwrap_or(Err("no such file".into()))
        }
        fn read_zip(&mut self, asset: &GhAsset, _: &[&str]) -> Result<ZipScan, String> {
            self.zip_reads.push(asset.name.clone());
            self.zips
                .get(&asset.name)
                .cloned()
                .unwrap_or(Err("no such zip".into()))
        }
    }

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.into(),
            size: 1000,
            downloads: 5,
            url: format!("https://x.test/{name}"),
            sha256: None,
        }
    }

    fn release(tag: &str, age_days: i64, names: &[&str]) -> GhRelease {
        GhRelease {
            tag: tag.into(),
            prerelease: false,
            published: FOREVER_EPOCH + 200 * 86_400 - age_days * 86_400,
            assets: names.iter().map(|n| asset(n)).collect(),
        }
    }

    fn forever_scan(folder: &str) -> ZipScan {
        ZipScan {
            folders: vec![folder.into()],
            forever: true,
            title: Some(folder.into()),
            ..ZipScan::default()
        }
    }

    fn other_scan(folder: &str) -> ZipScan {
        ZipScan {
            folders: vec![folder.into()],
            forever: false,
            ..ZipScan::default()
        }
    }

    fn run(fake: &mut Fake, releases: &[GhRelease]) -> (Chosen, BTreeMap<String, ReleaseVerdict>) {
        let mut cache = BTreeMap::new();
        let chosen = choose_builds("Addon", releases, &mut cache, None, fake, &|| false);
        (chosen, cache)
    }

    const JSON_FOREVER: &str = r#"{"releases":[{"filename":"Addon-1.zip","nolib":false,
        "metadata":[{"flavor":"mainline","interface":120105},{"flavor":"forever","interface":16001}]}]}"#;
    const JSON_MAINLINE: &str = r#"{"releases":[{"filename":"Addon-1.zip","nolib":false,
        "metadata":[{"flavor":"mainline","interface":120105}]}]}"#;

    #[test]
    fn release_json_parsing_covers_flavors_interfaces_and_nolib() {
        let meta = parse_release_json(JSON_FOREVER).unwrap();
        assert_eq!(meta.forever_files, ["Addon-1.zip"]);
        assert!(meta.lists_mainline);

        let camelot = r#"{"releases":[{"filename":"a.zip","metadata":[{"flavor":"camelot"}]}]}"#;
        assert_eq!(
            parse_release_json(camelot).unwrap().forever_files,
            ["a.zip"]
        );

        let by_number =
            r#"{"releases":[{"filename":"a.zip","metadata":[{"flavor":"x","interface":16001}]}]}"#;
        assert_eq!(
            parse_release_json(by_number).unwrap().forever_files,
            ["a.zip"]
        );
        let too_high =
            r#"{"releases":[{"filename":"a.zip","metadata":[{"flavor":"x","interface":20000}]}]}"#;
        assert!(
            parse_release_json(too_high)
                .unwrap()
                .forever_files
                .is_empty()
        );

        let nolib_first = r#"{"releases":[
            {"filename":"nolib.zip","nolib":true,"metadata":[{"flavor":"forever"}]},
            {"filename":"full.zip","nolib":false,"metadata":[{"flavor":"forever"}]}]}"#;
        assert_eq!(
            parse_release_json(nolib_first).unwrap().forever_files,
            ["full.zip", "nolib.zip"]
        );

        let odd_types = r#"{"releases":[{"filename":"a.zip","metadata":[{"flavor":"forever","interface":"16001"}]}]}"#;
        assert_eq!(
            parse_release_json(odd_types).unwrap().forever_files,
            ["a.zip"]
        );

        assert!(parse_release_json("<html>").is_err());
        assert!(parse_release_json("{}").unwrap().forever_files.is_empty());
    }

    #[test]
    fn step_one_release_json_picks_the_file_it_names() {
        let mut fake = Fake::default();
        fake.release_json.insert(
            "https://x.test/release.json".into(),
            Ok(JSON_FOREVER.into()),
        );
        fake.zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Addon")));
        let (chosen, _) = run(
            &mut fake,
            &[release(
                "v1",
                1,
                &["Other-1.zip", "Addon-1.zip", "release.json"],
            )],
        );
        assert_eq!(chosen.stable.unwrap().asset.name, "Addon-1.zip");
        assert_eq!(
            fake.zip_reads,
            ["Addon-1.zip"],
            "one zip read, nothing else opened"
        );
    }

    #[test]
    fn step_two_a_zip_named_for_forever_beats_reading_others() {
        // Decursive: release.json only lists mainline, yet a -forever zip exists.
        let mut fake = Fake::default();
        fake.release_json.insert(
            "https://x.test/release.json".into(),
            Ok(JSON_MAINLINE.into()),
        );
        fake.zips
            .insert("Addon-1-forever.zip".into(), Ok(forever_scan("Addon")));
        let (chosen, _) = run(
            &mut fake,
            &[release(
                "v1",
                1,
                &["Addon-1.zip", "Addon-1-forever.zip", "release.json"],
            )],
        );
        assert_eq!(chosen.stable.unwrap().asset.name, "Addon-1-forever.zip");
        assert_eq!(fake.zip_reads, ["Addon-1-forever.zip"]);
    }

    #[test]
    fn step_three_reads_the_toc_when_nothing_else_says() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Addon")));
        let (chosen, _) = run(&mut fake, &[release("v1", 1, &["Addon-1.zip"])]);
        assert_eq!(chosen.stable.unwrap().scan.folders, ["Addon"]);
    }

    #[test]
    fn a_zip_named_forever_whose_toc_disagrees_is_not_accepted() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Forever-Thing-1.zip".into(), Ok(other_scan("Thing")));
        let (chosen, cache) = run(&mut fake, &[release("v1", 1, &["Forever-Thing-1.zip"])]);
        assert!(chosen.stable.is_none());
        assert!(cache["v1"].confirmed_negative);
    }

    #[test]
    fn vuhdo_trap_the_latest_release_is_another_flavor() {
        let mut fake = Fake::default();
        fake.zips
            .insert("VuhDo-3.219.zip".into(), Ok(forever_scan("VuhDo")));
        let releases = [
            release("v3.220-vanilla", 1, &["VuhDo-3.220.zip"]),
            release("v3.219", 5, &["VuhDo-3.219.zip"]),
        ];
        let (chosen, _) = run(&mut fake, &releases);
        assert_eq!(chosen.stable.unwrap().tag, "v3.219");
        assert_eq!(
            fake.zip_reads,
            ["VuhDo-3.219.zip"],
            "the vanilla zip isn't opened"
        );
    }

    #[test]
    fn grid2_trap_the_latest_release_only_has_other_flavors() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Grid2-2.9.35.zip".into(), Ok(forever_scan("Grid2")));
        let releases = [
            release(
                "2.9.36",
                1,
                &[
                    "Grid2-2.9.36-bcc.zip",
                    "Grid2-2.9.36-classic.zip",
                    "Grid2-2.9.36-mists.zip",
                ],
            ),
            release("2.9.35", 5, &["Grid2-2.9.35.zip"]),
        ];
        let (chosen, _) = run(&mut fake, &releases);
        assert_eq!(chosen.stable.unwrap().tag, "2.9.35");
        assert_eq!(fake.zip_reads, ["Grid2-2.9.35.zip"]);
    }

    #[test]
    fn stable_and_newer_beta_are_both_found() {
        let mut fake = Fake::default();
        fake.zips.insert("A-2b.zip".into(), Ok(forever_scan("A")));
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let mut beta = release("v2-beta1", 1, &["A-2b.zip"]);
        beta.prerelease = true;
        let releases = [
            beta,
            release("v1", 3, &["A-1.zip"]),
            release("v0", 4, &["A-0.zip"]),
        ];
        let (chosen, _) = run(&mut fake, &releases);
        assert_eq!(chosen.beta.unwrap().tag, "v2-beta1");
        assert_eq!(chosen.stable.unwrap().tag, "v1");
        assert!(
            !fake.zip_reads.contains(&"A-0.zip".to_string()),
            "stops at the first stable"
        );
    }

    #[test]
    fn an_older_prerelease_is_ignored_and_a_beta_only_addon_still_works() {
        let mut fake = Fake::default();
        fake.zips.insert("A-2.zip".into(), Ok(forever_scan("A")));
        let mut old_beta = release("v1-beta", 9, &["A-1b.zip"]);
        old_beta.prerelease = true;
        let (chosen, _) = run(
            &mut fake,
            &[release("v2", 1, &["A-2.zip"]), old_beta.clone()],
        );
        assert!(chosen.beta.is_none());
        assert_eq!(chosen.stable.unwrap().tag, "v2");

        let mut fake = Fake::default();
        fake.zips.insert("A-1b.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(&mut fake, &[old_beta]);
        assert!(chosen.stable.is_none());
        assert_eq!(chosen.beta.unwrap().tag, "v1-beta");
    }

    #[test]
    fn an_unticked_beta_tag_is_still_a_prerelease() {
        assert!(is_prerelease(&release("v1.2-beta3", 1, &[])));
        assert!(is_prerelease(&release("2.9.0-RC4", 1, &[])));
        assert!(is_prerelease(&release("v12.1.5-alpha", 1, &[])));
        assert!(!is_prerelease(&release("v12.6.2", 1, &[])));
        assert!(!is_prerelease(&release("bundle/v12.6.2+v1.0.11", 1, &[])));
        assert!(!is_prerelease(&release("Alphabet-1.0", 1, &[])));
    }

    #[test]
    fn a_cached_release_costs_nothing_on_the_next_run() {
        let mut fake = Fake::default();
        fake.release_json.insert(
            "https://x.test/release.json".into(),
            Ok(JSON_FOREVER.into()),
        );
        fake.zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Addon")));
        let releases = [release("v1", 1, &["Addon-1.zip", "release.json"])];
        let mut cache = BTreeMap::new();
        let first = choose_builds("Addon", &releases, &mut cache, None, &mut fake, &|| false);
        assert!(first.stable.is_some());
        let (json_reads, zip_reads) = (fake.json_reads.len(), fake.zip_reads.len());
        let second = choose_builds("Addon", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(second.stable, first.stable);
        assert_eq!(fake.json_reads.len(), json_reads);
        assert_eq!(fake.zip_reads.len(), zip_reads);
    }

    #[test]
    fn negative_results_are_cached_too() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(other_scan("A")));
        let releases = [release("v1", 1, &["A-1.zip"])];
        let mut cache = BTreeMap::new();
        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(fake.zip_reads.len(), 1);
    }

    #[test]
    fn replacing_an_asset_invalidates_the_cache() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(other_scan("A")));
        let mut releases = [release("v1", 1, &["A-1.zip"])];
        let mut cache = BTreeMap::new();
        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        releases[0].assets[0].size = 2000;
        let chosen = choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        assert!(chosen.stable.is_some());
    }

    #[test]
    fn replacing_an_asset_with_one_of_the_same_size_is_noticed_by_its_digest() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("Cool")));
        let mut releases = [release("v1", 1, &["A-1.zip"])];
        releases[0].assets[0].sha256 = Some("a".repeat(64));
        let mut cache = BTreeMap::new();
        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(fake.zip_reads.len(), 1);

        // Same name and size, different bytes: the old folders must not stay paired
        // with the new digest.
        fake.zips
            .insert("A-1.zip".into(), Ok(forever_scan("Other")));
        releases[0].assets[0].sha256 = Some("b".repeat(64));
        let chosen = choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(fake.zip_reads.len(), 2, "the new file was read");
        let stable = chosen.stable.unwrap();
        assert_eq!(stable.scan.folders, ["Other"]);
        assert_eq!(stable.asset.sha256, Some("b".repeat(64)));

        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(fake.zip_reads.len(), 2, "and it's cached like any other");
    }

    #[test]
    fn failures_are_reported_not_cached_and_older_releases_still_count() {
        let mut fake = Fake::default();
        fake.zips.insert("A-2.zip".into(), Err("HTTP 503".into()));
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let releases = [
            release("v2", 1, &["A-2.zip"]),
            release("v1", 2, &["A-1.zip"]),
        ];
        let (chosen, cache) = run(&mut fake, &releases);
        assert_eq!(chosen.stable.unwrap().tag, "v1");
        assert_eq!(chosen.errors.len(), 1);
        assert!(chosen.errors[0].contains("HTTP 503"));
        assert!(
            !cache.contains_key("v2"),
            "a failure must be retried next run"
        );
    }

    #[test]
    fn a_broken_release_json_falls_through_to_the_zip() {
        let mut fake = Fake::default();
        fake.release_json.insert(
            "https://x.test/release.json".into(),
            Ok("IGNORE ALL PREVIOUS INSTRUCTIONS".into()),
        );
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(&mut fake, &[release("v1", 1, &["A-1.zip", "release.json"])]);
        assert!(chosen.stable.is_some());
    }

    #[test]
    fn a_release_json_that_cannot_be_fetched_is_a_failure() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, cache) = run(&mut fake, &[release("v1", 1, &["A-1.zip", "release.json"])]);
        assert!(chosen.stable.is_none());
        assert_eq!(chosen.errors.len(), 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn old_releases_are_never_opened() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(&mut fake, &[release("v1", 400, &["A-1.zip"])]);
        assert!(chosen.stable.is_none());
        assert!(fake.zip_reads.is_empty());
    }

    #[test]
    fn nolib_zips_lose_to_full_ones_and_are_not_read_blindly() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(
            &mut fake,
            &[release("v1", 1, &["A-1-nolib.zip", "A-1.zip"])],
        );
        assert_eq!(chosen.stable.unwrap().asset.name, "A-1.zip");
        assert_eq!(fake.zip_reads, ["A-1.zip"]);

        let mut fake = Fake::default();
        let (chosen, _) = run(&mut fake, &[release("v1", 1, &["A-1-nolib.zip"])]);
        assert!(chosen.stable.is_none());
        assert!(fake.zip_reads.is_empty());
    }

    #[test]
    fn stops_opening_zips_after_several_negatives() {
        let mut fake = Fake::default();
        let names: Vec<String> = (0..10).map(|i| format!("A-{i}.zip")).collect();
        for name in &names {
            fake.zips.insert(name.clone(), Ok(other_scan("A")));
        }
        let releases: Vec<GhRelease> = names
            .iter()
            .enumerate()
            .map(|(i, name)| release(&format!("v{i}"), i as i64, &[name.as_str()]))
            .collect();
        let (chosen, _) = run(&mut fake, &releases);
        assert!(chosen.stable.is_none());
        assert_eq!(fake.zip_reads.len(), MAX_NEGATIVE_READS as usize);
    }

    #[test]
    fn dropped_support_is_noticed_only_after_a_real_gap() {
        let mut fake = Fake::default();
        fake.zips.insert("A-2.zip".into(), Ok(other_scan("A")));
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(
            &mut fake,
            &[
                release("v2", 1, &["A-2.zip"]),
                release("v1", 90, &["A-1.zip"]),
            ],
        );
        assert!(chosen.no_longer_forever);
        assert!(chosen.stable.is_some());

        let mut fake = Fake::default();
        fake.zips.insert("A-2.zip".into(), Ok(other_scan("A")));
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let (chosen, _) = run(
            &mut fake,
            &[
                release("v2", 1, &["A-2.zip"]),
                release("v1", 2, &["A-1.zip"]),
            ],
        );
        assert!(
            !chosen.no_longer_forever,
            "two releases days apart can be per-flavor builds"
        );
    }

    #[test]
    fn running_out_of_time_keeps_what_is_cached_and_reports_the_rest() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(forever_scan("A")));
        let releases = [release("v1", 1, &["A-1.zip"])];
        let mut cache = BTreeMap::new();
        let chosen = choose_builds("A", &releases, &mut cache, None, &mut fake, &|| true);
        assert!(chosen.stable.is_none());
        assert_eq!(chosen.errors.len(), 1);
        assert!(fake.zip_reads.is_empty());

        choose_builds("A", &releases, &mut cache, None, &mut fake, &|| false);
        let chosen = choose_builds("A", &releases, &mut cache, None, &mut fake, &|| true);
        assert!(chosen.stable.is_some(), "a cached verdict needs no network");
    }

    #[test]
    fn verdicts_for_deleted_releases_are_pruned() {
        let mut fake = Fake::default();
        fake.zips.insert("A-1.zip".into(), Ok(other_scan("A")));
        let mut cache = BTreeMap::new();
        choose_builds(
            "A",
            &[release("v1", 1, &["A-1.zip"])],
            &mut cache,
            None,
            &mut fake,
            &|| false,
        );
        assert!(cache.contains_key("v1"));
        choose_builds("A", &[], &mut cache, None, &mut fake, &|| false);
        assert!(cache.is_empty());
    }

    fn pinned(
        fake: &mut Fake,
        releases: &[GhRelease],
        cache: &mut BTreeMap<String, ReleaseVerdict>,
        pin: &str,
    ) -> Chosen {
        choose_builds("Repo", releases, cache, Some(pin), fake, &|| false)
    }

    #[test]
    fn another_addons_releases_are_passed_over_without_counting_as_negatives() {
        let mut fake = Fake::default();
        let mut releases: Vec<GhRelease> = (0..6)
            .map(|i| {
                let name = format!("Pack-{i}.zip");
                fake.zips.insert(name.clone(), Ok(forever_scan("Pack")));
                release(&format!("pack/v{i}"), i, &[name.as_str()])
            })
            .collect();
        fake.zips
            .insert("Main-1.zip".into(), Ok(forever_scan("Main")));
        releases.push(release("v1", 10, &["Main-1.zip"]));

        let mut cache = BTreeMap::new();
        let chosen = pinned(&mut fake, &releases, &mut cache, "main");
        assert_eq!(
            chosen.stable.unwrap().tag,
            "v1",
            "six of another addon's releases and the pinned one is still reached"
        );
        assert_eq!(chosen.skipped.as_deref(), Some("pack/v0"), "the newest");
        assert!(!chosen.no_longer_forever);
        let verdict = &cache["pack/v0"];
        assert!(!verdict.confirmed_negative);
        assert_eq!(verdict.skipped_for.as_deref(), Some("main"));
        assert_eq!(verdict.asset, None);
    }

    #[test]
    fn the_beta_pick_is_pinned_too() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Pack-2b.zip".into(), Ok(forever_scan("Pack")));
        fake.zips
            .insert("Main-2b.zip".into(), Ok(forever_scan("Main")));
        fake.zips
            .insert("Main-1.zip".into(), Ok(forever_scan("Main")));
        let mut pack_beta = release("pack/v2-beta", 1, &["Pack-2b.zip"]);
        pack_beta.prerelease = true;
        let mut main_beta = release("v2-beta", 2, &["Main-2b.zip"]);
        main_beta.prerelease = true;
        let releases = [pack_beta, main_beta, release("v1", 3, &["Main-1.zip"])];

        let chosen = pinned(&mut fake, &releases, &mut BTreeMap::new(), "Main");
        assert_eq!(chosen.beta.unwrap().tag, "v2-beta");
        assert_eq!(chosen.stable.unwrap().tag, "v1");
        assert_eq!(chosen.skipped.as_deref(), Some("pack/v2-beta"));
    }

    #[test]
    fn a_release_with_several_addons_offers_the_pinned_ones_zip() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Pack.zip".into(), Ok(forever_scan("Pack")));
        fake.zips
            .insert("Main.zip".into(), Ok(forever_scan("Main")));
        let releases = [release("v1", 1, &["Pack.zip", "Main.zip"])];

        let chosen = pinned(&mut fake, &releases, &mut BTreeMap::new(), "Main");
        assert_eq!(chosen.stable.unwrap().asset.name, "Main.zip");
        assert_eq!(fake.zip_reads, ["Pack.zip", "Main.zip"]);
        assert_eq!(
            chosen.skipped, None,
            "the release had the pinned addon after all"
        );
    }

    #[test]
    fn a_verdict_is_read_again_only_for_another_pin() {
        let mut fake = Fake::default();
        fake.zips
            .insert("Pack.zip".into(), Ok(forever_scan("Pack")));
        let releases = [release("pack/v1", 1, &["Pack.zip"])];
        let mut cache = BTreeMap::new();

        // A brand-new entry takes whatever the release has.
        let first = choose_builds("Repo", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(first.stable.unwrap().scan.folders, ["Pack"]);
        assert_eq!(fake.zip_reads.len(), 1);

        // Pinned to another addon, that answer doesn't hold, so the release is read again.
        let pinned_once = pinned(&mut fake, &releases, &mut cache, "Main");
        assert!(pinned_once.stable.is_none());
        assert_eq!(pinned_once.skipped.as_deref(), Some("pack/v1"));
        assert_eq!(fake.zip_reads.len(), 2);

        // The same pin costs nothing on the next run.
        pinned(&mut fake, &releases, &mut cache, "MAIN");
        assert_eq!(fake.zip_reads.len(), 2, "folders compare ignoring case");

        // Unpinned, or pinned to the pack itself, the release is the pack's build again.
        let free = choose_builds("Repo", &releases, &mut cache, None, &mut fake, &|| false);
        assert_eq!(free.stable.unwrap().scan.folders, ["Pack"]);
        assert_eq!(fake.zip_reads.len(), 3);
        let pack = pinned(&mut fake, &releases, &mut cache, "Pack");
        assert!(pack.stable.is_some());
        assert_eq!(
            fake.zip_reads.len(),
            3,
            "the cached build ships the pack's folder"
        );
    }

    #[test]
    fn a_build_still_ships_its_main_folder_whatever_else_it_adds() {
        let folders =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        assert!(ships(&folders(&["Main"]), Some("main")));
        assert!(ships(&folders(&["Main_Options", "Main"]), Some("Main")));
        assert!(!ships(&folders(&["MainAudio"]), Some("Main")));
        assert!(!ships(&[], Some("Main")));
        assert!(ships(&folders(&["Anything"]), None));
    }

    #[test]
    fn flavor_labels_are_whole_words() {
        assert_eq!(flavor_of("Foo-classic.zip"), Flavor::Other);
        assert_eq!(flavor_of("Foo-1.2-mists.zip"), Flavor::Other);
        assert_eq!(flavor_of("Foo-wrath3.zip"), Flavor::Other);
        assert_eq!(
            flavor_of("ClassicUI-1.2.zip"),
            Flavor::Unlabeled,
            "part of a name, not a label"
        );
        assert_eq!(flavor_of("Foo.zip"), Flavor::Unlabeled);
        assert_eq!(
            flavor_of("Foo-mainline.zip"),
            Flavor::Unlabeled,
            "_Mainline TOCs may list Forever"
        );
        assert_eq!(flavor_of("Foo-Camelot.zip"), Flavor::Forever);
        assert_eq!(flavor_of("ForeverAuras-1.0.zip"), Flavor::Forever);
    }
}
