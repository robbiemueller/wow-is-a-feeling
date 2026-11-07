//! The hand-maintained files, plus the two kinds of entry that need them: source
//! installs for addons that publish tags but no packaged zips, and direct entries
//! with their own manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use feeling_core::catalog::{Addon, Kit, Move, Release, Source};
use feeling_core::toc::TOC_SUFFIXES;
use serde::{Deserialize, Serialize};

use crate::detect::{Flavor, flavor_of, tag_is_prerelease};
use crate::github::{self, RepoInfo};
use crate::http::{Channel, Http, unix_now};
use crate::model::{Candidate, ZipScan};
use crate::rangezip::{HttpRange, RangeReader};
use crate::text::{
    clean_author, clean_name, clean_summary, clean_version, encode_path, round_sig,
    says_nothing_new,
};
use crate::zipscan::{parse_toc, scan_zip};

/// `catalog/seeds.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seeds {
    /// GitHub repositories that are always checked, first.
    #[serde(default)]
    pub repos: Vec<String>,
    /// WoWInterface ids that are always checked, first.
    #[serde(default)]
    pub wowi: Vec<u64>,
}

/// `catalog/curated.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Curated {
    #[serde(default)]
    pub featured: Vec<String>,
    #[serde(default)]
    pub kits: Vec<Kit>,
    #[serde(default)]
    pub gaps: Vec<GapSpec>,
    #[serde(default)]
    pub source: Vec<SourceSpec>,
    #[serde(default)]
    pub direct: Vec<DirectSpec>,
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
    #[serde(default)]
    pub exclude: Exclude,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapSpec {
    pub name: String,
    pub reason: String,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub alternatives: Vec<String>,
    /// Catalog ids that share this name but are different projects, so finding
    /// them doesn't mean the gap has closed.
    #[serde(default)]
    pub unrelated: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub repo: String,
    /// Folders to install, main first. Without this they come from `.pkgmeta`.
    #[serde(default)]
    pub folders: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectSpec {
    /// `direct:name`.
    pub id: String,
    /// JSON with `version`, `url`, `sha256` and `bytes` under `key`.
    pub manifest: String,
    pub key: String,
    /// Base for a relative `url`.
    pub site: String,
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    /// Added to the popularity score, which is roughly orders of magnitude of downloads.
    #[serde(default)]
    pub boost: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    /// Accounts that only mirror other people's addons.
    #[serde(default)]
    pub owners: Vec<String>,
    /// Specific repositories that are junk.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Catalog ids to leave out, whatever their source.
    #[serde(default)]
    pub ids: Vec<String>,
}

impl Curated {
    /// Repositories someone chose by hand. These survive being archived.
    pub fn named_repos(&self) -> BTreeSet<String> {
        let mentioned = self
            .featured
            .iter()
            .chain(self.kits.iter().flat_map(|kit| kit.addons.iter()))
            .chain(self.overrides.keys());
        mentioned
            .filter_map(|id| id.strip_prefix("gh:"))
            .map(str::to_ascii_lowercase)
            .chain(self.source.iter().map(|s| s.repo.to_ascii_lowercase()))
            .collect()
    }

    pub fn excludes_repo(&self, repo: &str) -> bool {
        let owner = repo.split('/').next().unwrap_or(repo);
        self.exclude
            .owners
            .iter()
            .any(|o| o.eq_ignore_ascii_case(owner))
            || self
                .exclude
                .repos
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo))
    }
}

pub fn load_seeds(path: &Path) -> Result<Seeds, String> {
    let text = read_config(path)?;
    toml::from_str(&text).map_err(|e| format!("{} has a mistake in it: {e}", path.display()))
}

pub fn load_curated(path: &Path) -> Result<Curated, String> {
    let text = read_config(path)?;
    toml::from_str(&text).map_err(|e| format!("{} has a mistake in it: {e}", path.display()))
}

fn read_config(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))
}

/// The parts of a `.pkgmeta` that decide how a source archive installs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PkgMeta {
    pub package_as: Option<String>,
    pub externals: Vec<String>,
    pub move_folders: Vec<(String, String)>,
}

