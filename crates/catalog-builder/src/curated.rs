//! The hand-maintained files, plus the two kinds of entry that need them: source
//! installs for addons that publish tags but no packaged zips, and direct entries
//! with their own manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use feeling_core::catalog::{Addon, Kit, Move, Release, Source};
use feeling_core::toc::TOC_SUFFIXES;
use serde::{Deserialize, Serialize};

use crate::detect::{Flavor, flavor_of, tag_is_prerelease};
use crate::gate::is_safe_folder;
use crate::github::{self, RepoInfo};
use crate::http::{Channel, Http, unix_now};
use crate::model::{Candidate, MAX_FORMERLY, ZipScan};
use crate::rangezip::{HttpRange, RangeReader};
use crate::text::{
    clean_author, clean_name, clean_summary, clean_version, encode_path, round_sig,
    says_nothing_new, valid_repo_name,
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
    /// Folders of seeded repositories with no entry yet. Only something hand-picked may
    /// claim these, or a name that reads the same.
    #[serde(default)]
    pub reserved: Vec<String>,
    /// WoWInterface listing id to the repository it's approved to merge into. Nothing
    /// else merges a listing, whatever a repository's own files claim.
    #[serde(default)]
    pub merges: BTreeMap<String, String>,
    /// Which addon an entry is and what it was, by id, written down where the last catalog
    /// can't be trusted to say: a rename, another addon of the repository already in its place,
    /// or a former main folder the catalog lost or never vouched for.
    #[serde(default)]
    pub pins: BTreeMap<String, PinSpec>,
    /// Repositories that publish several addons, each under its own tag prefix.
    #[serde(default)]
    pub split: Vec<SplitSpec>,
    /// Addons that go with another, such as the voice packs it plays, as groups.
    #[serde(default)]
    pub packs: Vec<PackSpec>,
}

/// One repository that publishes several addons, each under its own tag prefix, as tags
/// like `<prefix>/v1.2.3`. See "One product per entry" in `docs/DESIGN.md`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitSpec {
    /// `owner/repo`. It's seeded too, so it's always read.
    pub repo: String,
    /// The tag prefix of the repository's own addon, the one `gh:owner/repo` lists.
    pub main: String,
    /// Tag prefixes that are addons of their own, each listed as `gh:owner/repo/<line>`.
    #[serde(default)]
    pub lines: Vec<String>,
}

/// One group of packs for an addon: any one of `ids` meets it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackSpec {
    /// The addon the packs go with.
    pub addon: String,
    /// What the group is for, such as "Quest voices".
    pub name: String,
    /// The addon does nothing useful until one of these is installed.
    #[serde(default)]
    pub needed: bool,
    /// Catalog ids, in the order to offer them.
    pub ids: Vec<String>,
    /// Folder name prefixes that meet the group too, for packs the catalog doesn't list:
    /// other languages, or ones from another tool.
    #[serde(default)]
    pub met_by: Vec<String>,
}

/// Which addon an entry is and what it was, as `[pins."<id>"]`. See "One product per entry" in
/// `docs/DESIGN.md`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinSpec {
    /// The folder that identifies the entry's addon. A new build only counts if it ships it.
    /// Only a repository's own GitHub entry outside `[[split]]` takes one: every other source
    /// already says which addon an entry is.
    #[serde(default)]
    pub main: Option<String>,
    /// Main folders the addon really had before. Copies installed under one of them still
    /// get its updates, because they go into its `formerly`. Any entry takes these.
    #[serde(default)]
    pub was: Vec<String>,
}

/// More lines than any repository needs, since each costs requests on every full look.
const MAX_LINES: usize = 32;
/// A line's prefix ends up in its id, which shared addon lists carry.
const MAX_LINE_BYTES: usize = 64;
/// A pack group's name is a label on the addon's page.
const MAX_PACK_GROUP_CHARS: usize = 40;
/// Prefixes per pack group. A few cover every language and every tool there is, and the app
/// checks each against every folder in AddOns.
const MAX_MET_BY: usize = 8;

impl SplitSpec {
    /// The catalog id of a release line: the repository as GitHub spells it, and the
    /// prefix in lower case, so the author changing a tag's case can't orphan installs.
    pub fn line_id(repo: &str, line: &str) -> String {
        format!("gh:{repo}/{}", line.to_ascii_lowercase())
    }

    /// Every tag prefix this repository is read under, its own addon's first.
    pub fn prefixes(&self) -> Vec<&str> {
        std::iter::once(self.main.as_str())
            .chain(self.lines.iter().map(String::as_str))
            .collect()
    }
}

/// The plain repository of a GitHub catalog id: `owner/repo` for `gh:owner/repo`, and for a
/// release line's `gh:owner/repo/<line>` too.
pub fn repo_of_id(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("gh:")?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(&rest[..owner.len() + 1 + repo.len()])
}

/// A tag prefix a repository's releases can be read under: `[A-Za-z0-9._-]`, so it can only
/// ever be one whole segment of a tag, and never a dot first.
fn is_tag_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 100
        && !prefix.starts_with('.')
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A prefix that can be the last part of a catalog id: letters, digits and dashes only,
/// which is all the app accepts there.
fn is_line_prefix(prefix: &str) -> bool {
    prefix.len() <= MAX_LINE_BYTES
        && !prefix.starts_with('-')
        && !prefix.ends_with('-')
        && is_tag_prefix(prefix)
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `gh:owner/repo`, `gh:owner/repo/<line>`, `wowi:1234` or `direct:name`.
fn looks_like_id(id: &str) -> bool {
    ["gh:", "wowi:", "direct:"].iter().any(|kind| {
        id.strip_prefix(kind)
            .is_some_and(|rest| !rest.trim().is_empty())
    })
}

/// `gh:owner/repo`: a repository's own entry, not one of its release lines.
fn is_repo_id(id: &str) -> bool {
    repo_of_id(id).is_some_and(|repo| valid_repo_name(repo) && id.len() == "gh:".len() + repo.len())
}

/// A release line's id, `gh:owner/repo/<line>`. Only `[[split]]` makes these, and a line only
/// ever holds its own tag prefix's builds, so it's one addon.
pub fn is_line_id(id: &str) -> bool {
    is_github_id(id) && !is_repo_id(id)
}

/// An id a `[pins]` entry can be for: `gh:owner/repo`, `gh:owner/repo/<line>`, `wowi:1234` or
/// `direct:name`.
fn is_pinnable_id(id: &str) -> bool {
    let listing = id
        .strip_prefix("wowi:")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let direct = id
        .strip_prefix("direct:")
        .is_some_and(|name| !name.trim().is_empty());
    is_github_id(id) || listing || direct
}

/// The id of a GitHub entry: a repository's own, or one of its release lines'.
fn is_github_id(id: &str) -> bool {
    let Some(repo) = repo_of_id(id).filter(|repo| valid_repo_name(repo)) else {
        return false;
    };
    match &id["gh:".len() + repo.len()..] {
        "" => true,
        line => line.strip_prefix('/').is_some_and(|line| {
            is_line_prefix(line) && line.bytes().all(|b| !b.is_ascii_uppercase())
        }),
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapSpec {
    pub name: String,
    pub reason: String,
    #[serde(default)]
    pub website: Option<String>,
    /// The addon's real folder names, written by hand: `Leatrix_Plus` for Leatrix Plus,
    /// `!BugGrabber` for BugGrabber. Reserved like [`Curated::reserved`], so that nobody
    /// but a hand-picked entry can step into the gap.
    #[serde(default)]
    pub folders: Vec<String>,
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
    /// The file gets replaced many times a day under the same version, and its old
    /// links serve the newest copy. A checksum from a catalog built hours ago would
    /// then fail every install, so none is published, and no cache-busting query.
    #[serde(default)]
    pub changes_often: bool,
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
            .chain(self.overrides.keys())
            .chain(self.pack_ids());
        mentioned
            .filter_map(|id| repo_of_id(id))
            .map(str::to_ascii_lowercase)
            .chain(self.source.iter().map(|s| s.repo.to_ascii_lowercase()))
            .chain(self.split.iter().map(|s| s.repo.to_ascii_lowercase()))
            .collect()
    }

    /// Every id `[[packs]]` names, the addons and their packs.
    fn pack_ids(&self) -> impl Iterator<Item = &String> {
        self.packs
            .iter()
            .flat_map(|group| std::iter::once(&group.addon).chain(group.ids.iter()))
    }

    /// The `[[split]]` for this repository, if it has one.
    pub fn split_of(&self, repo: &str) -> Option<&SplitSpec> {
        self.split
            .iter()
            .find(|s| s.repo.eq_ignore_ascii_case(repo))
    }

    /// What `[pins]` says about this entry. Case doesn't matter.
    pub fn pin_of(&self, id: &str) -> Option<&PinSpec> {
        self.pins
            .iter()
            .find(|(pinned, _)| pinned.eq_ignore_ascii_case(id))
            .map(|(_, pin)| pin)
    }

    /// The main folder `[pins]` gives this entry, when it gives one.
    pub fn main_pin_of(&self, id: &str) -> Option<&str> {
        self.pin_of(id)?.main.as_deref()
    }

    /// A split repository that isn't seeded, which nothing promises to read on every run.
    pub fn unseeded_split(&self, seeds: &Seeds) -> Option<String> {
        self.split
            .iter()
            .find(|s| !seeds.repos.iter().any(|r| r.eq_ignore_ascii_case(&s.repo)))
            .map(|s| format!("split: {} isn't in seeds.toml", s.repo))
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

    /// Whether this catalog id is left out on purpose, by name or by owner. A release line
    /// goes with its repository.
    pub fn excludes_id(&self, id: &str) -> bool {
        self.exclude.ids.iter().any(|x| x.eq_ignore_ascii_case(id))
            || repo_of_id(id).is_some_and(|r| self.excludes_repo(r))
    }

    fn mistake(&self) -> Option<String> {
        for (id, repo) in &self.merges {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Some(format!("merges: {id} isn't a WoWInterface id"));
            }
            if !valid_repo_name(repo) {
                return Some(format!("merges: {repo} isn't owner/repo"));
            }
        }
        let problem = self
            .split_mistake()
            .or_else(|| self.packs_mistake())
            .or_else(|| self.pins_mistake());
        if problem.is_some() {
            return problem;
        }
        let gap_folders = self.gaps.iter().flat_map(|gap| gap.folders.iter());
        gap_folders
            .chain(self.reserved.iter())
            .find(|folder| !is_safe_folder(folder))
            .map(|folder| format!("{folder} can't be a folder name"))
    }

    fn split_mistake(&self) -> Option<String> {
        let mut repos = BTreeSet::new();
        for split in &self.split {
            let repo = &split.repo;
            if !valid_repo_name(repo) {
                return Some(format!("split: {repo} isn't owner/repo"));
            }
            if !repos.insert(repo.to_ascii_lowercase()) {
                return Some(format!("split: {repo} is split twice"));
            }
            if !is_tag_prefix(&split.main) {
                return Some(format!(
                    "split {repo}: {} can't be a tag prefix",
                    split.main
                ));
            }
            if split.lines.len() > MAX_LINES {
                return Some(format!(
                    "split {repo}: {} lines is more than {MAX_LINES}",
                    split.lines.len()
                ));
            }
            let mut seen = BTreeSet::new();
            for line in &split.lines {
                if !is_line_prefix(line) {
                    return Some(format!(
                        "split {repo}: {line} can't be a line, which takes up to {MAX_LINE_BYTES} letters, digits and dashes"
                    ));
                }
                if line.eq_ignore_ascii_case(&split.main) {
                    return Some(format!("split {repo}: {line} is its main prefix"));
                }
                if !seen.insert(line.to_ascii_lowercase()) {
                    return Some(format!("split {repo}: {line} is listed twice"));
                }
            }
        }
        None
    }

    fn packs_mistake(&self) -> Option<String> {
        let mut groups = BTreeSet::new();
        for group in &self.packs {
            let addon = &group.addon;
            if !looks_like_id(addon) {
                return Some(format!("packs: {addon} isn't a catalog id"));
            }
            let name = group.name.trim();
            if name.is_empty() {
                return Some(format!("packs for {addon}: a group has no name"));
            }
            if name.chars().count() > MAX_PACK_GROUP_CHARS {
                return Some(format!(
                    "packs for {addon}: {name} is longer than {MAX_PACK_GROUP_CHARS} characters"
                ));
            }
            if !groups.insert((addon.to_ascii_lowercase(), name.to_lowercase())) {
                return Some(format!("packs for {addon}: two groups are called {name}"));
            }
            if group.ids.is_empty() {
                return Some(format!("packs for {addon}: {name} names no addons"));
            }
            let mut seen = BTreeSet::new();
            for id in &group.ids {
                if !looks_like_id(id) {
                    return Some(format!("packs for {addon}: {id} isn't a catalog id"));
                }
                if id.eq_ignore_ascii_case(addon) {
                    return Some(format!("packs for {addon}: {name} lists the addon itself"));
                }
                if !seen.insert(id.to_ascii_lowercase()) {
                    return Some(format!("packs for {addon}: {name} lists {id} twice"));
                }
            }
            if let Some(problem) = met_by_mistake(&group.met_by) {
                return Some(format!("packs {name} for {addon}: {problem}"));
            }
        }
        None
    }

    fn pins_mistake(&self) -> Option<String> {
        let mut pinned = BTreeSet::new();
        for (id, pin) in &self.pins {
            if !is_pinnable_id(id) {
                return Some(format!("pins: {id} isn't a catalog id"));
            }
            if pin.main.is_some() {
                if let Some(problem) = self.main_mistake(id) {
                    return Some(format!("pins: {id} {problem}, so it takes only was"));
                }
            } else if pin.was.is_empty() {
                return Some(format!("pins for {id}: it names neither main nor was"));
            }
            if !pinned.insert(id.to_ascii_lowercase()) {
                return Some(format!("pins: {id} is pinned twice"));
            }
            if let Some(problem) = pin_mistake(pin) {
                return Some(format!("pins for {id}: {problem}"));
            }
        }
        None
    }

    /// Why this entry can't be given a `main` folder in `[pins]`, if it can't.
    fn main_mistake(&self, id: &str) -> Option<&'static str> {
        if !is_github_id(id) {
            return Some("isn't a GitHub entry, whose source already says which addon it is");
        }
        if !is_repo_id(id) {
            return Some("is a release line, and its tag prefix already says which addon it is");
        }
        repo_of_id(id)
            .and_then(|repo| self.split_of(repo))
            .map(|_| "is in [[split]], whose tag prefixes already say which addon each entry is")
    }
}

/// What's wrong with one `[pins]` entry's folders, if anything.
fn pin_mistake(pin: &PinSpec) -> Option<String> {
    if let Some(folder) = pin
        .main
        .iter()
        .chain(&pin.was)
        .find(|folder| !is_safe_folder(folder))
    {
        return Some(format!("{folder:?} can't be a folder name"));
    }
    if pin.was.len() > MAX_FORMERLY {
        return Some(format!(
            "was names more than {MAX_FORMERLY} folders, the most formerly keeps"
        ));
    }
    let mut seen = BTreeSet::new();
    for folder in &pin.was {
        if pin
            .main
            .as_ref()
            .is_some_and(|main| folder.eq_ignore_ascii_case(main))
        {
            return Some(format!("was names {folder}, which is its main folder now"));
        }
        if !seen.insert(folder.to_ascii_lowercase()) {
            return Some(format!("was names {folder} twice"));
        }
    }
    None
}

/// What's wrong with a pack group's `met_by`, if anything. The app counts any folder that starts
/// with one of these as the group met, so an empty or odd one could meet it for anything.
fn met_by_mistake(met_by: &[String]) -> Option<String> {
    if met_by.is_empty() {
        return Some(
            "met_by is empty: name the folder prefixes that meet it, its own packs' and any from elsewhere"
                .to_string(),
        );
    }
    if met_by.len() > MAX_MET_BY {
        return Some(format!("met_by has more than {MAX_MET_BY} prefixes"));
    }
    let mut seen = BTreeSet::new();
    for prefix in met_by {
        if !is_safe_folder(prefix) {
            return Some(format!("met_by {prefix:?} can't start a folder name"));
        }
        if !seen.insert(prefix.to_ascii_lowercase()) {
            return Some(format!("met_by names {prefix} twice"));
        }
    }
    None
}

/// Everything a person chose: `seeds.toml`, what `curated.toml` names, and both sides
/// of an approved merge. Always beats anything found by search. Release lines exist only
/// because someone wrote them down, so they're always in here. `[pins]` isn't: it only says
/// which addon an entry is and what it was, and vouches for nothing else about it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HandPicked {
    ids: BTreeSet<String>,
}