/// Reads the small part of YAML that `.pkgmeta` files use. Unknown keys are ignored.
pub fn parse_pkgmeta(text: &str) -> PkgMeta {
    let mut meta = PkgMeta::default();
    let mut section = String::new();
    let mut child_indent = None;
    for raw in text.lines() {
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let (key, value) = split_entry(line.trim());
        if indent == 0 {
            section = key.to_ascii_lowercase();
            child_indent = None;
            if section == "package-as" && !value.is_empty() {
                meta.package_as = Some(value);
            }
            continue;
        }
        // Deeper lines belong to the entry above, like the url of a mapped external.
        if *child_indent.get_or_insert(indent) != indent || key.is_empty() {
            continue;
        }
        match section.as_str() {
            "externals" => meta.externals.push(key),
            "move-folders" if !value.is_empty() => meta.move_folders.push((key, value)),
            _ => {}
        }
    }
    meta
}

fn strip_comment(line: &str) -> &str {
    if line.trim_start().starts_with('#') {
        return "";
    }
    match line.find(" #") {
        Some(at) => &line[..at],
        None => line,
    }
}

fn split_entry(line: &str) -> (String, String) {
    let line = line.strip_prefix("- ").unwrap_or(line);
    let colon = line.match_indices(':').find(|(at, _)| {
        line[at + 1..].is_empty() || line[at + 1..].starts_with(char::is_whitespace)
    });
    match colon {
        Some((at, _)) => (unquote(&line[..at]), unquote(&line[at + 1..])),
        None => (String::new(), String::new()),
    }
}

fn unquote(text: &str) -> String {
    text.trim().trim_matches(['"', '\'']).trim().to_string()
}

/// How a source archive installs, once checked against the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePlan {
    pub moves: Vec<Move>,
    pub scan: ZipScan,
}

/// Why an addon can't be offered as a source install.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `.pkgmeta` pulls in libraries from elsewhere, and the archive doesn't have them.
    Externals(usize),
    /// Git submodules aren't part of a source archive either.
    Submodules,
    /// No folder has a TOC we could find.
    NoToc,
    /// The TOCs exist but none targets Forever.
    NotForever,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Externals(count) => write!(
                f,
                ".pkgmeta lists {count} externals, so a source archive would be missing libraries"
            ),
            Refusal::Submodules => write!(
                f,
                "the repository uses git submodules, which a source archive doesn't include"
            ),
            Refusal::NoToc => write!(f, "no folder has a TOC the client could load"),
            Refusal::NotForever => write!(f, "no TOC targets Forever"),
        }
    }
}

/// Work out the moves and folders for a source archive. `read_file` fetches a
/// repository-relative path at the tag, giving `Ok(None)` when it doesn't exist.
pub fn plan_source_install(
    spec: &SourceSpec,
    repo_name: &str,
    meta: &PkgMeta,
    read_file: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
) -> Result<Result<SourcePlan, Refusal>, String> {
    if !meta.externals.is_empty() {
        return Ok(Err(Refusal::Externals(meta.externals.len())));
    }
    if read_file(".gitmodules")?.is_some_and(|text| text.contains("path")) {
        return Ok(Err(Refusal::Submodules));
    }
    let package = meta.package_as.as_deref().unwrap_or(repo_name);
    let main = spec.folders.first().map_or(package, String::as_str);

    // Each folder paired with where it lives in the repository. `move-folders`
    // paths start with the package name, which stands for the repository root.
    let mut layout: Vec<(String, String)> = meta
        .move_folders
        .iter()
        .map(|(from, to)| {
            let prefix = format!("{package}/");
            (
                to.clone(),
                from.strip_prefix(prefix.as_str())
                    .unwrap_or(from)
                    .to_string(),
            )
        })
        .collect();
    for folder in &spec.folders {
        if !layout.iter().any(|(name, _)| name == folder) {
            layout.push((folder.clone(), folder.clone()));
        }
    }
    if !layout.iter().any(|(name, _)| name == main) {
        layout.push((main.to_string(), String::new()));
    }

    let mut toc_texts: Vec<(String, String, String)> = Vec::new();
    for (folder, path) in layout {
        if let Some(text) = find_toc(&path, &folder, read_file)? {
            toc_texts.push((folder, path, text));
        }
    }
    // The main folder's own subfolder is a common layout even without move-folders.
    if !toc_texts.iter().any(|(folder, _, _)| folder == main)
        && let Some(text) = find_toc(main, main, read_file)?
    {
        toc_texts.push((main.to_string(), main.to_string(), text));
    }
    if !toc_texts.iter().any(|(folder, _, _)| folder == main) {
        return Ok(Err(Refusal::NoToc));
    }

    let forever = toc_texts
        .iter()
        .any(|(_, _, text)| parse_toc(text).targets_forever);
    if !forever {
        return Ok(Err(Refusal::NotForever));
    }
    toc_texts.sort_by_key(|(folder, _, _)| (folder != main, folder.clone()));

    let moves = toc_texts
        .iter()
        .map(|(folder, path, _)| Move {
            from: if path.is_empty() {
                "*/".to_string()
            } else {
                format!("*/{path}")
            },
            to: folder.clone(),
        })
        .collect();
    let main_toc = parse_toc(&toc_texts[0].2).toc;
    let scan = ZipScan {
        folders: toc_texts
            .iter()
            .map(|(folder, _, _)| folder.clone())
            .collect(),
        forever,
        title: main_toc.title,
        notes: main_toc.notes,
        author: main_toc.author,
        deps: main_toc.deps,
        category: main_toc.category,
        curse: main_toc.curse_id,
        wowi: main_toc.wowi_id,
        wago: main_toc.wago_id,
    };
    Ok(Ok(SourcePlan { moves, scan }))
}