impl HandPicked {
    pub fn new(seeds: &Seeds, curated: &Curated) -> Self {
        let repo = |name: &str| format!("gh:{name}");
        let named = curated
            .featured
            .iter()
            .chain(curated.kits.iter().flat_map(|kit| kit.addons.iter()))
            .chain(curated.overrides.keys())
            .chain(curated.gaps.iter().flat_map(|gap| gap.alternatives.iter()))
            .chain(curated.gaps.iter().flat_map(|gap| gap.unrelated.iter()))
            .chain(curated.pack_ids())
            .cloned();
        let lines = curated.split.iter().flat_map(|split| {
            std::iter::once(repo(&split.repo)).chain(
                split
                    .lines
                    .iter()
                    .map(|line| SplitSpec::line_id(&split.repo, line)),
            )
        });
        let ids = named
            .chain(seeds.repos.iter().map(|name| repo(name)))
            .chain(seeds.wowi.iter().map(|id| format!("wowi:{id}")))
            .chain(curated.source.iter().map(|spec| repo(&spec.repo)))
            .chain(curated.direct.iter().map(|spec| spec.id.clone()))
            .chain(curated.merges.keys().map(|id| format!("wowi:{id}")))
            .chain(curated.merges.values().map(|name| repo(name)))
            .chain(lines)
            .map(|id| id.to_ascii_lowercase())
            .collect();
        Self { ids }
    }

    /// Whether a catalog id was chosen by hand. Case doesn't matter.
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(&id.to_ascii_lowercase())
    }
}

pub fn load_seeds(path: &Path) -> Result<Seeds, String> {
    let text = read_config(path)?;
    toml::from_str(&text).map_err(|e| format!("{} has a mistake in it: {e}", path.display()))
}

pub fn load_curated(path: &Path) -> Result<Curated, String> {
    let text = read_config(path)?;
    let curated: Curated = toml::from_str(&text)
        .map_err(|e| format!("{} has a mistake in it: {e}", path.display()))?;
    match curated.mistake() {
        Some(problem) => Err(format!("{} has a mistake in it: {problem}", path.display())),
        None => Ok(curated),
    }
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
        author: main_toc.author.as_deref().map(clean_author),
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
            let scan = scan_zip(RangeReader::new(source, size), &[&spec.name], None)
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
    let release = if spec.changes_often {
        Release {
            url: entry
                .url
                .split('?')
                .next()
                .unwrap_or(&entry.url)
                .to_string(),
            ..Release::default()
        }
    } else {
        Release {
            url: entry.url.clone(),
            size: entry.bytes,
            sha256: entry.sha256.clone(),
            ..Release::default()
        }
    };
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
                date: state.first_seen,
                ..release
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
    fn a_file_that_changes_often_gets_no_checksum_to_go_stale() {
        let entry = read_manifest(MANIFEST.as_bytes(), "addon", "https://foreverdb.net").unwrap();
        let state = DirectState {
            version: "0.57.0".into(),
            first_seen: 5,
            scan: ZipScan {
                folders: vec!["ForeverDB".into()],
                forever: true,
                ..ZipScan::default()
            },
            entry: None,
        };
        let spec = DirectSpec {
            id: "direct:foreverdb".into(),
            name: "ForeverDB".into(),
            site: "https://foreverdb.net".into(),
            ..DirectSpec::default()
        };
        let steady = direct_candidate(&spec, &entry, &state)
            .addon
            .stable
            .unwrap();
        assert_eq!(
            steady.url,
            "https://foreverdb.net/downloads/ForeverDB.zip?v=5a0e"
        );
        assert_eq!(steady.size, 5_558_921);
        assert!(steady.sha256.is_some());

        let often = DirectSpec {
            changes_often: true,
            ..spec
        };
        let release = direct_candidate(&often, &entry, &state)
            .addon
            .stable
            .unwrap();
        assert_eq!(release.url, "https://foreverdb.net/downloads/ForeverDB.zip");
        assert_eq!(release.version, "0.57.0");
        assert_eq!(release.date, 5);
        assert_eq!(release.size, 0);
        assert_eq!(release.sha256, None);
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
        for gap in &curated.gaps {
            assert!(
                !gap.folders.is_empty(),
                "{} needs its real folder names",
                gap.name
            );
        }
        let folders_of = |name: &str| {
            let gap = curated.gaps.iter().find(|gap| gap.name == name).unwrap();
            gap.folders.clone()
        };
        assert_eq!(folders_of("Leatrix Plus"), ["Leatrix_Plus"]);
        assert_eq!(folders_of("BugGrabber"), ["!BugGrabber"]);
        assert!(folders_of("ElvUI").contains(&"ElvUI_Options".to_string()));
        assert!(curated.reserved.contains(&"WeakAuras".to_string()));
        assert!(curated.reserved.contains(&"pfQuest".to_string()));

        assert!(curated.merges.len() >= 240, "today's reviewed merges");
        assert_eq!(curated.merges["25159"], "mooreatv/AuctionDB");
        assert!(
            !curated
                .merges
                .values()
                .any(|repo| repo.eq_ignore_ascii_case("brunohanss/Nauctions")),
            "a repository that copied a listing's id doesn't get the listing"
        );

        let picks = HandPicked::new(&seeds, &curated);
        for id in [
            "gh:Questie/Questie",
            "GH:QUESTIE/QUESTIE",
            "wowi:7032",
            "direct:foreverdb",
            "gh:tukui-org/ElvUI",
            "gh:Legacy-of-Sylvanaar/prat-3-0",
            "gh:oUF-wow/oUF",
            "wowi:9994",
            "gh:dldvlpr-addons-wow/Musca-Auras",
        ] {
            assert!(picks.contains(id), "{id} should be hand-picked");
        }
        for id in ["gh:brunohanss/Nauctions", "gh:zebra/TomTom", "wowi:1"] {
            assert!(!picks.contains(id), "{id} shouldn't be hand-picked");
        }
        assert!(curated.excludes_repo("HIPPULI/anything"));
        assert!(curated.excludes_id("gh:HIPPULI/anything"));
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

        assert_eq!(curated.unseeded_split(&seeds), None);
        for split in &curated.split {
            for line in &split.lines {
                let id = SplitSpec::line_id(&split.repo, line);
                assert!(is_github_id(&id), "{id}");
                assert!(picks.contains(&id), "{id} should be hand-picked");
                let edit = curated.overrides.get(&id);
                assert!(
                    edit.is_some_and(|e| e.name.is_some() && e.summary.is_some()),
                    "{id} needs a name and a summary in [overrides]"
                );
            }
        }
        for id in curated.pack_ids() {
            assert!(picks.contains(id), "{id} should be hand-picked");
        }
        let spoken: Vec<(&str, bool, &[String])> = curated
            .packs
            .iter()
            .filter(|group| group.addon == "gh:rusty-key/spoken-wow")
            .map(|group| (group.name.as_str(), group.needed, group.ids.as_slice()))
            .collect();
        assert_eq!(spoken.len(), 4);
        assert_eq!(
            spoken[0],
            (
                "Quest voices",
                true,
                &["gh:rusty-key/spoken-wow/quests-audio".to_string()][..]
            ),
            "the quest voices come first, and Spoken does nothing without them"
        );
        assert!(spoken[1..].iter().all(|(_, needed, _)| !needed));
        for folder in ["SpokenQuestsAudioAlliance", "SpokenQuestsAudioGossip"] {
            assert!(curated.reserved.contains(&folder.to_string()), "{folder}");
        }

        // Every language, CurseForge's splits, the old single-folder packs and the community
        // packs known today meet quest voices, and the names Spoken still reads meet places and
        // readables.
        let met_by = |name: &str| {
            let group = curated.packs.iter().find(|g| g.name == name).unwrap();
            group.met_by.clone()
        };
        assert_eq!(
            met_by("Quest voices"),
            ["SpokenQuestsAudio", "SpokenCommunity_"]
        );
        assert_eq!(
            met_by("Place voices"),
            ["SpokenPlacesAudio", "SpokenZonesAudio"]
        );
        assert_eq!(
            met_by("Readable voices"),
            ["SpokenReadablesAudio", "SpokenBooksAudio"]
        );
        assert_eq!(met_by("Boss voices"), ["SpokenBossesAudio"]);
        for group in curated.packs.iter() {
            for id in &group.ids {
                let name = curated.overrides[id].name.as_deref().unwrap();
                assert!(
                    name.ends_with("(English)"),
                    "{id}: a pack's name says its language"
                );
            }
        }

        // The data packs that had taken their addon's place.
        for (id, main) in [
            ("gh:jhunthrop/foreversixty", "ForeverSixty"),
            ("gh:LoyalFTW/Midnight-Routine", "MidnightRoutine"),
        ] {
            let pin = curated.pin_of(id).unwrap();
            assert_eq!(pin.main.as_deref(), Some(main), "{id}");
            assert!(pin.was.is_empty(), "{id}");
        }
        for id in curated.pins.keys() {
            assert!(!picks.contains(id), "{id}: a pin picks nothing");
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

        let split = |repo: &str, main: &str, lines: &str| {
            format!("[[split]]\nrepo = \"{repo}\"\nmain = \"{main}\"\nlines = [{lines}]\n")
        };
        let pack_met_by = |addon: &str, name: &str, ids: &str, met_by: &str| {
            format!(
                "[[packs]]\naddon = \"{addon}\"\nname = \"{name}\"\nids = [{ids}]\nmet_by = [{met_by}]\n"
            )
        };
        let pack = |addon: &str, name: &str, ids: &str| pack_met_by(addon, name, ids, "\"Voices\"");
        let pin = |id: &str, main: &str, was: &str| {
            format!("[pins.\"{id}\"]\nmain = \"{main}\"\nwas = [{was}]\n")
        };
        let was_only = |id: &str, was: &str| format!("[pins.\"{id}\"]\nwas = [{was}]\n");
        let twice = split("a/b", "main", "") + &split("A/B", "other", "");
        let long_name = pack("gh:a/b", &"x".repeat(41), "\"gh:a/c\"");
        let same_group =
            pack("gh:a/b", "Voices", "\"gh:a/c\"") + &pack("gh:a/b", "voices", "\"gh:a/d\"");
        let many: Vec<String> = (0..=MAX_LINES).map(|i| format!("\"l{i}\"")).collect();
        let many_prefixes: Vec<String> = (0..=MAX_MET_BY).map(|i| format!("\"V{i}\"")).collect();
        let many_was: Vec<String> = (0..=MAX_FORMERLY).map(|i| format!("\"Old{i}\"")).collect();
        let pinned_twice = pin("gh:a/b", "B", "") + &pin("GH:A/B", "B", "");
        let split_and_pinned = split("a/b", "main", "") + &pin("gh:a/b", "B", "");
        for wrong in [
            "[merges]\nabc = \"a/b\"\n".to_string(),
            "[merges]\n12 = \"not a repo\"\n".to_string(),
            "reserved = [\"../evil\"]\n".to_string(),
            "[[gaps]]\nname = \"G\"\nreason = \"R.\"\nfolders = [\"a/b\"]\n".to_string(),
            split("not a repo", "main", ""),
            twice,
            split("a/b", "main", "\"main\""),
            split("a/b", "main", "\"MAIN\""),
            split("a/b", "main", "\"x/y\""),
            split("a/b", "main", "\".hidden\""),
            split("a/b", "main", "\"with_underscore\""),
            split("a/b", "main", "\"-dash\""),
            split("a/b", "main", "\"x\", \"X\""),
            split("a/b", "", ""),
            split("a/b", ".main", ""),
            split("a/b", "main", &many.join(", ")),
            pack("gh:a/b", " ", "\"gh:a/c\""),
            long_name,
            pack("gh:a/b", "Voices", ""),
            pack("gh:a/b", "Voices", "\"gh:a/c\", \"GH:A/C\""),
            pack("gh:a/b", "Voices", "\"gh:a/b\""),
            pack("gh:a/b", "Voices", "\"a/c\""),
            pack("nothing", "Voices", "\"gh:a/c\""),
            same_group,
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", ""),
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", "\"\""),
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", "\" \""),
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", "\"Voices/Other\""),
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", "\"Voices\", \"VOICES\""),
            pack_met_by("gh:a/b", "Voices", "\"gh:a/c\"", &many_prefixes.join(", ")),
            pin("wowi:12", "B", ""),
            pin("direct:x", "B", ""),
            pin("gh:not a repo", "B", ""),
            pin("gh:a/b/quests-audio", "B", ""),
            pin("gh:a/b/c/d", "B", ""),
            pin("gh:a/b", "../evil", ""),
            pin("gh:a/b", "", ""),
            pin("gh:a/b", "B", "\"a/b\""),
            pin("gh:a/b", "B", "\"b\""),
            pin("gh:a/b", "B", "\"Old\", \"OLD\""),
            pin("gh:a/b", "B", &many_was.join(", ")),
            pinned_twice,
            split_and_pinned,
            "[pins.\"gh:a/b\"]\nmain = \"B\"\nfolder = \"C\"\n".to_string(),
            "[pins.\"wowi:12\"]\n".to_string(),
            was_only("wowi:12", ""),
            was_only("wowi:x", "\"Old\""),
            was_only("nothing", "\"Old\""),
            was_only("gh:a/b/c/d", "\"Old\""),
            was_only("direct: ", "\"Old\""),
            was_only("wowi:12", "\"../evil\""),
            was_only("wowi:12", &many_was.join(", ")),
        ] {
            std::fs::write(&path, &wrong).unwrap();
            let error = load_curated(&path).unwrap_err();
            assert!(error.contains("mistake"), "{wrong}: {error}");
        }
        std::fs::write(&path, pin("wowi:12", "B", "\"Old\"")).unwrap();
        let error = load_curated(&path).unwrap_err();
        assert!(
            error.contains("wowi:12 isn't a GitHub entry") && error.contains("only was"),
            "{error}"
        );

        // was alone writes a vouch back for any entry, a listing, a direct one, a release line
        // or a [[split]] repository included.
        let fine = pin("gh:a/c", "C", "\"OldC\"")
            + &split("a/b", "spoken", "\"quests-audio\", \"places2\"")
            + &pack_met_by(
                "gh:a/b",
                "Quest voices",
                "\"gh:a/b/quests-audio\"",
                "\"SpokenQuestsAudio\"",
            )
            + &was_only("wowi:12", "\"OldListing\"")
            + &was_only("direct:x", "\"OldDirect\"")
            + &was_only("gh:a/b/quests-audio", "\"SpokenQuestsAudioOld\"")
            + &was_only("gh:a/b", "\"SpokenPlayer\"")
            + &was_only("gh:a/d", "\"OldD\"");
        std::fs::write(&path, &fine).unwrap();
        let curated = load_curated(&path).unwrap();
        assert_eq!(
            curated.split[0].prefixes(),
            ["spoken", "quests-audio", "places2"]
        );
        assert!(!curated.packs[0].needed, "an extra unless it says so");
        assert_eq!(curated.packs[0].met_by, ["SpokenQuestsAudio"]);
        let pinned = curated.pin_of("GH:A/C").unwrap();
        assert_eq!(
            (pinned.main.as_deref(), &pinned.was[..]),
            (Some("C"), &["OldC".to_string()][..])
        );
        let listing = curated.pin_of("WOWI:12").unwrap();
        assert_eq!(
            (listing.main.as_deref(), &listing.was[..]),
            (None, &["OldListing".to_string()][..])
        );
        assert_eq!(curated.main_pin_of("gh:a/c"), Some("C"));
        for id in [
            "wowi:12",
            "gh:a/b",
            "gh:a/b/quests-audio",
            "gh:a/d",
            "gh:a/e",
        ] {
            assert_eq!(curated.main_pin_of(id), None, "{id}");
        }
    }

    #[test]
    fn a_split_repository_must_be_seeded() {
        let curated = Curated {
            split: vec![SplitSpec {
                repo: "rusty-key/spoken-wow".into(),
                main: "spoken".into(),
                lines: vec![],
            }],
            ..Curated::default()
        };
        let mut seeds = Seeds::default();
        assert!(
            curated
                .unseeded_split(&seeds)
                .unwrap()
                .contains("rusty-key/spoken-wow")
        );
        seeds.repos.push("Rusty-Key/Spoken-WoW".into());
        assert_eq!(curated.unseeded_split(&seeds), None);
    }

    #[test]
    fn a_release_line_goes_with_its_repository() {
        assert_eq!(
            SplitSpec::line_id("rusty-key/spoken-wow", "Quests-Audio"),
            "gh:rusty-key/spoken-wow/quests-audio"
        );
        assert_eq!(
            repo_of_id("gh:rusty-key/spoken-wow/quests-audio"),
            Some("rusty-key/spoken-wow")
        );
        assert_eq!(repo_of_id("gh:a/b"), Some("a/b"));
        assert_eq!(repo_of_id("gh:a"), None);
        assert_eq!(repo_of_id("wowi:12"), None);

        let curated = Curated {
            exclude: Exclude {
                repos: vec!["rusty-key/spoken-wow".into()],
                ..Exclude::default()
            },
            overrides: BTreeMap::from([("gh:Other/Repo/pack".to_string(), Override::default())]),
            packs: vec![PackSpec {
                addon: "gh:third/main".into(),
                name: "Voices".into(),
                needed: false,
                ids: vec!["gh:fourth/voices".into()],
                met_by: vec!["Voices".into()],
            }],
            ..Curated::default()
        };
        assert!(curated.excludes_id("gh:rusty-key/spoken-wow/quests-audio"));
        assert!(curated.excludes_id("gh:rusty-key/spoken-wow"));
        assert!(!curated.excludes_id("gh:rusty-key/other/quests-audio"));
        let named = curated.named_repos();
        for repo in ["other/repo", "third/main", "fourth/voices"] {
            assert!(named.contains(repo), "{repo}: {named:?}");
        }
        assert!(!named.iter().any(|repo| repo.contains("pack")), "{named:?}");
    }

    #[test]
    fn lines_and_everything_packs_name_are_hand_picked() {
        let curated = Curated {
            split: vec![SplitSpec {
                repo: "rusty-key/spoken-wow".into(),
                main: "spoken".into(),
                lines: vec!["quests-audio".into()],
            }],
            packs: vec![PackSpec {
                addon: "gh:third/main".into(),
                name: "Voices".into(),
                needed: true,
                ids: vec!["wowi:99".into()],
                met_by: vec!["Voices".into()],
            }],
            pins: BTreeMap::from([(
                "gh:fifth/renamed".to_string(),
                PinSpec {
                    main: Some("Renamed".into()),
                    was: vec!["Old".into()],
                },
            )]),
            ..Curated::default()
        };
        let picks = HandPicked::new(&Seeds::default(), &curated);
        for id in [
            "gh:rusty-key/spoken-wow",
            "gh:rusty-key/spoken-wow/quests-audio",
            "gh:third/main",
            "wowi:99",
        ] {
            assert!(picks.contains(id), "{id}");
        }
        assert!(!picks.contains("gh:rusty-key/spoken-wow/places-audio"));
        assert!(
            !picks.contains("gh:fifth/renamed"),
            "saying which addon an entry is doesn't pick it"
        );
        assert!(!curated.named_repos().contains("fifth/renamed"));
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