/// Try the client's TOC names in order, in `dir` relative to the repository root.
fn find_toc(
    dir: &str,
    folder: &str,
    read_file: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
) -> Result<Option<String>, String> {
    for suffix in TOC_SUFFIXES {
        let path = if dir.is_empty() {
            format!("{folder}{suffix}.toc")
        } else {
            format!("{dir}/{folder}{suffix}.toc")
        };
        if let Some(text) = read_file(&path)? {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

/// A file in a repository at a tag.
fn raw_url(repo: &str, tag: &str, path: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{repo}/refs/tags/{}/{}",
        encode_path(tag),
        encode_path(path)
    )
}

/// The source archive GitHub builds for a tag.
pub fn archive_url(repo: &str, tag: &str) -> String {
    format!(
        "https://github.com/{repo}/archive/refs/tags/{}.zip",
        encode_path(tag)
    )
}

fn read_if_present(http: &Http, url: &str) -> Result<Option<String>, String> {
    match http.get(Channel::Raw, url) {
        Ok(reply) => Ok(Some(String::from_utf8_lossy(&reply.body).into_owned())),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Result of the newest tag check, kept so an unchanged tag costs no raw requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceState {
    pub newest_tag: String,
    /// The tag actually offered. Older than `newest_tag` when that one lacks Forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offered: Option<SourceBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// The last good result. Used when GitHub can't be reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Candidate>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceBuild {
    pub tag: String,
    pub date: i64,
    pub plan: SourcePlan,
}

const MAX_TAGS_TRIED: usize = 5;

/// Find what to offer for a curated source repository. An `Err` keeps the last-known result.
pub fn process_source(
    http: &Http,
    spec: &SourceSpec,
    state: &mut SourceState,
) -> Result<(RepoInfo, Option<SourceBuild>), String> {
    let (info, tags) = github::fetch_tags(http, &spec.repo, 20)?;
    let repo_name = info
        .name
        .split('/')
        .nth(1)
        .unwrap_or(&info.name)
        .to_string();
    let usable: Vec<_> = tags
        .iter()
        .filter(|t| flavor_of(&t.name) != Flavor::Other && !tag_is_prerelease(&t.name))
        .collect();
    let Some(newest) = usable.first() else {
        return Err("no usable tags found".to_string());
    };
    if state.newest_tag == newest.name {
        return Ok((info, state.offered.clone()));
    }

    let mut offered = None;
    let mut refused = None;
    for tag in usable.iter().take(MAX_TAGS_TRIED) {
        let mut read = |path: &str| read_if_present(http, &raw_url(&info.name, &tag.name, path));
        let meta = read(".pkgmeta")?
            .as_deref()
            .map(parse_pkgmeta)
            .unwrap_or_default();
        match plan_source_install(spec, &repo_name, &meta, &mut read)? {
            Ok(plan) => {
                offered = Some(SourceBuild {
                    tag: tag.name.clone(),
                    date: tag.date,
                    plan,
                });
                break;
            }
            // The same recipe applies to older tags, so looking further won't help.
            Err(refusal @ (Refusal::Externals(_) | Refusal::Submodules)) => {
                refused = Some(refusal.to_string());
                break;
            }
            Err(refusal) => refused = Some(format!("{}: {refusal}", tag.name)),
        }
    }
    *state = SourceState {
        newest_tag: newest.name.clone(),
        offered: offered.clone(),
        refused: if offered.is_some() { None } else { refused },
        entry: None,
    };
    Ok((info, offered))
}

/// Turn a checked source install into a catalog entry.
pub fn source_candidate(info: &RepoInfo, build: &SourceBuild) -> Candidate {
    let repo_name = info.name.split('/').nth(1).unwrap_or(&info.name);
    let scan = &build.plan.scan;
    let main = scan.folders.first().map_or(repo_name, String::as_str);
    let mut name = clean_name(scan.title.as_deref().unwrap_or_default());
    if name.is_empty() {
        name = main.to_string();
    }
    let mut summary = clean_summary(scan.notes.as_deref().unwrap_or_default());
    if says_nothing_new(&summary, &name) {
        summary = clean_summary(info.description.as_deref().unwrap_or_default());
    }
    let mut author = clean_author(scan.author.as_deref().unwrap_or_default());
    if author.is_empty() {
        author = info.name.split('/').next().unwrap_or_default().to_string();
    }
    Candidate {
        addon: Addon {
            id: format!("gh:{}", info.name),
            name,
            summary,
            author,
            source: Source::Github {
                repo: info.name.clone(),
            },
            website: Some(format!("https://github.com/{}", info.name)),
            stable: Some(Release {
                version: clean_version(&build.tag, &[main, repo_name]),
                url: archive_url(&info.name, &build.tag),
                date: build.date,
                moves: build.plan.moves.clone(),
                ..Release::default()
            }),
            folders: scan.folders.clone(),
            deps: scan.external_deps(),
            stars: u32::try_from(round_sig(u64::from(info.stars), 3)).unwrap_or(info.stars),
            ids: scan.external_ids(),
            ..Addon::default()
        },
        toc_category: scan.category.clone(),
        topics: info.topics.clone(),
        ..Candidate::default()
    }
}

/// What a direct entry's manifest said last time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DirectState {
    pub version: String,
    pub first_seen: i64,
    pub scan: ZipScan,
    /// The last good result. Used when the manifest can't be fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Candidate>,
}

#[derive(Debug, PartialEq, Eq)]
struct ManifestEntry {
    version: String,
    url: String,
    sha256: Option<String>,
    bytes: u64,
}

fn read_manifest(body: &[u8], key: &str, site: &str) -> Result<ManifestEntry, String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("the manifest doesn't parse: {e}"))?;
    let entry = manifest
        .get(key)
        .ok_or_else(|| format!("the manifest has no `{key}` entry"))?;
    let text = |field: &str| {
        entry
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let version = text("version")
        .filter(|v| !v.trim().is_empty())
        .ok_or("the manifest has no version")?;
    let url = text("url")
        .filter(|u| !u.trim().is_empty())
        .ok_or("the manifest has no url")?;
    let url = if url.starts_with("https://") {
        url
    } else if url.contains("://") {
        return Err(format!("the manifest's url isn't https: {url}"));
    } else {
        format!(
            "{}/{}",
            site.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    };
    let sha256 = text("sha256")
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    let bytes = entry
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Ok(ManifestEntry {
        version,
        url,
        sha256,
        bytes,
    })
}

/// Fetch a direct entry. An `Err` keeps the last-known result.
pub fn process_direct(
    http: &Http,
    spec: &DirectSpec,
    state: &mut Option<DirectState>,
) -> Result<Candidate, String> {
    let reply = http
        .get(Channel::Direct, &spec.manifest)
        .map_err(|e| e.to_string())?;
    let entry = read_manifest(&reply.body, &spec.key, &spec.site)?;
    let cached = state.as_ref().filter(|s| s.version == entry.version);
    let current = match cached {
        Some(hit) => hit.clone(),
        None => {
            let size = match entry.bytes {
                0 => HttpRange::probe_size(http, Channel::Direct, &entry.url)
                    .map_err(|e| e.to_string())?,
                known => known,
            };
            let source = HttpRange::new(http, Channel::Direct, &entry.url, Some(size));
            let scan = scan_zip(RangeReader::new(source, size), &[&spec.name])
                .map_err(|e| e.to_string())?;
            DirectState {
                version: entry.version.clone(),
                first_seen: unix_now(),
                scan,
                entry: None,
            }
        }
    };
    if !current.scan.forever || current.scan.folders.is_empty() {
        return Err(format!("version {} doesn't target Forever", entry.version));
    }
    let candidate = direct_candidate(spec, &entry, &current);
    *state = Some(DirectState {
        entry: Some(candidate.clone()),
        ..current
    });
    Ok(candidate)
}

fn direct_candidate(spec: &DirectSpec, entry: &ManifestEntry, state: &DirectState) -> Candidate {
    let scan = &state.scan;
    let website = spec.website.clone().or_else(|| Some(spec.site.clone()));
    let summary = spec
        .summary
        .as_deref()
        .or(scan.notes.as_deref())
        .unwrap_or_default();
    let author = spec
        .author
        .as_deref()
        .or(scan.author.as_deref())
        .unwrap_or_default();
    Candidate {
        addon: Addon {
            id: spec.id.clone(),
            name: spec.name.clone(),
            summary: clean_summary(summary),
            author: clean_author(author),
            source: Source::Direct {
                url: spec.site.clone(),
            },
            website,
            stable: Some(Release {
                version: entry.version.clone(),
                url: entry.url.clone(),
                size: entry.bytes,
                date: state.first_seen,
                sha256: entry.sha256.clone(),
                ..Release::default()
            }),
            folders: scan.folders.clone(),
            deps: scan.external_deps(),
            ids: scan.external_ids(),
            ..Addon::default()
        },
        toc_category: scan.category.clone(),
        ..Candidate::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::http::testing::{client, reply, serve};

    const ELVUI_META: &str = "package-as: ElvUI\n\nexternals:\n  ElvUI_Libraries/Game/Shared/LibStub: https://repos.curseforge.com/wow/libstub/trunk\n  ElvUI_Libraries/Game/Shared/TaintLess:\n    url: https://www.townlong-yak.com/addons.git/taintless\n    commit: default\n\nignore:\n  - \"*.md\"\n  - Makefile\n\nmove-folders:\n  ElvUI/ElvUI: ElvUI\n  ElvUI/ElvUI_Libraries: ElvUI_Libraries\n";
    const DETAILS_META: &str = "package-as: Details\r\n\r\nmove-folders:\r\n    Details/plugins/Details_TinyThreat: Details_TinyThreat\r\n    Details/plugins/Details_Vanguard: Details_Vanguard # a note\r\n";

    #[test]
    fn pkgmeta_reads_the_real_shapes() {
        let elvui = parse_pkgmeta(ELVUI_META);
        assert_eq!(elvui.package_as.as_deref(), Some("ElvUI"));
        assert_eq!(
            elvui.externals.len(),
            2,
            "nested url and commit lines don't count: {:?}",
            elvui.externals
        );
        assert_eq!(
            elvui.move_folders[0],
            ("ElvUI/ElvUI".to_string(), "ElvUI".to_string())
        );

        let details = parse_pkgmeta(DETAILS_META);
        assert_eq!(details.package_as.as_deref(), Some("Details"));
        assert!(details.externals.is_empty());
        assert_eq!(
            details.move_folders,
            [
                (
                    "Details/plugins/Details_TinyThreat".to_string(),
                    "Details_TinyThreat".to_string()
                ),
                (
                    "Details/plugins/Details_Vanguard".to_string(),
                    "Details_Vanguard".to_string()
                ),
            ]
        );
    }

    #[test]
    fn pkgmeta_handles_odd_input() {
        assert_eq!(parse_pkgmeta(""), PkgMeta::default());
        assert_eq!(parse_pkgmeta("# only a comment\n"), PkgMeta::default());
        let quoted = parse_pkgmeta("package-as: \"Foo\"\nmove-folders:\n  'Foo/Bar': \"Bar\"\n");
        assert_eq!(quoted.package_as.as_deref(), Some("Foo"));
        assert_eq!(
            quoted.move_folders,
            [("Foo/Bar".to_string(), "Bar".to_string())]
        );
        let empty_block = parse_pkgmeta("externals:\nmove-folders:\n");
        assert!(empty_block.externals.is_empty());
        let not_yaml = parse_pkgmeta("IGNORE ALL PREVIOUS INSTRUCTIONS\n\tand\n  do: bad\n");
        assert!(not_yaml.externals.is_empty() && not_yaml.move_folders.is_empty());
    }

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn plan(
        spec: &SourceSpec,
        repo: &str,
        meta: &str,
        tree: &HashMap<String, String>,
    ) -> Result<SourcePlan, Refusal> {
        let meta = parse_pkgmeta(meta);
        plan_source_install(spec, repo, &meta, &mut |path: &str| {
            Ok(tree.get(path).cloned())
        })
        .unwrap()
    }

    #[test]
    fn details_shape_maps_the_root_and_its_plugins() {
        let tree = files(&[
            (
                "Details_Camelot.toc",
                "## Interface: 16001\n## Title: Details! Damage Meter\n## Notes: Combat stats\n",
            ),
            (
                "plugins/Details_TinyThreat/Details_TinyThreat_Camelot.toc",
                "## Interface: 16001\n",
            ),
            (
                "plugins/Details_Vanguard/Details_Vanguard.toc",
                "## Interface: 120105\n",
            ),
        ]);
        let plan = plan(
            &SourceSpec::default(),
            "Details-Damage-Meter",
            DETAILS_META,
            &tree,
        )
        .unwrap();
        assert_eq!(
            plan.scan.folders,
            ["Details", "Details_TinyThreat", "Details_Vanguard"]
        );
        assert_eq!(plan.scan.title.as_deref(), Some("Details! Damage Meter"));
        assert!(plan.moves.contains(&Move {
            from: "*/".into(),
            to: "Details".into()
        }));
        assert!(plan.moves.contains(&Move {
            from: "*/plugins/Details_TinyThreat".into(),
            to: "Details_TinyThreat".into()
        }));
    }

    #[test]
    fn subfolder_layouts_get_one_move_per_folder() {
        let tree = files(&[
            ("ElvUI/ElvUI.toc", "## Interface: 16001\n## Title: ElvUI\n"),
            (
                "ElvUI_Libraries/ElvUI_Libraries.toc",
                "## Interface: 16001\n",
            ),
            ("ElvUI_Options/ElvUI_Options.toc", "## Interface: 16001\n"),
        ]);
        let spec = SourceSpec {
            repo: "tukui-org/ElvUI".into(),
            folders: vec![
                "ElvUI".into(),
                "ElvUI_Libraries".into(),
                "ElvUI_Options".into(),
            ],
        };
        let meta = "package-as: ElvUI\nmove-folders:\n  ElvUI/ElvUI: ElvUI\n  ElvUI/ElvUI_Libraries: ElvUI_Libraries\n  ElvUI/ElvUI_Options: ElvUI_Options\n";
        let plan = plan(&spec, "ElvUI", meta, &tree).unwrap();
        assert_eq!(
            plan.scan.folders,
            ["ElvUI", "ElvUI_Libraries", "ElvUI_Options"]
        );
        assert!(plan.moves.contains(&Move {
            from: "*/ElvUI_Options".into(),
            to: "ElvUI_Options".into()
        }));
        assert!(
            !plan.moves.iter().any(|m| m.from == "*/"),
            "the repo root isn't an addon here"
        );
    }

    #[test]
    fn no_pkgmeta_with_a_toc_at_the_root_uses_the_repo_name() {
        let tree = files(&[("GnomeLevelUp.toc", "## Interface: 16001\n## Title: Gnome\n")]);
        let plan = plan(&SourceSpec::default(), "GnomeLevelUp", "", &tree).unwrap();
        assert_eq!(plan.scan.folders, ["GnomeLevelUp"]);
        assert_eq!(
            plan.moves,
            [Move {
                from: "*/".into(),
                to: "GnomeLevelUp".into()
            }]
        );
    }

    #[test]
    fn no_pkgmeta_with_the_addon_in_a_subfolder_works_too() {
        let tree = files(&[("Foo/Foo.toc", "## Interface: 16001\n")]);
        let plan = plan(&SourceSpec::default(), "Foo", "", &tree).unwrap();
        assert_eq!(
            plan.moves,
            [Move {
                from: "*/Foo".into(),
                to: "Foo".into()
            }]
        );
    }

    #[test]
    fn externals_refuse_the_install_before_any_toc_is_read() {
        let meta = parse_pkgmeta(ELVUI_META);
        let mut reads = 0;
        let result = plan_source_install(&SourceSpec::default(), "ElvUI", &meta, &mut |_: &str| {
            reads += 1;
            Ok(None)
        });
        assert_eq!(result.unwrap().unwrap_err(), Refusal::Externals(2));
        assert_eq!(reads, 0);
    }

    #[test]
    fn git_submodules_refuse_the_install() {
        let tree = files(&[
            ("Foo.toc", "## Interface: 16001\n"),
            (
                ".gitmodules",
                "[submodule \"Libs/Ace3\"]\n\tpath = Libs/Ace3\n\turl = https://x.test/ace3\n",
            ),
        ]);
        assert_eq!(
            plan(&SourceSpec::default(), "Foo", "", &tree).unwrap_err(),
            Refusal::Submodules
        );
    }

    #[test]
    fn missing_or_non_forever_tocs_are_refused_and_folders_without_tocs_are_dropped() {
        let empty = HashMap::new();
        assert_eq!(
            plan(&SourceSpec::default(), "Foo", "", &empty).unwrap_err(),
            Refusal::NoToc
        );

        let retail = files(&[("Foo.toc", "## Interface: 120105\n")]);
        assert_eq!(
            plan(&SourceSpec::default(), "Foo", "", &retail).unwrap_err(),
            Refusal::NotForever
        );

        let tree = files(&[("Foo.toc", "## Interface: 16001\n")]);
        let meta = "package-as: Foo\nmove-folders:\n  Foo/plugins/Gone: Gone\n";
        let plan = plan(&SourceSpec::default(), "Foo", meta, &tree).unwrap();
        assert_eq!(plan.scan.folders, ["Foo"]);
    }

    #[test]
    fn a_network_failure_is_an_error_not_a_refusal() {
        let meta = PkgMeta::default();
        let result = plan_source_install(&SourceSpec::default(), "Foo", &meta, &mut |_: &str| {
            Err("HTTP 503".to_string())
        });
        assert!(result.is_err());
    }

    #[test]
    fn urls_encode_tags() {
        assert_eq!(
            archive_url("Tercioo/Details-Damage-Meter", "Details.20261104.15400.172"),
            "https://github.com/Tercioo/Details-Damage-Meter/archive/refs/tags/Details.20261104.15400.172.zip"
        );
        assert_eq!(
            archive_url("a/b", "bundle/v1+v2"),
            "https://github.com/a/b/archive/refs/tags/bundle/v1%2Bv2.zip"
        );
        assert_eq!(
            raw_url("a/b", "v1+v2", "plugins/Foo Bar.toc"),
            "https://raw.githubusercontent.com/a/b/refs/tags/v1%2Bv2/plugins/Foo%20Bar.toc"
        );
    }

    #[test]
    fn source_candidates_are_complete() {
        let tree = files(&[(
            "Details_Camelot.toc",
            "## Interface: 16001\n## Title: |cffffaa00Details!|r\n## Dependencies: Ace3\n",
        )]);
        let plan = plan(
            &SourceSpec::default(),
            "Details-Damage-Meter",
            DETAILS_META,
            &tree,
        )
        .unwrap();
        let info = RepoInfo {
            name: "Tercioo/Details-Damage-Meter".into(),
            description: Some("Combat analysis".into()),
            stars: 227,
            ..RepoInfo::default()
        };
        let build = SourceBuild {
            tag: "Details.20261104.15400.172".into(),
            date: 99,
            plan,
        };
        let c = source_candidate(&info, &build);
        assert_eq!(c.addon.id, "gh:Tercioo/Details-Damage-Meter");
        assert_eq!(c.addon.name, "Details!");
        assert_eq!(c.addon.summary, "Combat analysis");
        assert_eq!(c.addon.deps, ["Ace3"]);
        let release = c.addon.stable.unwrap();
        assert_eq!(release.version, "20261104.15400.172");
        assert_eq!(release.date, 99);
        assert_eq!(release.size, 0);
        assert!(!release.moves.is_empty());
    }

    const MANIFEST: &str = r#"{"addon":{"version":"0.57.0","url":"downloads/ForeverDB.zip?v=5a0e","sha256":"5A0E2352914AFB2B6C3429BF317D5A931ACC38B95D1E817C1482C5051740B393","bytes":5558921},
        "helper":{"version":"2.12.0","url":"downloads/ForeverDB-Helper.exe?v=6d3e","sha256":"x","bytes":1},
        "site":"https://foreverdb.net"}"#;

    #[test]
    fn manifests_resolve_relative_urls_and_validate_fields() {
        let entry = read_manifest(MANIFEST.as_bytes(), "addon", "https://foreverdb.net").unwrap();
        assert_eq!(
            entry.url,
            "https://foreverdb.net/downloads/ForeverDB.zip?v=5a0e"
        );
        assert_eq!(entry.version, "0.57.0");
        assert_eq!(entry.bytes, 5_558_921);
        assert_eq!(
            entry.sha256.as_deref(),
            Some("5a0e2352914afb2b6c3429bf317d5a931acc38b95d1e817c1482c5051740b393")
        );

        let bad_hash =
            read_manifest(MANIFEST.as_bytes(), "helper", "https://foreverdb.net").unwrap();
        assert_eq!(bad_hash.sha256, None);

        assert!(read_manifest(MANIFEST.as_bytes(), "nope", "https://x.test").is_err());
        assert!(read_manifest(b"<html>", "addon", "https://x.test").is_err());
        assert!(read_manifest(br#"{"addon":{"url":"a.zip"}}"#, "addon", "https://x.test").is_err());
        let http_url = br#"{"addon":{"version":"1","url":"http://x.test/a.zip"}}"#;
        assert!(read_manifest(http_url, "addon", "https://x.test").is_err());
    }

    #[test]
    fn the_real_config_files_parse_and_are_consistent() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog");
        let seeds = load_seeds(&root.join("seeds.toml")).unwrap();
        assert!(seeds.repos.len() >= 25);
        assert!(
            seeds.repos.iter().all(|r| crate::text::valid_repo_name(r)),
            "bad repo name in seeds"
        );
        assert!(seeds.wowi.contains(&7032) && seeds.wowi.contains(&8092));
        let mut sorted = seeds.repos.clone();
        sorted.sort_by_key(|r| r.to_ascii_lowercase());
        sorted.dedup_by_key(|r| r.to_ascii_lowercase());
        assert_eq!(sorted.len(), seeds.repos.len(), "duplicate repo in seeds");

        let curated = load_curated(&root.join("curated.toml")).unwrap();
        assert!(curated.featured.len() >= 10);
        for kit in &curated.kits {
            assert!(
                (4..=8).contains(&kit.addons.len()),
                "{} has {} addons",
                kit.name,
                kit.addons.len()
            );
        }
        let names: Vec<&str> = curated.kits.iter().map(|k| k.name.as_str()).collect();
        assert!(names.contains(&"New to Forever") && names.contains(&"Raid night"));
        for wanted in [
            "Leatrix Plus",
            "Leatrix Maps",
            "Bagnon",
            "Baganator",
            "Auctionator",
            "Platynator",
            "AtlasLoot",
            "DungeonJournal",
            "RareScanner",
            "BugGrabber",
            "ForeverPlus",
            "ForeverUI",
        ] {
            assert!(
                curated.gaps.iter().any(|g| g.name == wanted),
                "{wanted} is missing from gaps"
            );
        }
        for gap in &curated.gaps {
            assert!(
                gap.reason.ends_with('.'),
                "{}: the reason should be a sentence",
                gap.name
            );
            assert_eq!(
                gap.reason.matches(". ").count(),
                0,
                "{}: one sentence only",
                gap.name
            );
            assert!(
                !gap.alternatives.is_empty(),
                "{} needs alternatives",
                gap.name
            );
        }
        for id in curated
            .featured
            .iter()
            .chain(curated.kits.iter().flat_map(|k| k.addons.iter()))
        {
            assert!(
                id.starts_with("gh:") || id.starts_with("wowi:") || id.starts_with("direct:"),
                "{id} isn't a catalog id"
            );
        }
        assert!(curated.excludes_repo("HIPPULI/anything"));
        let known: Vec<&str> = feeling_core::catalog::CATEGORIES
            .iter()
            .map(|(id, _)| *id)
            .collect();
        for (id, entry) in &curated.overrides {
            if let Some(category) = &entry.category {
                assert!(
                    known.contains(&category.as_str()),
                    "{id}: unknown category {category}"
                );
            }
        }
    }

    #[test]
    fn config_mistakes_are_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("curated.toml");
        std::fs::write(&path, "feature = [\"typo\"]\n").unwrap();
        let error = load_curated(&path).unwrap_err();
        assert!(error.contains("mistake"), "{error}");
        assert!(load_seeds(&dir.path().join("missing.toml")).is_err());
    }

    #[test]
    fn repo_files_treat_404_as_absent_and_other_failures_as_errors() {
        let (base, _) = serve(vec![
            reply(200, &[], b"text"),
            reply(404, &[], b""),
            reply(403, &[], b""),
        ]);
        let http = client(None);
        let url = format!("{base}/file");
        assert_eq!(read_if_present(&http, &url), Ok(Some("text".to_string())));
        assert_eq!(read_if_present(&http, &url), Ok(None));
        assert!(read_if_present(&http, &url).is_err());
    }
}
