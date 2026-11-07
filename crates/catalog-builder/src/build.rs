//! The pipeline: discover, detect, merge, categorize, check, write.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use feeling_core::catalog::{
    CATALOG_SCHEMA, CATEGORIES, Catalog, Gap, Release, Source, category_name,
};
use flate2::Compression;
use flate2::write::GzEncoder;

use crate::categorize::categorize;
use crate::curated::{self, Curated, HandPicked, Seeds, SplitSpec};
use crate::detect::{self, Build, Chosen, GhAsset, Probe, ReleaseVerdict};
use crate::gate::{GateContext, run_gate};
use crate::github::{self, BatchResult, PrefixReleases, RepoInfo};
use crate::http::{Channel, Http, unix_now};
use crate::merge::{self, Pin, Pins};
use crate::model::{Candidate, Held, MAX_RENAMES, ZipScan};
use crate::rangezip::{HttpRange, RangeReader};
use crate::state::{Remembered, RepoState, State, write_atomic};
use crate::text::{
    clean_author, clean_name, clean_summary, clean_version, encode_path, round_sig,
    says_nothing_new, valid_repo_name,
};
use crate::tier::assign_tiers;
use crate::wowi::{self, WowiState};
use crate::zipscan::{ScanError, scan_zip};

/// Topic and code search only find new repositories, so once a day is plenty.
const SEARCH_EVERY_SECS: i64 = 20 * 60 * 60;
const DAY_SECS: i64 = 24 * 60 * 60;
/// How often a listed addon gets a proper look even without a new release: download
/// counts move, and files can be replaced under the same tag.
const RELOOK_LISTED_SECS: i64 = DAY_SECS;
/// The same for repositories that weren't Forever-capable last time.
const RELOOK_OTHERS_SECS: i64 = 7 * DAY_SECS;
/// How often progress is written to the state file.
const SAVE_EVERY: Duration = Duration::from_secs(120);
/// Significant digits kept for download and star counts in the published file.
const COUNT_DIGITS: u32 = 3;
/// How many releases GraphQL returns per repository. A list this long may hide older ones.
const RELEASES_FETCHED: usize = 15;
/// GitHub refuses a file over 100 MiB, and the push carries the catalog with it. Past this
/// the workflow publishes the catalog and keeps the previous scan cache.
const MAX_STATE_BYTES: u64 = 95 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub out: PathBuf,
    pub seeds: PathBuf,
    pub curated: PathBuf,
    pub previous: Option<PathBuf>,
    pub state: Option<PathBuf>,
    pub limit: Option<usize>,
    pub wowi_limit: Option<usize>,
    pub no_wowi: bool,
    pub max_minutes: Option<u64>,
    /// Publish even if the catalog lost a lot. For a maintainer who has looked at why.
    pub accept_smaller: bool,
}

/// Network access for GitHub release detection.
struct LiveProbe<'a> {
    http: &'a Http,
}

impl Probe for LiveProbe<'_> {
    fn release_json(&mut self, asset: &GhAsset) -> Result<String, String> {
        let reply = self
            .http
            .get_with(Channel::Asset, &asset.url, &[], 4 * 1024 * 1024)
            .map_err(|e| format!("release.json: {e}"))?;
        Ok(String::from_utf8_lossy(&reply.body).into_owned())
    }

    fn read_zip(
        &mut self,
        asset: &GhAsset,
        hints: &[&str],
        pin: Option<&str>,
    ) -> Result<ZipScan, String> {
        let source = HttpRange::new(self.http, Channel::Asset, &asset.url, Some(asset.size));
        match scan_zip(RangeReader::new(source, asset.size), hints, pin) {
            Ok(scan) => Ok(scan),
            Err(ScanError::Invalid(detail)) => {
                // Remembered as "no Forever build", so a bad upload isn't re-read every run.
                eprintln!("{}: not a usable zip: {detail}", asset.url);
                Ok(ZipScan::default())
            }
            Err(error @ ScanError::Io(_)) => Err(error.to_string()),
        }
    }
}

/// Writes the state file now and then during long loops.
struct Saver {
    path: PathBuf,
    last: Instant,
}

impl Saver {
    fn maybe_save(&mut self, state: &State) {
        if self.last.elapsed() >= SAVE_EVERY {
            self.save(state);
        }
    }

    fn save(&mut self, state: &State) {
        if let Err(problem) = state.save(&self.path) {
            eprintln!("{problem}");
        }
        self.last = Instant::now();
    }
}

fn discover(http: &Http, state: &mut State, seeds: &Seeds, curated: &Curated, searches: bool) {
    for repo in &seeds.repos {
        state.note_repo(repo, "seed");
    }
    for spec in &curated.source {
        state.note_repo(&spec.repo, "curated");
    }
    state.repos.retain(|name, _| !curated.excludes_repo(name));

    match github::fetch_strongbox(http, state.strongbox_etag.as_deref()) {
        Ok(Some(update)) => {
            eprintln!("strongbox: {} Forever repositories", update.repos.len());
            for repo in update.repos {
                state.note_repo(&repo.name, "strongbox").known_downloads = repo.downloads;
            }
            state.strongbox_etag = update.etag;
        }
        Ok(None) => eprintln!("strongbox: unchanged"),
        Err(problem) => eprintln!("strongbox: {problem}"),
    }

    if !searches {
        return;
    }
    if unix_now() - state.searched_at < SEARCH_EVERY_SECS {
        eprintln!("search: done recently, skipping");
        return;
    }
    let topics = github::search_topics(http);
    eprintln!("topic search: {} hits", topics.len());
    for repo in topics {
        state.note_repo(&repo, "topic");
    }
    let code = github::search_code(http);
    eprintln!("code search: {} repositories", code.len());
    for repo in code {
        state.note_repo(&repo, "code");
    }
    state.searched_at = unix_now();
}

/// Everything to check this run, seeds first and then the best known, without
/// duplicates that differ only by case.
fn repo_order(
    state: &State,
    seeds: &Seeds,
    curated: &Curated,
    limit: Option<usize>,
) -> Vec<String> {
    let mut known: Vec<(&String, &RepoState)> = state.repos.iter().collect();
    known.sort_by(|a, b| {
        b.1.known_downloads
            .cmp(&a.1.known_downloads)
            .then(a.0.cmp(b.0))
    });
    let mut seen = HashSet::new();
    let mut order: Vec<String> = seeds
        .repos
        .iter()
        .chain(known.into_iter().map(|(name, _)| name))
        .filter(|name| valid_repo_name(name) && !curated.excludes_repo(name))
        .filter(|name| seen.insert(name.to_ascii_lowercase()))
        .cloned()
        .collect();
    if let Some(limit) = limit {
        order.truncate(limit);
    }
    order
}

/// Turn what detection found into a catalog entry.
pub fn github_candidate(
    info: &RepoInfo,
    chosen: &Chosen,
    known_downloads: u64,
) -> Option<Candidate> {
    let primary = chosen.stable.as_ref().or(chosen.beta.as_ref())?;
    let scan = &primary.scan;
    let (owner, repo_name) = info.name.split_once('/').unwrap_or(("", &info.name));
    let main = scan.folders.first().map_or(repo_name, String::as_str);

    let mut name = clean_name(scan.title.as_deref().unwrap_or_default());
    if name.is_empty() {
        name = clean_name(main);
    }
    let mut summary = clean_summary(scan.notes.as_deref().unwrap_or_default());
    if says_nothing_new(&summary, &name) {
        summary = clean_summary(info.description.as_deref().unwrap_or_default());
    }
    let mut author = clean_author(scan.author.as_deref().unwrap_or_default());
    if author.is_empty() {
        author = owner.to_string();
    }
    let zip_downloads: u64 = info
        .releases
        .iter()
        .flat_map(|r| r.assets.iter())
        .filter(|a| a.name.to_ascii_lowercase().ends_with(".zip"))
        .map(|a| a.downloads)
        .sum();
    let release = |build: &Build| {
        let mut version = clean_version(&build.tag, &[main, repo_name]);
        // A tag like `latest` keeps its name while its files change, so the
        // file's digest has to be what tells one version from the next.
        if is_rolling_tag(&build.tag)
            && let Some(digest) = build.asset.sha256.as_deref().and_then(|d| d.get(..7))
        {
            version = format!("{version}-{digest}");
        }
        Release {
            version,
            url: build.asset.url.clone(),
            size: build.asset.size,
            date: build.published,
            sha256: build.asset.sha256.clone(),
            ..Release::default()
        }
    };
    Some(Candidate {
        addon: feeling_core::catalog::Addon {
            id: format!("gh:{}", info.name),
            name,
            summary,
            author,
            source: Source::Github {
                repo: info.name.clone(),
            },
            website: Some(format!("https://github.com/{}", info.name)),
            stable: chosen.stable.as_ref().map(release),
            beta: chosen.beta.as_ref().map(release),
            folders: scan.folders.clone(),
            deps: scan.external_deps(),
            downloads: round_sig(zip_downloads.max(known_downloads), COUNT_DIGITS),
            stars: rounded_stars(info.stars),
            ids: scan.external_ids(),
            ..feeling_core::catalog::Addon::default()
        },
        toc_category: scan.category.clone(),
        topics: info.topics.clone(),
        fork_of: info
            .is_fork
            .then(|| info.fork_of.clone().unwrap_or_default()),
        no_longer_forever: chosen.no_longer_forever,
        ..Candidate::default()
    })
}

/// `latest` or `Release`: a tag with no number in it is one authors keep reusing.
fn is_rolling_tag(tag: &str) -> bool {
    !tag.bytes().any(|b| b.is_ascii_digit())
}

fn rounded_stars(stars: u32) -> u32 {
    u32::try_from(round_sig(u64::from(stars), COUNT_DIGITS)).unwrap_or(stars)
}

/// Whether an earlier build's file can still be downloaded: its release is still listed, or
/// the list is full, so the release may only be older than what it shows.
fn file_can_exist(previous: &Candidate, info: &RepoInfo) -> bool {
    let Some(release) = previous
        .addon
        .stable
        .as_ref()
        .or(previous.addon.beta.as_ref())
    else {
        return false;
    };
    let still_listed = info
        .releases
        .iter()
        .flat_map(|r| r.assets.iter())
        .any(|a| a.url == release.url);
    still_listed || info.releases.len() >= RELEASES_FETCHED
}

/// A previous entry for a repository that now shows no Forever build. It stays,
/// marked grey, as long as its file can still exist.
fn grey_previous(previous: &Candidate, info: &RepoInfo) -> Option<Candidate> {
    file_can_exist(previous, info).then(|| {
        let mut grey = previous.clone();
        grey.no_longer_forever = true;
        grey
    })
}

/// Counts that end up in the summary.
#[derive(Debug, Default)]
struct Tally {
    rejected: BTreeMap<String, u32>,
    kept_last_known: u32,
    gone: u32,
    no_build: u32,
    placeholder: u32,
    unchanged: u32,
    /// New repositories not read this run, because their owner already has plenty.
    capped: u32,
    held: usize,
    /// Lowercased ids a source gave a clear answer about this run, whatever the answer.
    answered: BTreeSet<String>,
    /// Entries that passed over a newer release as another addon's: what each lists instead.
    passed_over: Vec<String>,
    /// Releases taken for another addon's with some of their zips never read.
    left_unread: Vec<String>,
    /// `[[split]]` repositories GitHub knows by another name now.
    renamed_splits: Vec<String>,
    /// Former main folders the catalog vouches for from this build on.
    vouched: Vec<String>,
    /// Former main folders it stops vouching for, and which entry ships each.
    unvouched: Vec<String>,
    /// GitHub main folders that moved with nothing vouching for the old one.
    moved: Vec<String>,
    /// Folders `[pins]` says an entry was that don't fit in its `formerly`.
    cut: Vec<String>,
    /// Packs too big for old apps to install by themselves, or of unknown size, published
    /// unreviewed.
    unreviewed: merge::Unreviewed,
    /// Hand-picked repositories GitHub knows by another name now.
    renamed: Vec<String>,
}

fn repo_id(name: &str) -> String {
    format!("gh:{}", name.to_ascii_lowercase())
}

/// Notes that a source answered for a repository, and so for each of its release lines.
fn answer(tally: &mut Tally, name: &str, split: Option<&SplitSpec>) {
    tally.answered.insert(repo_id(name));
    for line in split.into_iter().flat_map(|s| s.lines.iter()) {
        tally
            .answered
            .insert(SplitSpec::line_id(name, line).to_ascii_lowercase());
    }
}

/// The `[[split]]` for a repository GitHub answered for. After a rename, `curated.toml` may
/// still use the name it was asked for, or already the new one.
fn split_of<'a>(curated: &'a Curated, asked: &str, name: &str) -> Option<&'a SplitSpec> {
    curated.split_of(name).or_else(|| curated.split_of(asked))
}

/// What a repository's last looks found that's still wanted: its own entry, then the entries
/// of the release lines `[[split]]` still names.
pub fn listed_entries(record: &RepoState, split: Option<&SplitSpec>) -> Vec<Candidate> {
    let lines = split
        .into_iter()
        .flat_map(|split| split.lines.iter())
        .filter_map(|line| record.lines.get(line)?.entry.clone());
    record.entry.iter().cloned().chain(lines).collect()
}

/// What reading one repository's releases goes by, besides the repository itself.
struct Look<'a> {
    /// The name GitHub was asked for in the full look. The quick look already moved a renamed
    /// repository to its new name, so this is the old one only when GitHub renamed it between
    /// the two looks. The id the last catalog used stays on the scan cache's last result.
    asked: &'a str,
    /// Someone chose the repository by hand, so a placeholder title doesn't keep it out.
    picked: bool,
    pins: &'a Pins<'a>,
    out_of_time: &'a dyn Fn() -> bool,
}

/// One addon a repository publishes, read on its own: the repository's own addon, or a
/// release line of one in `[[split]]`.
struct Product<'a> {
    id: String,
    /// The repository, with only this addon's releases.
    info: &'a RepoInfo,
    known_downloads: u64,
    picked: bool,
    pin: Option<Pin>,
    /// The tag prefix cut off its releases' tags, in a split repository.
    prefix: Option<&'a str>,
    /// A release line, whose page is its own release's rather than the repository's.
    is_line: bool,
}

impl Product<'_> {
    /// A tag as GitHub has it.
    fn full_tag(&self, tag: &str) -> String {
        match self.prefix {
            Some(prefix) => format!("{prefix}/{tag}"),
            None => tag.to_string(),
        }
    }

    /// The entry under this addon's own id, and for a line, with its own release's page.
    fn own(&self, mut entry: Candidate, chosen: &Chosen) -> Candidate {
        if self.is_line {
            entry.addon.id.clone_from(&self.id);
            if let Some(build) = chosen.stable.as_ref().or(chosen.beta.as_ref()) {
                entry.addon.website = Some(format!(
                    "https://github.com/{}/releases/tag/{}",
                    self.info.name,
                    encode_path(&self.full_tag(&build.tag))
                ));
            }
        }
        entry
    }
}

/// The summary line for an entry that passed over `newer` as another addon's, saying what it
/// lists instead, and how to tell the builder when `newer` is really this addon. `left_out`: it
/// lists nothing, and stays that way while the pin stands. `unread`: some of `newer`'s zips
/// weren't read, so only the ones read are known not to ship the pinned folder.
fn passed_over(
    id: &str,
    instead: &str,
    newer: &str,
    pin: &Pin,
    left_out: bool,
    unread: bool,
) -> String {
    let main = &pin.main;
    let read = if unread { " in the zips read" } else { "" };
    let line =
        format!("{id} {instead}, passed over newer {newer}, which doesn't ship {main}{read}");
    if pin.written {
        format!("{line}, the main folder [pins] in curated.toml gives it")
    } else if left_out {
        format!(
            "{line}. It stays out until a build that ships {main} is in reach, or [pins.\"{id}\"] in curated.toml says which folder it should list"
        )
    } else {
        format!(
            "{line}. If {newer} holds the addon this entry should list, set [pins.\"{id}\"] in curated.toml"
        )
    }
}

/// The summary line for a release taken for another addon's with zips nobody read: a zip read in
/// it was another addon's, and none was a Forever build of the pinned addon, `main`.
fn left_unread(id: &str, tag: &str, unread: u32, main: &str) -> String {
    format!(
        "{id} passed over {tag} with {unread} of its zips unread and no Forever build of {main} among the ones read. It isn't read again until its files or this entry's main folder change"
    )
}

/// After GitHub renames a repository, its entry is still the one the last catalog listed under
/// the id its last result has. The ids it had stay on every later result, so later builds know
/// them too.
fn carry_old_ids(entry: &mut Candidate, last: Option<&Candidate>) {
    let Some(last) = last else {
        return;
    };
    let id = entry.addon.id.clone();
    let mut seen = HashSet::new();
    entry.renamed_from = std::iter::once(&last.addon.id)
        .chain(&last.renamed_from)
        .filter(|old| !old.eq_ignore_ascii_case(&id))
        .filter(|old| seen.insert(old.to_ascii_lowercase()))
        .take(MAX_RENAMES)
        .cloned()
        .collect();
}

/// Reads one addon's releases into its entry: its newest build that's still the same addon,
/// or what to keep when there isn't one. Returns the entry, which is also what the scan cache
/// keeps as the last result, and whether anything failed.
fn read_product(
    probe: &mut dyn Probe,
    product: &Product<'_>,
    verdicts: &mut BTreeMap<String, ReleaseVerdict>,
    last: Option<&Candidate>,
    out_of_time: &dyn Fn() -> bool,
    tally: &mut Tally,
) -> (Option<Candidate>, bool) {
    let info = product.info;
    let repo_name = info.name.split('/').nth(1).unwrap_or(&info.name);
    let pin = product.pin.as_ref();
    let main = pin.map(|pin| pin.main.as_str());
    let chosen = detect::choose_builds(
        repo_name,
        &info.releases,
        verdicts,
        main,
        probe,
        out_of_time,
    );
    for problem in &chosen.errors {
        eprintln!("{} {problem}", product.id);
    }
    for (tag, unread) in &chosen.left_unread {
        let tag = product.full_tag(tag);
        tally.left_unread.push(left_unread(
            &product.id,
            &tag,
            *unread,
            main.unwrap_or_default(),
        ));
    }
    let incomplete = !chosen.errors.is_empty();
    let skipped = chosen.skipped.as_deref().map(|tag| product.full_tag(tag));
    let skipped_unread = chosen
        .left_unread
        .iter()
        .any(|(tag, _)| chosen.skipped.as_ref() == Some(tag));
    let last_result = last;
    // A last result that's another addon is no use for keeping this one listed, and one that is
    // keeps the pinned folder first, as every build of it does now.
    let last = last
        .filter(|entry| detect::ships(&entry.addon.folders, main))
        .map(|entry| {
            let mut entry = entry.clone();
            if let Some(main) = main {
                detect::lead_with(&mut entry.addon.folders, main);
            }
            entry
        });

    let entry = match github_candidate(info, &chosen, product.known_downloads) {
        Some(entry) if merge::is_placeholder_name(&entry.addon.name) && !product.picked => {
            tally.placeholder += 1;
            eprintln!("{}: skipped, its title is only a placeholder", product.id);
            None
        }
        Some(entry) => {
            let listed = chosen.stable.as_ref().or(chosen.beta.as_ref());
            if let (Some(pin), Some(newer), Some(listed)) = (pin, &skipped, listed) {
                let instead = format!("lists {}", product.full_tag(&listed.tag));
                let line = passed_over(&product.id, &instead, newer, pin, false, skipped_unread);
                tally.passed_over.push(line);
            }
            Some(product.own(entry, &chosen))
        }
        None if incomplete => {
            if last.is_some() {
                tally.kept_last_known += 1;
            }
            last
        }
        None => match (pin, skipped) {
            (Some(pin), Some(newer)) => {
                // Nothing in reach is this addon. Its last build stays while its file can.
                let had_last = last.is_some();
                let kept = last.filter(|entry| file_can_exist(entry, info));
                let instead = match (&kept, had_last) {
                    (Some(entry), _) => format!("keeps {}, its last build", listed_version(entry)),
                    (None, true) => "is left out, as its last build's file is gone".to_string(),
                    (None, false) => "is left out, with no build of its own to keep".to_string(),
                };
                let left_out = kept.is_none();
                let line =
                    passed_over(&product.id, &instead, &newer, pin, left_out, skipped_unread);
                tally.passed_over.push(line);
                kept
            }
            _ => {
                tally.no_build += 1;
                last.as_ref()
                    .and_then(|previous| grey_previous(previous, info))
            }
        },
    };
    let mut entry = entry;
    if let Some(entry) = &mut entry {
        carry_old_ids(entry, last_result);
    }
    (entry, incomplete)
}

/// The version an entry lists, its stable one first.
fn listed_version(entry: &Candidate) -> &str {
    entry
        .addon
        .stable
        .as_ref()
        .or(entry.addon.beta.as_ref())
        .map_or("", |release| release.version.as_str())
}

/// The repository's newest tag, whatever addon it's for, so the quick look notices any new
/// release. Rounded to the day, so looking at something and finding no change leaves the
/// state file byte-identical, and a run with no news doesn't need to push anything.
fn note_look(record: &mut RepoState, info: &RepoInfo) {
    record.newest_tag = info.releases.first().map(|r| r.tag.clone());
    record.looked_at = unix_now().div_euclid(DAY_SECS) * DAY_SECS;
}

fn process_repo(
    probe: &mut dyn Probe,
    info: &RepoInfo,
    record: &mut RepoState,
    look: &Look<'_>,
    tally: &mut Tally,
) -> Option<Candidate> {
    note_look(record, info);
    let id = format!("gh:{}", info.name);
    let asked = format!("gh:{}", look.asked);
    // After GitHub renames a repository, its last result still has the id the last catalog
    // and `curated.toml` use for it, and every later result keeps the ids it had.
    let last_ids = record.entry.iter().flat_map(Candidate::known_as);
    let ids: Vec<&str> = [id.as_str(), asked.as_str()]
        .into_iter()
        .chain(last_ids)
        .collect();
    let product = Product {
        pin: look.pins.get(&ids, record.entry.as_ref()),
        id,
        info,
        known_downloads: record.known_downloads,
        picked: look.picked,
        prefix: None,
        is_line: false,
    };
    let (entry, incomplete) = read_product(
        probe,
        &product,
        &mut record.releases,
        record.entry.as_ref(),
        look.out_of_time,
        tally,
    );
    record.incomplete = incomplete;
    record.entry.clone_from(&entry);
    entry
}

/// A repository in `[[split]]`. Its own addon reads only the main prefix's releases, and each
/// line is an entry of its own that reads only its own. No other prefix is ever read.
/// `fetched` is each prefix's releases, or why they couldn't be fetched. Nothing here is
/// pinned: the prefixes already decide which addon each entry is, and a pin taken from a
/// catalog that listed the wrong one would only keep it there.
fn process_split(
    probe: &mut dyn Probe,
    info: &RepoInfo,
    split: &SplitSpec,
    fetched: Option<&Result<PrefixReleases, String>>,
    record: &mut RepoState,
    look: &Look<'_>,
    tally: &mut Tally,
) -> Vec<Candidate> {
    note_look(record, info);
    record.lines.retain(|line, _| split.lines.contains(line));
    let fetched = match fetched {
        Some(Ok(fetched)) => fetched,
        failed => {
            let problem = failed.and_then(|f| f.as_ref().err());
            eprintln!(
                "{}: {}",
                info.name,
                problem.map_or("its releases weren't fetched", String::as_str)
            );
            record.incomplete = true;
            for line in &split.lines {
                record.lines.entry(line.clone()).or_default().incomplete = true;
            }
            let kept = listed_entries(record, Some(split));
            tally.kept_last_known += u32::try_from(kept.len()).unwrap_or(u32::MAX);
            return kept;
        }
    };
    let own = |prefix: &str| fetched.get(prefix).cloned().unwrap_or_default();

    let main_info = RepoInfo {
        releases: own(&split.main),
        ..info.clone()
    };
    let main = Product {
        pin: None,
        id: format!("gh:{}", info.name),
        info: &main_info,
        // Counts from elsewhere cover the whole repository, every line's downloads included.
        known_downloads: 0,
        picked: look.picked,
        prefix: Some(&split.main),
        is_line: false,
    };
    let (entry, incomplete) = read_product(
        probe,
        &main,
        &mut record.releases,
        record.entry.as_ref(),
        look.out_of_time,
        tally,
    );
    record.incomplete = incomplete;
    record.entry.clone_from(&entry);
    let mut entries: Vec<Candidate> = entry.into_iter().collect();

    for line in &split.lines {
        // An addon of its own: the repository's text, stars, topics and forks are its own addon's.
        let line_info = RepoInfo {
            name: info.name.clone(),
            releases: own(line),
            ..RepoInfo::default()
        };
        let product = Product {
            pin: None,
            id: SplitSpec::line_id(&info.name, line),
            info: &line_info,
            known_downloads: 0,
            picked: true,
            prefix: Some(line),
            is_line: true,
        };
        let state = record.lines.entry(line.clone()).or_default();
        let (entry, incomplete) = read_product(
            probe,
            &product,
            &mut state.releases,
            state.entry.as_ref(),
            look.out_of_time,
            tally,
        );
        state.incomplete = incomplete;
        state.entry.clone_from(&entry);
        entries.extend(entry);
    }
    entries
}

/// `[pins]` says the entry is an addon its last result doesn't list first, such as one just
/// written to bring back an addon another had replaced. The quick look's "nothing new" doesn't
/// settle that.
fn pin_unmet(record: &RepoState, curated: &Curated, names: [&str; 2]) -> bool {
    let main = names
        .iter()
        .find_map(|name| curated.main_pin_of(&format!("gh:{name}")));
    match (main, &record.entry) {
        (Some(main), Some(entry)) => !detect::leads(&entry.addon.folders, Some(main)),
        _ => false,
    }
}

/// `[[split]]` repositories GitHub now answers for under another name while `curated.toml`
/// still has the old one: the new name in lower case, and the one `[[split]]` has.
fn renamed_splits<'a>(heads: &BatchResult, curated: &'a Curated) -> BTreeMap<String, &'a str> {
    heads
        .found
        .iter()
        .filter_map(|(asked, head)| {
            let split = curated.split_of(asked)?;
            let renamed = !split.repo.eq_ignore_ascii_case(&head.name)
                && curated.split_of(&head.name).is_none();
            renamed.then(|| (head.name.to_ascii_lowercase(), split.repo.as_str()))
        })
        .collect()
}

/// Says loudly that a `[[split]]` repository has a new name on GitHub, because it stays as it
/// was until a person puts that name in `seeds.toml` and `curated.toml`.
fn note_renamed_split(tally: &mut Tally, old: &str, new: &str) {
    let line = format!(
        "{old} is {new} on GitHub now. It keeps what it had until seeds.toml and curated.toml use the new name."
    );
    eprintln!("warning: {line}");
    tally.renamed_splits.push(line);
}

/// A release line that was never read, such as one just added to `[[split]]`. The quick look
/// only sees the repository's newest tag, which says nothing about that.
fn has_unread_lines(record: &RepoState, split: Option<&SplitSpec>) -> bool {
    split.is_some_and(|split| {
        split
            .lines
            .iter()
            .any(|line| !record.lines.contains_key(line))
    })
}

/// Does this repository need its releases fetched, or does the quick look say
/// nothing can have changed? This is what keeps a routine run cheap.
pub fn needs_full_look(record: &RepoState, head: &RepoInfo, now: i64) -> bool {
    let newest = head.releases.first().map(|r| r.tag.as_str());
    let failed = record.incomplete || record.lines.values().any(|line| line.incomplete);
    if failed || record.newest_tag.as_deref() != newest {
        return true;
    }
    // A release line counts as much as the repository's own addon: its file can be replaced
    // under the same tag too.
    let listed = record.entry.is_some() || record.lines.values().any(|line| line.entry.is_some());
    // No releases, yet something is listed: the tag comparison can't see this when a
    // rename and a deleted release fall between two runs, so the entry would stay for good.
    let Some(newest) = newest else {
        return listed;
    };
    if is_rolling_tag(newest) {
        return true;
    }
    let relook = if listed {
        RELOOK_LISTED_SECS
    } else {
        RELOOK_OTHERS_SECS
    };
    now - record.looked_at >= relook
}

/// Repositories each owner already has listed, not counting hand-picked ones.
fn listed_per_owner(state: &State, picks: &HandPicked) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, record) in &state.repos {
        if record.entry.is_some() && !picks.contains(&repo_id(name)) {
            *counts.entry(owner_of(name)).or_default() += 1;
        }
    }
    counts
}

fn owner_of(repo: &str) -> String {
    repo.split('/').next().unwrap_or(repo).to_ascii_lowercase()
}

/// What the quick look says about each repository: listed entries that can't have changed
/// come back right away, and the rest are named for a proper look.
fn triage_heads(
    heads: &BatchResult,
    state: &mut State,
    curated: &Curated,
    picks: &HandPicked,
    now: i64,
    tally: &mut Tally,
) -> (Vec<Candidate>, Vec<String>) {
    tally.gone += u32::try_from(heads.gone.len()).unwrap_or(u32::MAX);
    for name in &heads.gone {
        state.repos.remove(name);
        answer(tally, name, curated.split_of(name));
    }
    let hand_picked = curated.named_repos();

    let mut entries = Vec::new();
    for name in &heads.failed {
        entries.extend(keep_last_known(state, curated, name, tally));
    }

    let mut seen = BTreeSet::new();
    let mut to_read = Vec::new();
    // A flood of new repositories under one name stops costing anything here, before a
    // single release is read. Ones that were skipped are simply looked at on a later run.
    let mut per_owner = listed_per_owner(state, picks);
    let renamed = renamed_splits(heads, curated);
    for (asked, head) in &heads.found {
        if let Some(old) = renamed.get(&head.name.to_ascii_lowercase()) {
            // Read under its new name, it would be one addon again and lose its lines. It keeps
            // what it had, like a repository that couldn't be fetched, until the config follows.
            if asked.eq_ignore_ascii_case(old) {
                note_renamed_split(tally, old, &head.name);
                entries.extend(keep_last_known(state, curated, asked, tally));
            }
            continue;
        }
        let split = split_of(curated, asked, &head.name);
        let excluded = curated.excludes_repo(&head.name);
        if excluded || !seen.insert(head.name.to_ascii_lowercase()) {
            // A second name for a repository already looked at this run goes, such as a seed
            // still naming it as it was before GitHub renamed it. The record under GitHub's own
            // name is the one the first look used, so it stays.
            if excluded || !asked.eq_ignore_ascii_case(&head.name) {
                state.repos.remove(asked);
            }
            answer(tally, asked, split);
            continue;
        }
        if !asked.eq_ignore_ascii_case(&head.name) {
            // Renamed: GitHub answered for the old name, and the new one is looked at below.
            answer(tally, asked, split);
        }
        state.rename_repo(asked, &head.name);
        let record = state.repos.entry(head.name.clone()).or_default();
        record.license.clone_from(&head.license);
        let picked = hand_picked.contains(&head.name.to_ascii_lowercase());
        if let Some(reason) = merge::repo_rejection(head, picked) {
            *tally.rejected.entry(reason.to_string()).or_default() += 1;
            record.entry = None;
            record.releases.clear();
            record.lines.clear();
            answer(tally, &head.name, split);
        } else if needs_full_look(record, head, now)
            || has_unread_lines(record, split)
            || pin_unmet(record, curated, [asked, &head.name])
        {
            if record.entry.is_none() && !picks.contains(&repo_id(&head.name)) {
                let count = per_owner.entry(owner_of(&head.name)).or_default();
                if *count >= merge::MAX_ENTRIES_PER_OWNER {
                    tally.capped += 1;
                    continue;
                }
                *count += 1;
            }
            to_read.push(head.name.clone());
        } else {
            tally.unchanged += 1;
            answer(tally, &head.name, split);
            if let Some(entry) = &mut record.entry {
                entry.addon.stars = rounded_stars(head.stars);
            }
            tally.passed_over.extend(still_passed_over(
                record,
                &format!("gh:{}", head.name),
                curated,
            ));
            entries.extend(listed_entries(record, split));
        }
    }
    (entries, to_read)
}

/// For a repository the quick look found unchanged: its newest release is still another
/// addon's, as the last full look found, so the summary names it on every run, not only on the
/// runs that read it.
fn still_passed_over(record: &RepoState, id: &str, curated: &Curated) -> Option<String> {
    let newest = record.newest_tag.as_deref()?;
    let main = record.releases.get(newest)?.skipped_for.clone()?;
    let entry = record.entry.as_ref()?;
    let pin = Pin {
        main,
        written: curated.main_pin_of(id).is_some(),
    };
    let instead = format!("lists {}", listed_version(entry));
    let unread = record.releases.get(newest).is_some_and(|v| v.unread > 0);
    Some(passed_over(id, &instead, newest, &pin, false, unread))
}

/// Each split repository's releases, prefix by prefix, keyed by the name it was asked for in
/// lower case. A failure stays one, and that repository keeps what it had.
type SplitReleases = BTreeMap<String, Result<PrefixReleases, String>>;

fn fetch_split_releases(http: &Http, batch: &BatchResult, curated: &Curated) -> SplitReleases {
    batch
        .found
        .iter()
        .filter_map(|(asked, info)| {
            // One GitHub renamed since the quick look is kept as it was, so it isn't fetched.
            let split = split_of(curated, asked, &info.name)
                .filter(|split| split.repo.eq_ignore_ascii_case(&info.name))?;
            let fetched = github::fetch_prefix_releases(http, &info.name, &split.prefixes());
            Some((asked.to_ascii_lowercase(), fetched))
        })
        .collect()
}

/// The proper look: every release of the repositories that need one.
#[allow(clippy::too_many_arguments)]
fn read_releases(
    batch: &BatchResult,
    split_releases: &SplitReleases,
    state: &mut State,
    curated: &Curated,
    pins: &Pins<'_>,
    probe: &mut dyn Probe,
    out_of_time: &dyn Fn() -> bool,
    saver: &mut Saver,
    tally: &mut Tally,
) -> Vec<Candidate> {
    tally.gone += u32::try_from(batch.gone.len()).unwrap_or(u32::MAX);
    for name in &batch.gone {
        state.repos.remove(name);
        answer(tally, name, curated.split_of(name));
    }
    let mut entries = Vec::new();
    for name in &batch.failed {
        entries.extend(keep_last_known(state, curated, name, tally));
    }
    let hand_picked = curated.named_repos();
    let total = batch.found.len();
    for (done, (asked, info)) in batch.found.iter().enumerate() {
        if done % 100 == 0 {
            eprintln!("releases: {done} of {total} repositories");
        }
        let split = split_of(curated, asked, &info.name);
        if let Some(split) = split.filter(|split| !split.repo.eq_ignore_ascii_case(&info.name)) {
            // Renamed since the quick look, so it's kept as it was, the same as there.
            note_renamed_split(tally, &split.repo, &info.name);
            entries.extend(keep_last_known(state, curated, asked, tally));
            continue;
        }
        let record = state.repos.entry(asked.clone()).or_default();
        let look = Look {
            asked,
            picked: hand_picked.contains(&asked.to_ascii_lowercase()),
            pins,
            out_of_time,
        };
        match split {
            Some(split) => {
                let fetched = split_releases.get(&asked.to_ascii_lowercase());
                entries.extend(process_split(
                    probe, info, split, fetched, record, &look, tally,
                ));
            }
            None => {
                record.lines.clear();
                entries.extend(process_repo(probe, info, record, &look, tally));
            }
        }
        for name in [asked, &info.name] {
            if !record.incomplete {
                tally.answered.insert(repo_id(name));
            }
            for (line, line_state) in &record.lines {
                if !line_state.incomplete {
                    let id = SplitSpec::line_id(name, line);
                    tally.answered.insert(id.to_ascii_lowercase());
                }
            }
        }
        saver.maybe_save(state);
    }
    entries
}

#[allow(clippy::too_many_arguments)]
fn run_github(
    http: &Http,
    state: &mut State,
    names: &[String],
    curated: &Curated,
    picks: &HandPicked,
    pins: &Pins<'_>,
    out_of_time: &dyn Fn() -> bool,
    saver: &mut Saver,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let heads = github::fetch_heads(http, names);
    eprintln!(
        "quick look: {} repositories found, {} gone, {} failed",
        heads.found.len(),
        heads.gone.len(),
        heads.failed.len()
    );
    let (mut entries, to_read) = triage_heads(&heads, state, curated, picks, unix_now(), tally);
    saver.save(state);

    eprintln!("reading releases of {} repositories", to_read.len());
    let batch = github::fetch_repos(http, &to_read);
    eprintln!(
        "graphql: {} repositories found, {} gone, {} failed",
        batch.found.len(),
        batch.gone.len(),
        batch.failed.len()
    );
    let split_releases = fetch_split_releases(http, &batch, curated);
    let mut probe = LiveProbe { http };
    entries.extend(read_releases(
        &batch,
        &split_releases,
        state,
        curated,
        pins,
        &mut probe,
        out_of_time,
        saver,
        tally,
    ));
    entries
}

fn keep_last_known(
    state: &State,
    curated: &Curated,
    name: &str,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let entries = state
        .repos
        .get(name)
        .map(|record| listed_entries(record, curated.split_of(name)))
        .unwrap_or_default();
    tally.kept_last_known += u32::try_from(entries.len()).unwrap_or(u32::MAX);
    entries
}

fn run_wowi(
    http: &Http,
    state: &mut State,
    seeds: &Seeds,
    limit: Option<usize>,
    out_of_time: &dyn Fn() -> bool,
    saver: &mut Saver,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let rows = match wowi::fetch_rows(http) {
        Ok(rows) => rows,
        Err(problem) => {
            eprintln!("wowinterface: {problem}. Keeping what we knew.");
            return state
                .wowi
                .values()
                .filter_map(|s| s.entry.clone())
                .collect();
        }
    };
    let listed: BTreeSet<u64> = rows.iter().map(|row| row.id).collect();
    let selected = wowi::select_rows(rows, &seeds.wowi, limit);
    eprintln!(
        "wowinterface: checking {} recently updated addons",
        selected.len()
    );

    let mut probe = wowi::LiveProbe { http };
    let entries = scan_wowi_rows(&selected, state, &mut probe, out_of_time, saver, tally);
    // Read at its current version, or no longer on the site at all: both are answers.
    for row in &selected {
        if state
            .wowi
            .get(&row.id)
            .is_some_and(|s| s.version == row.version)
        {
            tally.answered.insert(format!("wowi:{}", row.id));
        }
    }
    for id in state.wowi.keys().filter(|id| !listed.contains(id)) {
        tally.answered.insert(format!("wowi:{id}"));
    }
    state.wowi.retain(|id, _| listed.contains(id));
    entries
}

/// Examine the rows that changed since last time. An unchanged version costs nothing.
fn scan_wowi_rows(
    rows: &[wowi::Row],
    state: &mut State,
    probe: &mut dyn wowi::WowiProbe,
    out_of_time: &dyn Fn() -> bool,
    saver: &mut Saver,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let mut entries = Vec::new();
    let total = rows.len();
    for (done, row) in rows.iter().enumerate() {
        if done % 25 == 0 {
            eprintln!("wowinterface: {done} of {total}");
        }
        if let Some(hit) = state.wowi.get(&row.id).filter(|s| s.version == row.version) {
            entries.extend(hit.entry.as_ref().map(|entry| wowi::refreshed(entry, row)));
            continue;
        }
        let last_known = state.wowi.get(&row.id).and_then(|s| s.entry.as_ref());
        if out_of_time() {
            entries.extend(last_known.map(wowi::without_integrity));
            continue;
        }
        match wowi::examine(row, probe) {
            Ok(entry) => {
                entries.extend(entry.clone());
                state.wowi.insert(
                    row.id,
                    WowiState {
                        version: row.version.clone(),
                        entry,
                    },
                );
            }
            Err(problem) => {
                eprintln!("wowi:{} {}: {problem}", row.id, row.name);
                if let Some(entry) = last_known {
                    tally.kept_last_known += 1;
                    entries.push(wowi::without_integrity(entry));
                }
            }
        }
        saver.maybe_save(state);
    }
    entries
}

fn run_sources(
    http: &Http,
    state: &mut State,
    curated: &Curated,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let mut entries = Vec::new();
    for spec in &curated.source {
        let slot = state.sources.entry(spec.repo.clone()).or_default();
        match curated::process_source(http, spec, slot) {
            Ok((info, Some(build))) => {
                let entry = curated::source_candidate(&info, &build);
                slot.entry = Some(entry.clone());
                entries.push(entry);
            }
            Ok((info, None)) => {
                eprintln!(
                    "{}: no source install: {}",
                    info.name,
                    slot.refused.as_deref().unwrap_or("nothing usable")
                );
                slot.entry = None;
            }
            Err(problem) => {
                eprintln!("{}: {problem}", spec.repo);
                if let Some(entry) = &slot.entry {
                    tally.kept_last_known += 1;
                    entries.push(entry.clone());
                }
            }
        }
    }
    entries
}

fn run_direct(
    http: &Http,
    state: &mut State,
    curated: &Curated,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let mut entries = Vec::new();
    for spec in &curated.direct {
        let mut slot = state.direct.get(&spec.id).cloned();
        match curated::process_direct(http, spec, &mut slot) {
            Ok(entry) => entries.push(entry),
            Err(problem) => {
                eprintln!("{}: {problem}", spec.id);
                if let Some(entry) = slot.as_ref().and_then(|s| s.entry.clone()) {
                    tally.kept_last_known += 1;
                    entries.push(entry);
                }
            }
        }
        if let Some(value) = slot {
            state.direct.insert(spec.id.clone(), value);
        }
    }
    entries
}

/// What the scans came back with.
#[derive(Debug, Default)]
pub struct Scanned {
    /// GitHub repositories, packaged or installed from source.
    pub repos: Vec<Candidate>,
    pub wowi: Vec<Candidate>,
    pub direct: Vec<Candidate>,
}

/// The list that goes to the gate, and what was kept out on the way.
#[derive(Debug, Default)]
pub struct Settled {
    pub candidates: Vec<Candidate>,
    pub held: Vec<Held>,
    pub forks: Vec<String>,
    pub merged: usize,
    /// Former main folders the catalog vouches for from this build on, one line each.
    pub vouched: Vec<String>,
    /// Former main folders it stops vouching for, because an entry with a better claim ships
    /// them.
    pub unvouched: Vec<String>,
    /// GitHub main folders that moved with nothing vouching for the old one.
    pub moved: Vec<String>,
    /// Folders `[pins]` says an entry was that don't fit in its `formerly`.
    pub cut: Vec<String>,
    /// Packs published unreviewed because they're over 100 MB, or their size isn't known.
    pub unreviewed: merge::Unreviewed,
    /// Hand-picked repositories GitHub knows by another name now.
    pub renamed: Vec<String>,
}

/// Everything between the scans and the gate: who merges with whom, who owns which name
/// and folder, and what the app may pick by itself. No network and no clock of its own.
/// `remembered` is what the scan cache remembers of entries `previous` left out, and `now` is
/// the build's time.
pub fn settle(
    scanned: Scanned,
    curated: &Curated,
    picks: &HandPicked,
    previous: Option<&Catalog>,
    remembered: &Remembered,
    now: i64,
) -> Settled {
    let wanted = |candidates: Vec<Candidate>| -> Vec<Candidate> {
        // Before anything else, so a left-out repository can't take a listing down with it.
        candidates
            .into_iter()
            .filter(|c| !curated.excludes_id(&c.addon.id))
            .collect()
    };
    let (repos, forks) = merge::drop_redundant_forks(wanted(scanned.repos), picks);
    let (mut candidates, merged) = merge::merge_wowi(repos, wanted(scanned.wowi), &curated.merges);
    candidates.extend(wanted(scanned.direct));

    // A hand-given name is the one players see, so it's the one that gets claimed.
    for candidate in &mut candidates {
        let edit = curated.overrides.get(&candidate.addon.id);
        if let Some(name) = edit.and_then(|edit| edit.name.as_ref()) {
            candidate.addon.name.clone_from(name);
        }
    }
    let renamed = merge::renamed_picks(&candidates, picks);
    merge::stamp_ages(&mut candidates, previous, remembered, picks, now);
    let reserved = merge::Reserved::new(curated);
    let left_out: HashSet<String> = forks.iter().map(|id| id.to_ascii_lowercase()).collect();
    let rules = merge::ClaimRules {
        picks,
        reserved: &reserved,
        previous,
        remembered,
        curated,
        now,
        left_out: &left_out,
    };
    let (mut candidates, held) = merge::hold_collisions(candidates, &rules);
    merge::settle_trust(&mut candidates, picks, &curated.merges, now);
    let unreviewed = merge::unreview_big_packs(&mut candidates, curated);
    let stamped = merge::stamp_formerly(&mut candidates, previous, remembered, curated, picks);
    finish_candidates(&mut candidates, curated);
    Settled {
        candidates,
        held,
        forks,
        merged,
        vouched: stamped.vouched,
        unvouched: stamped.unvouched,
        moved: stamped.moved,
        cut: stamped.cut,
        unreviewed,
        renamed,
    }
}

/// Apply hand edits and work out categories and tiers.
pub fn finish_candidates(candidates: &mut [Candidate], curated: &Curated) {
    let mut boosts = BTreeMap::new();
    for candidate in candidates.iter_mut() {
        let edit = curated.overrides.get(&candidate.addon.id);
        if let Some(edit) = edit {
            if let Some(name) = &edit.name {
                candidate.addon.name.clone_from(name);
            }
            if let Some(summary) = &edit.summary {
                candidate.addon.summary.clone_from(summary);
            }
            if let Some(boost) = edit.boost {
                boosts.insert(candidate.addon.id.clone(), boost);
            }
        }
        candidate.addon.category = categorize(candidate, edit);
    }
    assign_tiers(candidates, &boosts);
}

/// Put everything in its published form. Sorted by id, so unchanged addons stay in
/// place and the file compresses well.
pub fn assemble(mut candidates: Vec<Candidate>, curated: &Curated) -> Catalog {
    candidates.sort_by(|a, b| a.addon.id.cmp(&b.addon.id));
    let addons = candidates
        .into_iter()
        .map(|candidate| {
            let mut addon = candidate.addon;
            addon.downloads = round_sig(addon.downloads, COUNT_DIGITS);
            addon.stars = rounded_stars(addon.stars);
            addon
        })
        .collect();
    Catalog {
        schema: CATALOG_SCHEMA,
        generated: unix_now(),
        addons,
        featured: curated.featured.clone(),
        kits: curated.kits.clone(),
        gaps: curated
            .gaps
            .iter()
            .map(|g| Gap {
                name: g.name.clone(),
                reason: g.reason.clone(),
                website: g.website.clone(),
                alternatives: g.alternatives.clone(),
            })
            .collect(),
    }
}

/// The last published catalog, which is where every entry's age comes from. Losing that
/// would make everything brand new at once, so a file that's there but can't be read stops
/// the build rather than start over quietly, unless a maintainer says to start over.
fn load_previous(path: Option<&Path>, accept_smaller: bool) -> Result<Option<Catalog>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let read = std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice::<Catalog>(&bytes).map_err(|e| e.to_string()));
    match read {
        Ok(catalog) => Ok(Some(catalog)),
        Err(problem) if accept_smaller => {
            eprintln!(
                "{}: {problem}. Starting every addon's age over, as asked.",
                path.display()
            );
            Ok(None)
        }
        Err(problem) => Err(format!(
            "{}: {problem}. Addons' ages come from this file, so nothing is being published. \
             Running the workflow with accept_smaller starts them over.",
            path.display()
        )),
    }
}

/// The repositories taking the most room in the scan cache, largest first.
fn largest_repos(state: &State, count: usize) -> Vec<(usize, String)> {
    let mut sizes: Vec<(usize, String)> = state
        .repos
        .iter()
        .map(|(name, record)| {
            let size = serde_json::to_vec(record).map_or(0, |bytes| bytes.len());
            (size, name.clone())
        })
        .collect();
    sizes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    sizes.truncate(count);
    sizes
}

/// A scan cache this big won't go through a push. Whoever looks at the failed run needs
/// to know which repositories to exclude, so name them.
fn warn_about_a_huge_state(state: &State, path: &Path) {
    let size = std::fs::metadata(path).map_or(0, |meta| meta.len());
    if size > MAX_STATE_BYTES {
        eprintln!(
            "warning: {} is {size} bytes, too big to publish, so the previous copy stays. Largest repositories:",
            path.display()
        );
        for (bytes, name) in largest_repos(state, 10) {
            eprintln!("  {name}: {bytes} bytes");
        }
    }
}

fn gzipped_len(bytes: &[u8]) -> Result<usize, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).map_err(|e| e.to_string())?;
    Ok(encoder.finish().map_err(|e| e.to_string())?.len())
}

/// Prints a heading with a count, then one line per item, when there are any.
fn print_list(heading: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    println!("{heading}: {}", lines.len());
    for line in lines {
        println!("    {line}");
    }
}

fn print_summary(
    catalog: &Catalog,
    raw: usize,
    gz: usize,
    merged: usize,
    tally: &Tally,
    http: &Http,
    took: Duration,
) {
    let by_source =
        |want: fn(&Source) -> bool| catalog.addons.iter().filter(|a| want(&a.source)).count();
    println!("addons: {}", catalog.addons.len());
    println!(
        "  by source: github {} (of which source installs {}), wowinterface {}, direct {}",
        by_source(|s| matches!(s, Source::Github { .. })),
        catalog
            .addons
            .iter()
            .filter(|a| a.stable.as_ref().is_some_and(|r| !r.moves.is_empty()))
            .count(),
        by_source(|s| matches!(s, Source::Wowi { .. })),
        by_source(|s| matches!(s, Source::Direct { .. })),
    );
    println!("  merged github and wowinterface entries: {merged}");
    println!(
        "  held out of the catalog: {}, see catalog-held.json",
        tally.held
    );
    println!(
        "  new repositories not read because their owner has plenty already: {}",
        tally.capped
    );
    println!("  by category:");
    for (id, _) in CATEGORIES {
        let count = catalog.addons.iter().filter(|a| a.category == id).count();
        println!("    {:<28} {count}", category_name(id));
    }
    let tier = |t: u8| catalog.addons.iter().filter(|a| a.tier == t).count();
    println!(
        "  by tier: legendary {}, epic {}, rare {}, uncommon {}, common {}, grey {}",
        tier(5),
        tier(4),
        tier(3),
        tier(2),
        tier(1),
        tier(0)
    );
    println!(
        "  featured {}, kits {}, gaps {}",
        catalog.featured.len(),
        catalog.kits.len(),
        catalog.gaps.len()
    );
    println!("catalog.json: {raw} bytes raw, {gz} bytes gzipped");
    println!(
        "skipped repositories: {} without a Forever build, {} gone, {} placeholders, rejected {:?}",
        tally.no_build, tally.gone, tally.placeholder, tally.rejected
    );
    println!(
        "kept last-known results for {} that failed this run, {} repositories unchanged since last time",
        tally.kept_last_known, tally.unchanged
    );
    print_list(
        "warning: [[split]] repositories GitHub has renamed, kept as they were",
        &tally.renamed_splits,
    );
    print_list(
        "hand-picked repositories GitHub knows by another name now",
        &tally.renamed,
    );
    print_list(
        "passed over newer releases that are another addon's",
        &tally.passed_over,
    );
    print_list(
        "releases passed over with zips left unread",
        &tally.left_unread,
    );
    print_list("former main folders vouched for", &tally.vouched);
    print_list(
        "former main folders no longer vouched for",
        &tally.unvouched,
    );
    print_list(
        "main folders that moved, the old one not vouched for",
        &tally.moved,
    );
    print_list("former main folders in [pins] that don't fit", &tally.cut);
    print_list(
        "packs published unreviewed, as they're over 100 MB",
        &tally.unreviewed.big,
    );
    print_list(
        "packs published unreviewed, as their size isn't known",
        &tally.unreviewed.unknown_size,
    );
    println!("requests:");
    for (channel, count) in http.counts() {
        println!("    {:<28} {count}", channel.label());
    }
    println!(
        "downloaded {} bytes in {:.0} seconds",
        http.bytes(),
        took.as_secs_f64()
    );
}

pub fn run(options: &Options) -> Result<(), String> {
    let started = Instant::now();
    let deadline = options
        .max_minutes
        .map(|minutes| started + Duration::from_secs(minutes * 60));
    let out_of_time = move || deadline.is_some_and(|d| Instant::now() >= d);

    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    let http = Http::new(token);
    if !http.has_token() {
        return Err(
            "GITHUB_TOKEN isn't set. GitHub's GraphQL API needs a token, so set it and run again."
                .to_string(),
        );
    }
    let seeds = curated::load_seeds(&options.seeds)?;
    let curated = curated::load_curated(&options.curated)?;
    if let Some(problem) = curated.unseeded_split(&seeds) {
        return Err(format!(
            "{} has a mistake in it: {problem}",
            options.curated.display()
        ));
    }
    let picks = HandPicked::new(&seeds, &curated);
    // Ages come from the last catalog, and so does which addon each entry is, so it's read
    // before anything is fetched. Nothing is lost by stopping here.
    let previous = load_previous(options.previous.as_deref(), options.accept_smaller)?;
    std::fs::create_dir_all(&options.out)
        .map_err(|e| format!("Couldn't create {}: {e}", options.out.display()))?;

    let state_out = options.out.join("catalog-state.json");
    let mut state = State::load(options.state.as_deref().unwrap_or(&state_out));
    let pins = Pins::new(previous.as_ref(), &curated).remembering(&state.remembered);
    let mut saver = Saver {
        path: state_out.clone(),
        last: Instant::now(),
    };
    let mut tally = Tally::default();

    discover(&http, &mut state, &seeds, &curated, options.limit.is_none());
    saver.save(&state);

    let names = repo_order(&state, &seeds, &curated, options.limit);
    eprintln!("checking {} GitHub repositories", names.len());
    let mut packaged = run_github(
        &http,
        &mut state,
        &names,
        &curated,
        &picks,
        &pins,
        &out_of_time,
        &mut saver,
        &mut tally,
    );
    saver.save(&state);

    let wowi_entries = if options.no_wowi {
        Vec::new()
    } else {
        run_wowi(
            &http,
            &mut state,
            &seeds,
            options.wowi_limit,
            &out_of_time,
            &mut saver,
            &mut tally,
        )
    };
    saver.save(&state);

    let sources = run_sources(&http, &mut state, &curated, &mut tally);
    let direct = run_direct(&http, &mut state, &curated, &mut tally);
    saver.save(&state);
    warn_about_a_huge_state(&state, &state_out);

    // A repository that started publishing real packages doesn't need its source install any more.
    let packaged_ids: BTreeSet<String> = packaged
        .iter()
        .map(|c| c.addon.id.to_ascii_lowercase())
        .collect();
    packaged.extend(
        sources
            .into_iter()
            .filter(|c| !packaged_ids.contains(&c.addon.id.to_ascii_lowercase())),
    );

    let scanned = Scanned {
        repos: packaged,
        wowi: wowi_entries,
        direct,
    };
    let settled = settle(
        scanned,
        &curated,
        &picks,
        previous.as_ref(),
        &state.remembered,
        unix_now(),
    );
    for id in &settled.forks {
        eprintln!(
            "{id}: dropped, a fork with the same folders as its parent or a better-known sibling"
        );
    }
    tally.vouched = settled.vouched;
    tally.unvouched = settled.unvouched;
    tally.moved = settled.moved;
    tally.cut = settled.cut;
    tally.unreviewed = settled.unreviewed;
    tally.renamed = settled.renamed;
    let published = publish(
        settled.candidates,
        settled.held,
        &GateContext {
            curated: &curated,
            picks: &picks,
            previous: previous.as_ref(),
            answered: &tally.answered,
            accept_smaller: options.accept_smaller,
        },
        &options.out,
    )?;
    // Only a catalog that went out says what an entry was: a refused one changes nothing.
    state.remembered.note(&published.catalog);
    saver.save(&state);
    tally.held = published.held.len();
    print_summary(
        &published.catalog,
        published.raw,
        published.gz,
        settled.merged,
        &tally,
        &http,
        started.elapsed(),
    );
    Ok(())
}

/// What went out.
#[derive(Debug)]
pub struct Published {
    pub catalog: Catalog,
    pub raw: usize,
    pub gz: usize,
    pub held: Vec<Held>,
}

/// `catalog-held.json`, so a maintainer can see what's being kept out and why. Written even
/// when the gate refuses the catalog, because that's when someone is looking.
fn write_held(out: &Path, held: &[Held]) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct HeldFile<'a> {
        held: &'a [Held],
    }
    for entry in held {
        eprintln!("{}: held out of the catalog, {}", entry.id, entry.reason);
    }
    let bytes = serde_json::to_vec_pretty(&HeldFile { held })
        .map_err(|e| format!("Couldn't encode the held list: {e}"))?;
    write_atomic(&out.join("catalog-held.json"), &bytes)
}

/// Check the finished list and write `catalog.json` and `catalog-held.json`. When the gate
/// refuses, no catalog is written and whatever was published before stays as it is.
pub fn publish(
    candidates: Vec<Candidate>,
    mut held: Vec<Held>,
    context: &GateContext<'_>,
    out: &Path,
) -> Result<Published, String> {
    let catalog = assemble(candidates, context.curated);
    let passed = match run_gate(catalog, context) {
        Ok(passed) => passed,
        Err(problem) => {
            write_held(out, &held)?;
            return Err(problem);
        }
    };
    held.extend(passed.held);
    held.sort_by(|a, b| a.id.cmp(&b.id));
    write_held(out, &held)?;
    for warning in &passed.warnings {
        eprintln!("warning: {warning}");
    }
    let catalog = passed.catalog;
    let bytes =
        serde_json::to_vec(&catalog).map_err(|e| format!("Couldn't encode the catalog: {e}"))?;
    write_atomic(&out.join("catalog.json"), &bytes)?;
    let gz = gzipped_len(&bytes)?;
    Ok(Published {
        catalog,
        raw: bytes.len(),
        gz,
        held,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use feeling_core::catalog::{Addon, Kit};

    use super::*;
    use crate::curated::{GapSpec, Override};
    use crate::detect::GhRelease;
    use crate::state::LineState;

    fn scan(title: Option<&str>) -> ZipScan {
        ZipScan {
            folders: vec!["Questie".into(), "QuestieDB".into()],
            forever: true,
            title: title.map(str::to_string),
            notes: Some("Shows quests".into()),
            author: Some("Aero".into()),
            deps: vec!["QuestieDB".into(), "Ace3".into()],
            wowi: Some("@wowi-id@".into()),
            curse: Some("334372".into()),
            ..ZipScan::default()
        }
    }

    fn build(tag: &str, url: &str) -> Build {
        Build {
            tag: tag.into(),
            published: 1000,
            asset: GhAsset {
                name: "Questie.zip".into(),
                size: 93_581_472,
                downloads: 5,
                url: url.into(),
                sha256: Some("a".repeat(64)),
            },
            scan: scan(Some("Questie [Forever]")),
        }
    }

    fn info() -> RepoInfo {
        RepoInfo {
            name: "Questie/Questie".into(),
            description: Some("Repo text".into()),
            stars: 800,
            topics: vec!["wow-addon".into()],
            releases: vec![GhRelease {
                tag: "v1".into(),
                assets: vec![
                    GhAsset {
                        name: "a.zip".into(),
                        downloads: 100,
                        ..GhAsset::default()
                    },
                    GhAsset {
                        name: "b.ZIP".into(),
                        downloads: 50,
                        ..GhAsset::default()
                    },
                    GhAsset {
                        name: "release.json".into(),
                        downloads: 99_999,
                        ..GhAsset::default()
                    },
                ],
                ..GhRelease::default()
            }],
            ..RepoInfo::default()
        }
    }

    #[test]
    fn github_entries_are_filled_from_the_toc_then_the_repo() {
        let chosen = Chosen {
            stable: Some(build("bundle/v12.6.2+v1.0.11", "https://github.com/a.zip")),
            beta: Some(build("v13-beta", "https://github.com/b.zip")),
            ..Chosen::default()
        };
        let c = github_candidate(&info(), &chosen, 0).unwrap();
        let addon = &c.addon;
        assert_eq!(addon.id, "gh:Questie/Questie");
        assert_eq!(addon.name, "Questie");
        assert_eq!(addon.summary, "Shows quests");
        assert_eq!(addon.author, "Aero");
        assert_eq!(addon.folders, ["Questie", "QuestieDB"]);
        assert_eq!(
            addon.deps,
            ["Ace3"],
            "folders in the same zip aren't dependencies"
        );
        assert_eq!(
            addon.downloads, 150,
            "only zips count, not release.json fetches"
        );
        assert_eq!(addon.stars, 800);
        let bigger = github_candidate(
            &RepoInfo {
                stars: 1234,
                ..info()
            },
            &chosen,
            4_727_895,
        )
        .unwrap();
        assert_eq!(
            bigger.addon.stars, 1230,
            "counts are rounded so small changes don't churn"
        );
        assert_eq!(bigger.addon.downloads, 4_720_000);
        assert_eq!(addon.ids.curse.as_deref(), Some("334372"));
        assert_eq!(addon.ids.wowi, None, "an unexpanded token isn't an id");
        let stable = addon.stable.as_ref().unwrap();
        assert_eq!(stable.version, "v12.6.2+v1.0.11");
        assert_eq!(stable.url, "https://github.com/a.zip");
        assert_eq!(stable.size, 93_581_472);
        assert_eq!(stable.sha256, Some("a".repeat(64)));
        assert_eq!(addon.beta.as_ref().unwrap().version, "v13-beta");
        assert!(stable.moves.is_empty());
    }

    #[test]
    fn missing_toc_fields_fall_back_to_the_repository() {
        let mut stable = build("v1", "https://github.com/a.zip");
        stable.scan = ZipScan {
            folders: vec!["Questie".into()],
            forever: true,
            ..ZipScan::default()
        };
        let chosen = Chosen {
            stable: Some(stable),
            ..Chosen::default()
        };
        let c = github_candidate(&info(), &chosen, 7).unwrap();
        assert_eq!(c.addon.name, "Questie", "the folder, not the repo");
        assert_eq!(c.addon.summary, "Repo text");
        assert_eq!(c.addon.author, "Questie", "the owner");

        let mut plain = build("v1", "https://github.com/a.zip");
        plain.scan = ZipScan {
            title: Some("Basic Chat Mods".into()),
            notes: Some("Basic Chat Mods".into()),
            ..plain.scan
        };
        let repeat = Chosen {
            stable: Some(plain),
            ..Chosen::default()
        };
        let c2 = github_candidate(&info(), &repeat, 0).unwrap();
        assert_eq!(
            c2.addon.summary, "Repo text",
            "a note repeating the name adds nothing"
        );
        assert_eq!(c.addon.downloads, 150);

        let bigger = github_candidate(&info(), &chosen, 10_000).unwrap();
        assert_eq!(
            bigger.addon.downloads, 10_000,
            "strongbox has seen older releases"
        );
    }

    #[test]
    fn nothing_found_means_no_entry() {
        assert_eq!(github_candidate(&info(), &Chosen::default(), 0), None);
    }

    #[test]
    fn a_beta_only_addon_gets_an_entry() {
        let chosen = Chosen {
            beta: Some(build("v1-beta", "https://github.com/a.zip")),
            ..Chosen::default()
        };
        let c = github_candidate(&info(), &chosen, 0).unwrap();
        assert!(c.addon.stable.is_none());
        assert!(c.addon.beta.is_some());
    }

    #[test]
    fn a_fork_without_a_visible_parent_is_still_a_fork() {
        let chosen = Chosen {
            stable: Some(build("v1", "https://github.com/a.zip")),
            ..Chosen::default()
        };
        let fork = RepoInfo {
            is_fork: true,
            fork_of: None,
            ..info()
        };
        assert_eq!(
            github_candidate(&fork, &chosen, 0)
                .unwrap()
                .fork_of
                .as_deref(),
            Some("")
        );
        let original = github_candidate(&info(), &chosen, 0).unwrap();
        assert_eq!(original.fork_of, None);
    }

    #[test]
    fn a_previous_entry_only_goes_grey_while_its_file_can_exist() {
        let mut previous = Candidate::default();
        previous.addon.stable = Some(Release {
            url: "https://github.com/a.zip".into(),
            ..Release::default()
        });

        let mut listed = info();
        listed.releases[0].assets[0].url = "https://github.com/a.zip".into();
        assert!(grey_previous(&previous, &listed).unwrap().no_longer_forever);

        assert_eq!(
            grey_previous(&previous, &info()),
            None,
            "the release was deleted"
        );

        let mut long_list = info();
        long_list.releases = vec![GhRelease::default(); RELEASES_FETCHED];
        assert!(
            grey_previous(&previous, &long_list).is_some(),
            "it may just be older than the list"
        );
    }

    fn state_with(repos: &[(&str, u64)]) -> State {
        let mut state = State::empty();
        for (name, downloads) in repos {
            state.repos.insert(
                (*name).to_string(),
                RepoState {
                    known_downloads: *downloads,
                    ..RepoState::default()
                },
            );
        }
        state
    }

    fn head(tag: Option<&str>) -> RepoInfo {
        RepoInfo {
            name: "a/b".into(),
            releases: tag
                .map(|t| GhRelease {
                    tag: t.into(),
                    ..GhRelease::default()
                })
                .into_iter()
                .collect(),
            ..RepoInfo::default()
        }
    }

    #[test]
    fn a_full_look_is_only_needed_when_something_can_have_changed() {
        let now = 10_000_000;
        let recent = RepoState {
            newest_tag: Some("v1".into()),
            looked_at: now - 60,
            ..RepoState::default()
        };
        assert!(
            !needs_full_look(&recent, &head(Some("v1")), now),
            "same tag, looked at recently"
        );
        assert!(
            needs_full_look(&recent, &head(Some("v2")), now),
            "a new tag"
        );
        assert!(
            needs_full_look(&recent, &head(None), now),
            "the releases are gone"
        );

        let new_repo = RepoState::default();
        assert!(
            needs_full_look(&new_repo, &head(Some("v1")), now),
            "never looked at"
        );
        assert!(
            !needs_full_look(&new_repo, &head(None), now),
            "no releases, nothing to read"
        );

        let failed = RepoState {
            incomplete: true,
            ..recent.clone()
        };
        assert!(
            needs_full_look(&failed, &head(Some("v1")), now),
            "last time went wrong"
        );

        let listed = RepoState {
            entry: Some(Candidate::default()),
            looked_at: now - RELOOK_LISTED_SECS - 1,
            ..recent.clone()
        };
        assert!(
            needs_full_look(&listed, &head(Some("v1")), now),
            "listed addons get refreshed daily"
        );
        let other = RepoState {
            looked_at: now - RELOOK_LISTED_SECS - 1,
            ..recent.clone()
        };
        assert!(
            !needs_full_look(&other, &head(Some("v1")), now),
            "the rest less often"
        );
        let old = RepoState {
            looked_at: now - RELOOK_OTHERS_SECS - 1,
            ..recent
        };
        assert!(needs_full_look(&old, &head(Some("v1")), now));
    }

    #[test]
    fn a_renamed_repository_with_no_releases_left_gets_looked_at_instead_of_staying_listed() {
        let now = 10_000_000;
        let mut state = State::empty();
        let old = state.note_repo("stranger/TomTom", "topic");
        old.entry = Some(Candidate::default());
        old.newest_tag = Some("v1".into());
        old.looked_at = now - 60;

        // Both happen between two runs: the name changes and the releases are deleted.
        state.rename_repo("stranger/TomTom", "stranger/Gone");
        let moved = &state.repos["stranger/Gone"];
        assert!(moved.entry.is_some());
        assert_eq!(
            moved.newest_tag, None,
            "the record forgets its tag, so the comparison alone can't see it"
        );
        assert!(needs_full_look(moved, &head(None), now));

        // Nothing listed and nothing to read stays free, however often it's asked.
        assert!(!needs_full_look(&RepoState::default(), &head(None), now));
    }

    #[test]
    fn rolling_tags_are_always_looked_at_and_versioned_by_their_file() {
        let now = 10_000_000;
        let record = RepoState {
            newest_tag: Some("latest".into()),
            looked_at: now - 60,
            ..RepoState::default()
        };
        assert!(needs_full_look(&record, &head(Some("latest")), now));

        let rolling = Chosen {
            stable: Some(build("latest", "https://github.com/a.zip")),
            ..Chosen::default()
        };
        let c = github_candidate(&info(), &rolling, 0).unwrap();
        let version = c.addon.stable.unwrap().version;
        assert_eq!(version, format!("latest-{}", "a".repeat(7)));
    }

    #[test]
    fn repositories_are_checked_seeds_first_then_most_downloaded() {
        let state = state_with(&[
            ("a/small", 1),
            ("b/big", 100),
            ("c/seeded", 0),
            ("bad name", 999),
            ("mirror/x", 50),
        ]);
        let seeds = Seeds {
            repos: vec!["c/seeded".into(), "d/new-seed".into()],
            wowi: vec![],
        };
        let curated = Curated {
            exclude: crate::curated::Exclude {
                owners: vec!["mirror".into()],
                ..Default::default()
            },
            ..Curated::default()
        };
        let order = repo_order(&state, &seeds, &curated, None);
        assert_eq!(order, ["c/seeded", "d/new-seed", "b/big", "a/small"]);
        assert_eq!(
            repo_order(&state, &seeds, &curated, Some(3)),
            ["c/seeded", "d/new-seed", "b/big"]
        );
    }

    #[test]
    fn names_differing_only_by_case_are_checked_once() {
        let state = state_with(&[("Foo/Bar", 5), ("foo/bar", 1)]);
        let order = repo_order(&state, &Seeds::default(), &Curated::default(), None);
        assert_eq!(order, ["Foo/Bar"]);
    }

    fn addon_candidate(id: &str, name: &str, downloads: u64) -> Candidate {
        Candidate {
            addon: Addon {
                id: id.into(),
                name: name.into(),
                folders: vec![name.replace(' ', "")],
                downloads,
                stable: Some(Release {
                    version: "1".into(),
                    url: "https://github.com/x.zip".into(),
                    ..Release::default()
                }),
                ..Addon::default()
            },
            ..Candidate::default()
        }
    }

    #[test]
    fn hand_edits_apply_before_categories_and_tiers() {
        let mut list = vec![
            addon_candidate("gh:a/x", "x thing", 5),
            addon_candidate("gh:a/y", "Questie", 500),
        ];
        for candidate in &mut list {
            candidate.addon.trusted = true;
        }
        let curated = Curated {
            overrides: BTreeMap::from([(
                "gh:a/x".to_string(),
                Override {
                    name: Some("Pretty Name".into()),
                    summary: Some("Hand written".into()),
                    category: Some("pvp".into()),
                    boost: Some(9.0),
                },
            )]),
            ..Curated::default()
        };
        finish_candidates(&mut list, &curated);
        assert_eq!(list[0].addon.name, "Pretty Name");
        assert_eq!(list[0].addon.summary, "Hand written");
        assert_eq!(list[0].addon.category, "pvp");
        assert_eq!(list[1].addon.category, "quests");
        assert!(
            list.iter().all(|c| c.addon.tier == 5),
            "with so few addons all are legendary"
        );
    }

    #[test]
    fn assembling_sorts_rounds_and_carries_the_hand_written_lists() {
        let curated = Curated {
            featured: vec!["gh:a/y".into()],
            kits: vec![Kit {
                name: "K".into(),
                description: "D".into(),
                addons: vec!["gh:a/y".into()],
            }],
            gaps: vec![GapSpec {
                name: "Gap".into(),
                reason: "Why.".into(),
                website: Some("https://x.test".into()),
                folders: vec!["GapFolder".into()],
                alternatives: vec!["gh:a/y".into()],
                unrelated: vec!["gh:a/z".into()],
            }],
            ..Curated::default()
        };
        let list = vec![
            addon_candidate("wowi:1", "B", 1_234_567),
            addon_candidate("gh:a/y", "A", 42),
        ];
        let catalog = assemble(list, &curated);
        assert_eq!(catalog.schema, CATALOG_SCHEMA);
        assert_eq!(catalog.addons[0].id, "gh:a/y");
        assert_eq!(catalog.addons[1].downloads, 1_230_000);
        assert_eq!(catalog.addons[0].downloads, 42);
        assert_eq!(catalog.featured.len(), 1);
        assert_eq!(catalog.kits.len(), 1);
        assert_eq!(catalog.gaps[0].website.as_deref(), Some("https://x.test"));
        let json = serde_json::to_string(&catalog).unwrap();
        assert!(
            !json.contains("unrelated") && !json.contains("GapFolder"),
            "our bookkeeping isn't published"
        );
        assert!(!json.contains('\n'), "the file is minified");
    }

    fn publishable(id: &str) -> Candidate {
        let mut c = addon_candidate(id, "Some Addon", 10);
        c.addon.category = "misc".into();
        c
    }

    fn publish_plainly(
        candidates: Vec<Candidate>,
        held: Vec<Held>,
        previous: Option<&Catalog>,
        out: &Path,
    ) -> Result<Published, String> {
        publish(
            candidates,
            held,
            &GateContext {
                curated: &Curated::default(),
                picks: &HandPicked::default(),
                previous,
                answered: &BTreeSet::new(),
                accept_smaller: false,
            },
            out,
        )
    }

    #[test]
    fn publishing_writes_a_minified_catalog_the_app_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let published =
            publish_plainly(vec![publishable("gh:a/b")], vec![], None, dir.path()).unwrap();
        assert_eq!(published.catalog.addons.len(), 1);
        let bytes = std::fs::read(dir.path().join("catalog.json")).unwrap();
        assert_eq!(bytes.len(), published.raw);
        assert!(published.gz > 0);
        assert!(!bytes.contains(&b'\n'));
        let back: Catalog = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.schema, CATALOG_SCHEMA);
        assert_eq!(back.addons[0].id, "gh:a/b");
    }

    fn held_file(out: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(out.join("catalog-held.json")).unwrap()).unwrap()
    }

    #[test]
    fn what_was_held_is_published_with_its_reason_and_nothing_that_churns() {
        let dir = tempfile::tempdir().unwrap();
        let held = vec![Held::new(
            &addon_candidate("gh:z/Copy", "Copy", 1).addon,
            "its name is already taken by gh:a/b".to_string(),
        )];
        publish_plainly(vec![publishable("gh:a/b")], held, None, dir.path()).unwrap();
        let file = held_file(dir.path());
        assert_eq!(file["held"][0]["id"], "gh:z/Copy");
        assert_eq!(file["held"][0]["name"], "Copy");
        assert_eq!(file["held"][0]["folders"][0], "Copy");
        assert!(
            file["held"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("already taken")
        );
        let text = std::fs::read_to_string(dir.path().join("catalog-held.json")).unwrap();
        assert!(!text.contains("generated"), "same held list, same bytes");

        publish_plainly(vec![publishable("gh:a/b")], vec![], None, dir.path()).unwrap();
        assert_eq!(
            held_file(dir.path())["held"].as_array().unwrap().len(),
            0,
            "an empty list replaces a stale one"
        );
    }

    #[test]
    fn a_refused_catalog_leaves_the_published_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let many: Vec<Addon> = (0..10)
            .map(|i| Addon {
                id: format!("gh:a/{i}"),
                ..Addon::default()
            })
            .collect();
        let old = Catalog {
            schema: CATALOG_SCHEMA,
            addons: many,
            ..Catalog::default()
        };
        let published = dir.path().join("catalog.json");
        std::fs::write(&published, b"the old catalog").unwrap();

        let held = vec![Held::new(
            &addon_candidate("gh:z/Copy", "Copy", 1).addon,
            "why".to_string(),
        )];
        let result = publish_plainly(vec![publishable("gh:a/b")], held, Some(&old), dir.path());
        assert!(result.unwrap_err().contains("Not publishing"));
        assert_eq!(std::fs::read(&published).unwrap(), b"the old catalog");
        assert_eq!(
            held_file(dir.path())["held"][0]["id"],
            "gh:z/Copy",
            "someone is going to look at this run"
        );
    }

    fn real_entry(repo: &str, name: &str, folders: &[&str], downloads: u64) -> Candidate {
        let mut c = addon_candidate(&format!("gh:{repo}"), name, downloads);
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Github { repo: repo.into() };
        c
    }

    fn listing_entry(id: u64, name: &str, folders: &[&str], downloads: u64) -> Candidate {
        let mut c = addon_candidate(&format!("wowi:{id}"), name, downloads);
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Wowi { id };
        c.addon.ids.wowi = Some(id.to_string());
        c
    }

    /// A small world: a few real addons, the hand-picked set, and a catalog published
    /// before this code ran.
    struct World {
        seeds: Seeds,
        curated: Curated,
        repos: Vec<Candidate>,
        wowi: Vec<Candidate>,
        previous: Catalog,
        /// What the scan cache remembers of each entry's last published catalog.
        remembered: Remembered,
        /// When the next build runs.
        now: i64,
    }

    const BUILT_AT: i64 = 1_800_000_000;

    fn world() -> World {
        let curated = Curated {
            gaps: vec![
                GapSpec {
                    name: "ElvUI".into(),
                    reason: "Why.".into(),
                    folders: vec!["ElvUI".into(), "ElvUI_Options".into()],
                    ..GapSpec::default()
                },
                GapSpec {
                    name: "Leatrix Plus".into(),
                    reason: "Why.".into(),
                    folders: vec!["Leatrix_Plus".into()],
                    ..GapSpec::default()
                },
            ],
            merges: BTreeMap::from([("10783".to_string(), "Legacy/prat-3-0".to_string())]),
            overrides: BTreeMap::from([(
                "gh:Tercioo/Details".to_string(),
                Override {
                    name: Some("Details! Damage Meter".into()),
                    ..Override::default()
                },
            )]),
            ..Curated::default()
        };
        let seeds = Seeds {
            repos: vec!["Questie/Questie".into(), "Legacy/prat-3-0".into()],
            wowi: vec![7032],
        };
        let mut prat = real_entry("Legacy/prat-3-0", "Prat 3.0", &["Prat-3.0"], 610_000);
        prat.fork_of = Some("sylvanaar/prat-3-0".into());
        prat.addon.ids.wowi = Some("10783".into());
        let repos = vec![
            real_entry(
                "Questie/Questie",
                "Questie",
                &["Questie", "QuestieDB"],
                5_000_000,
            ),
            real_entry("Tercioo/Details", "Details", &["Details"], 0),
            prat,
            real_entry(
                "Peavers/PeaversCommons",
                "PeaversCommons",
                &["PeaversCommons"],
                40,
            ),
            real_entry("zebra/Helpers", "Helpers", &["Helpers"], 3),
        ];
        let wowi = vec![
            listing_entry(7032, "TomTom", &["TomTom"], 3_550_000),
            listing_entry(10783, "Prat 3.0", &["Prat-3.0"], 610_433),
        ];
        // Published by the old code: no ages, and the Prat listing already folded in.
        let previous = Catalog {
            schema: CATALOG_SCHEMA,
            addons: repos
                .iter()
                .chain(wowi.first())
                .map(|c| c.addon.clone())
                .collect(),
            ..Catalog::default()
        };
        World {
            seeds,
            curated,
            repos,
            wowi,
            previous,
            remembered: Remembered::default(),
            now: BUILT_AT,
        }
    }

    impl World {
        /// The settle step of one build with these extra repositories in the scan results.
        fn settle(&self, extra: Vec<Candidate>) -> Settled {
            let picks = HandPicked::new(&self.seeds, &self.curated);
            let mut repos = self.repos.clone();
            repos.extend(extra);
            let scanned = Scanned {
                repos,
                wowi: self.wowi.clone(),
                direct: vec![],
            };
            settle(
                scanned,
                &self.curated,
                &picks,
                Some(&self.previous),
                &self.remembered,
                self.now,
            )
        }

        /// One build with these extra repositories in the scan results.
        fn build(&self, extra: Vec<Candidate>) -> (Published, tempfile::TempDir) {
            let picks = HandPicked::new(&self.seeds, &self.curated);
            let settled = self.settle(extra);
            // The settle step leaves the gate no vouch to drop.
            let settled_addons: Vec<Addon> =
                settled.candidates.iter().map(|c| c.addon.clone()).collect();
            let unsettled = crate::gate::formerly_shipped_elsewhere(&settled_addons);
            assert!(unsettled.is_empty(), "{unsettled:?}");
            let dir = tempfile::tempdir().unwrap();
            let published = publish(
                settled.candidates,
                settled.held,
                &GateContext {
                    curated: &self.curated,
                    picks: &picks,
                    previous: Some(&self.previous),
                    answered: &BTreeSet::new(),
                    accept_smaller: false,
                },
                dir.path(),
            )
            .unwrap();
            // What the catalog check holds every published catalog to.
            let elsewhere = crate::gate::formerly_shipped_elsewhere(&published.catalog.addons);
            assert!(elsewhere.is_empty(), "{elsewhere:?}");
            (published, dir)
        }

        /// The next build reads this one's catalog as the last, made at this build's time, and the
        /// scan cache remembers it.
        fn after(&mut self, published: &Published) {
            let mut catalog = published.catalog.clone();
            catalog.generated = self.now;
            self.remembered.note(&catalog);
            self.previous = catalog;
        }
    }

    fn addon_of<'a>(published: &'a Published, id: &str) -> &'a Addon {
        published
            .catalog
            .addons
            .iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("{id} isn't in the catalog"))
    }

    fn is_held(published: &Published, id: &str) -> bool {
        published.held.iter().any(|h| h.id == id)
    }

    fn claim_of(repo: &str, name: &str, folders: &[&str], downloads: u64) -> Candidate {
        real_entry(repo, name, folders, downloads)
    }

    #[test]
    fn with_nobody_attacking_a_build_keeps_what_was_listed_and_makes_it_established() {
        let (published, _dir) = world().build(vec![]);
        assert!(published.held.is_empty(), "{:?}", published.held);
        assert_eq!(published.catalog.addons.len(), 6);
        for addon in &published.catalog.addons {
            assert_eq!(addon.created, merge::BASELINE_CREATED, "{}", addon.id);
            assert!(addon.trusted, "{}", addon.id);
        }
        let prat = addon_of(&published, "gh:Legacy/prat-3-0");
        assert_eq!(prat.downloads, 1_220_000, "the approved pair adds up");
        assert_eq!(prat.ids.wowi.as_deref(), Some("10783"));
        assert_eq!(
            addon_of(&published, "gh:Tercioo/Details").name,
            "Details! Damage Meter"
        );
    }

    #[test]
    fn a_stranger_taking_tomtom_is_held_with_or_without_its_id() {
        let plain = claim_of("zebra/TomTom", "TomTom", &["TomTom"], 4_000_000);
        let mut with_id = claim_of("zebra2/CoolBags", "CoolBags", &["CoolBags"], 1);
        with_id.addon.ids.wowi = Some("7032".into());
        let (published, dir) = world().build(vec![plain, with_id]);

        assert!(is_held(&published, "gh:zebra/TomTom"));
        let real = addon_of(&published, "wowi:7032");
        assert_eq!(real.downloads, 3_550_000);
        assert_eq!(real.ids.wowi.as_deref(), Some("7032"));

        // The other one has its own name and folder, so it's listed, but as a stranger:
        // not picked by the app, and its claim to TomTom's id is gone.
        let other = addon_of(&published, "gh:zebra2/CoolBags");
        assert!(!other.trusted);
        assert!(other.ids.is_empty());
        assert_eq!(other.created, BUILT_AT);
        assert_eq!(other.downloads, 1, "no counts borrowed");
        assert_eq!(
            held_file(dir.path())["held"][0]["id"],
            "gh:zebra/TomTom",
            "and a maintainer can see it"
        );
    }

    #[test]
    fn a_look_alike_of_a_popular_name_is_held() {
        let fake = claim_of("zebra/Q", "Qu\u{0435}stie", &["QuestieX"], 9_000_000);
        let (published, _dir) = world().build(vec![fake]);
        assert!(is_held(&published, "gh:zebra/Q"));
    }

    #[test]
    fn a_retitled_sleeper_loses_its_age_and_the_name_it_wanted() {
        let mut w = world();
        // The sleeper has been listed longer than the addon whose name it's about to take.
        for addon in &mut w.previous.addons {
            addon.created = match addon.id.as_str() {
                "gh:zebra/Helpers" => BUILT_AT - 90 * DAY_SECS,
                "gh:Peavers/PeaversCommons" => BUILT_AT - 30 * DAY_SECS,
                _ => merge::BASELINE_CREATED,
            };
        }
        for repo in &mut w.repos {
            if repo.addon.id == "gh:zebra/Helpers" {
                repo.addon.name = "PeaversCommons".into();
            }
        }
        let (published, _dir) = w.build(vec![]);
        assert!(is_held(&published, "gh:zebra/Helpers"));
        assert_eq!(
            addon_of(&published, "gh:Peavers/PeaversCommons").downloads,
            40
        );
    }

    #[test]
    fn claiming_a_gap_is_held_and_the_gap_stays() {
        let claims = vec![
            claim_of("zebra/ElvUI", "ElvUI", &["ElvUI"], 150_000),
            claim_of("zebra/LP", "Leatrix Plus", &["Leatrix_Plus"], 1),
            claim_of("zebra/LP2", "Better Tweaks", &["Tweaks", "Leatrix_Plus"], 1),
        ];
        let (published, _dir) = world().build(claims);
        for id in ["gh:zebra/ElvUI", "gh:zebra/LP", "gh:zebra/LP2"] {
            assert!(is_held(&published, id), "{id}");
        }
        assert_eq!(
            published
                .catalog
                .gaps
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["ElvUI", "Leatrix Plus"]
        );
    }

    #[test]
    fn the_real_elvui_arriving_closes_its_own_gap() {
        let mut w = world();
        w.seeds.repos.push("tukui-org/ElvUI".into());
        let real = claim_of(
            "tukui-org/ElvUI",
            "ElvUI",
            &["ElvUI", "ElvUI_Options"],
            1000,
        );
        let (published, _dir) = w.build(vec![real]);
        assert!(addon_of(&published, "gh:tukui-org/ElvUI").trusted);
        assert_eq!(published.catalog.gaps.len(), 1);
        assert_eq!(published.catalog.gaps[0].name, "Leatrix Plus");
    }

    #[test]
    fn a_new_repo_doesnt_evict_a_seeded_fork() {
        let stranger = claim_of("free-account/Prat", "Prat 3.0", &["Prat-3.0"], 700_000);
        let renamed = claim_of(
            "free-account2/Unrelated",
            "Totally Unrelated",
            &["Prat-3.0"],
            700_000,
        );
        let (published, _dir) = world().build(vec![stranger, renamed]);
        let real = addon_of(&published, "gh:Legacy/prat-3-0");
        assert_eq!(real.downloads, 1_220_000);
        assert!(is_held(&published, "gh:free-account/Prat"));
        assert!(is_held(&published, "gh:free-account2/Unrelated"));
    }

    #[test]
    fn a_one_download_claim_on_details_is_held_under_either_name() {
        let claims = vec![
            claim_of("one/Details", "Details", &["Details"], 1),
            claim_of("two/Details", "Details! Damage Meter", &["DetailsFake"], 1),
        ];
        let (published, _dir) = world().build(claims);
        assert!(is_held(&published, "gh:one/Details"));
        assert!(
            is_held(&published, "gh:two/Details"),
            "the hand-given name is the one that's claimed"
        );
    }

    #[test]
    fn a_new_addon_of_its_own_is_listed_but_not_picked_for_two_weeks() {
        let w = world();
        let new = claim_of("someone/CoolPins", "Cool Pins", &["CoolPins"], 5);
        let (published, _dir) = w.build(vec![new.clone()]);
        let listed = addon_of(&published, "gh:someone/CoolPins");
        assert_eq!(listed.created, BUILT_AT);
        assert!(!listed.trusted);

        let mut later = World {
            previous: published.catalog.clone(),
            ..world()
        };
        later.repos.push(new);
        let picks = HandPicked::new(&later.seeds, &later.curated);
        let at = |days: i64| {
            let scanned = Scanned {
                repos: later.repos.clone(),
                wowi: later.wowi.clone(),
                direct: vec![],
            };
            let settled = settle(
                scanned,
                &later.curated,
                &picks,
                Some(&later.previous),
                &later.remembered,
                BUILT_AT + days * DAY_SECS,
            );
            let addon = settled
                .candidates
                .into_iter()
                .find(|c| c.addon.id == "gh:someone/CoolPins")
                .unwrap()
                .addon;
            (addon.created, addon.trusted)
        };
        assert_eq!(at(13), (BUILT_AT, false));
        assert_eq!(at(14), (BUILT_AT, true));
    }

    #[test]
    fn a_left_out_repository_cant_take_a_listing_with_it() {
        let mut w = world();
        w.curated.exclude.ids.push("gh:Legacy/prat-3-0".into());
        w.previous
            .addons
            .push(listing_entry(1, "x", &["x"], 1).addon);
        let (published, _dir) = w.build(vec![]);
        assert!(
            published
                .catalog
                .addons
                .iter()
                .all(|a| a.id != "gh:Legacy/prat-3-0")
        );
        assert_eq!(addon_of(&published, "wowi:10783").downloads, 610_000);
    }

    fn heads_of(names: &[&str]) -> BatchResult {
        let mut heads = BatchResult::default();
        for name in names {
            heads.found.insert(
                (*name).to_string(),
                RepoInfo {
                    name: (*name).to_string(),
                    releases: vec![GhRelease {
                        tag: "v1".into(),
                        ..GhRelease::default()
                    }],
                    ..RepoInfo::default()
                },
            );
        }
        heads
    }

    #[test]
    fn a_flood_of_new_repos_stops_being_read_once_the_owner_has_plenty() {
        let names: Vec<String> = (0..merge::MAX_ENTRIES_PER_OWNER + 10)
            .map(|at| format!("flood/R{at:03}"))
            .collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut heads = heads_of(&borrowed);
        heads
            .found
            .extend(heads_of(&["other/One", "chosen/Mine"]).found);
        let mut state = State::empty();
        let picks = HandPicked::new(
            &Seeds {
                repos: vec!["chosen/Mine".into()],
                wowi: vec![],
            },
            &Curated::default(),
        );
        let mut tally = Tally::default();
        let (_, to_read) = triage_heads(
            &heads,
            &mut state,
            &Curated::default(),
            &picks,
            1,
            &mut tally,
        );
        assert_eq!(tally.capped, 10);
        assert_eq!(
            to_read
                .iter()
                .filter(|name| name.starts_with("flood/"))
                .count(),
            merge::MAX_ENTRIES_PER_OWNER
        );
        assert!(to_read.iter().any(|name| name == "other/One"));
        assert!(to_read.iter().any(|name| name == "chosen/Mine"));
    }

    #[test]
    fn an_owners_listed_repos_count_toward_the_cap_and_are_still_read() {
        let mut state = State::empty();
        for at in 0..merge::MAX_ENTRIES_PER_OWNER {
            let record = state.note_repo(&format!("big/R{at:03}"), "seed");
            record.entry = Some(Candidate::default());
        }
        let mut heads = heads_of(&["big/New"]);
        heads.found.extend(heads_of(&["big/R000"]).found);
        let mut tally = Tally::default();
        let (_, to_read) = triage_heads(
            &heads,
            &mut state,
            &Curated::default(),
            &HandPicked::default(),
            1,
            &mut tally,
        );
        assert_eq!(
            to_read,
            ["big/R000"],
            "what's listed keeps getting looked at"
        );
        assert_eq!(tally.capped, 1);
    }

    #[test]
    fn only_a_clear_answer_from_github_counts_as_one() {
        let mut heads = heads_of(&[
            "a/Moved",
            "a/Rejected",
            "a/Excluded",
            "a/Unchanged",
            "a/ToRead",
        ]);
        heads.found.get_mut("a/Moved").unwrap().name = "a/NewName".into();
        heads.found.get_mut("a/Rejected").unwrap().archived = true;
        heads.gone.push("a/Gone".into());
        heads.failed.push("a/Failed".into());

        let mut state = State::empty();
        for name in ["a/Gone", "a/Failed", "a/Moved", "a/Rejected", "a/ToRead"] {
            state.note_repo(name, "seed");
        }
        let unchanged = state.note_repo("a/Unchanged", "seed");
        unchanged.newest_tag = Some("v1".into());
        unchanged.looked_at = 1;
        unchanged.entry = Some(Candidate::default());
        let curated = Curated {
            exclude: crate::curated::Exclude {
                repos: vec!["a/Excluded".into()],
                ..Default::default()
            },
            ..Curated::default()
        };
        let mut tally = Tally::default();
        let (entries, to_read) = triage_heads(
            &heads,
            &mut state,
            &curated,
            &HandPicked::default(),
            1,
            &mut tally,
        );
        assert_eq!(entries.len(), 1);
        assert!(to_read.contains(&"a/ToRead".to_string()));
        assert!(to_read.contains(&"a/NewName".to_string()));
        for answered in [
            "a/moved",
            "a/rejected",
            "a/excluded",
            "a/unchanged",
            "a/gone",
        ] {
            assert!(
                tally.answered.contains(&format!("gh:{answered}")),
                "{answered}: {:?}",
                tally.answered
            );
        }
        for not_yet in ["a/failed", "a/toread", "a/newname"] {
            assert!(
                !tally.answered.contains(&format!("gh:{not_yet}")),
                "{not_yet}"
            );
        }

        // The proper look answers for what it could read, and not for the rest.
        let mut batch = BatchResult {
            gone: vec!["a/GoneLater".into()],
            failed: vec!["a/FailedLater".into()],
            ..BatchResult::default()
        };
        batch
            .found
            .insert("a/ToRead".into(), repo_with_release("v1", "Good.zip"));
        batch
            .found
            .insert("a/NewName".into(), repo_with_release("v2", "Bad.zip"));
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Good.zip".into(), Ok(forever_scan("Good Thing")));
        probe.zips.insert("Bad.zip".into(), Err("HTTP 503".into()));
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver {
            path: dir.path().join("state.json"),
            last: Instant::now(),
        };
        read_releases(
            &batch,
            &SplitReleases::new(),
            &mut state,
            &curated,
            &Pins::default(),
            &mut probe,
            &|| false,
            &mut saver,
            &mut tally,
        );
        assert!(tally.answered.contains("gh:a/toread"));
        assert!(tally.answered.contains("gh:a/gonelater"));
        assert!(!tally.answered.contains("gh:a/newname"), "its read failed");
        assert!(!tally.answered.contains("gh:a/failedlater"));
    }

    #[test]
    fn the_largest_repositories_can_be_named() {
        let mut state = State::empty();
        state.note_repo("small/One", "seed");
        let big = state.note_repo("big/One", "seed");
        big.entry = Some(addon_candidate("gh:big/One", "Big", 1));
        state.note_repo("small/Two", "seed");
        let largest = largest_repos(&state, 2);
        assert_eq!(largest[0].1, "big/One");
        assert!(largest[0].0 > largest[1].0);
        assert_eq!(largest.len(), 2);
    }

    /// Detection answers from a script, so the bookkeeping can be tested offline.
    #[derive(Default)]
    struct ScriptedProbe {
        zips: BTreeMap<String, Result<ZipScan, String>>,
        reads: u32,
    }

    impl Probe for ScriptedProbe {
        fn release_json(&mut self, _: &GhAsset) -> Result<String, String> {
            Err("not used".to_string())
        }
        fn read_zip(
            &mut self,
            asset: &GhAsset,
            _: &[&str],
            pin: Option<&str>,
        ) -> Result<ZipScan, String> {
            self.reads += 1;
            let mut scan = self
                .zips
                .get(&asset.name)
                .cloned()
                .unwrap_or_else(|| Err("no such zip".to_string()))?;
            // As the real scan does: the pinned folder first whenever the zip has it.
            if let Some(pin) = pin {
                detect::lead_with(&mut scan.folders, pin);
            }
            Ok(scan)
        }
    }

    fn repo_with_release(tag: &str, zip: &str) -> RepoInfo {
        RepoInfo {
            name: "a/Addon".into(),
            stars: 3,
            releases: vec![GhRelease {
                tag: tag.into(),
                published: detect::FOREVER_EPOCH + 1000,
                assets: vec![GhAsset {
                    name: zip.into(),
                    size: 10,
                    url: format!("https://github.com/a/Addon/{zip}"),
                    ..GhAsset::default()
                }],
                ..GhRelease::default()
            }],
            ..RepoInfo::default()
        }
    }

    fn forever_scan(title: &str) -> ZipScan {
        ZipScan {
            folders: vec!["Addon".into()],
            forever: true,
            title: Some(title.into()),
            ..ZipScan::default()
        }
    }

    fn retail_scan() -> ZipScan {
        ZipScan {
            folders: vec!["Addon".into()],
            ..ZipScan::default()
        }
    }

    fn process(
        probe: &mut ScriptedProbe,
        info: &RepoInfo,
        record: &mut RepoState,
        picked: bool,
        tally: &mut Tally,
    ) -> Option<Candidate> {
        let look = Look {
            asked: &info.name,
            picked,
            pins: &Pins::default(),
            out_of_time: &|| false,
        };
        process_repo(probe, info, record, &look, tally)
    }

    #[test]
    fn a_good_repo_is_listed_and_remembered() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let info = repo_with_release("v1", "Addon-1.zip");
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let entry = process(&mut probe, &info, &mut record, false, &mut tally).unwrap();
        assert_eq!(entry.addon.name, "Good Thing");
        assert_eq!(record.entry.as_ref(), Some(&entry));
        assert_eq!(record.newest_tag.as_deref(), Some("v1"));
        assert!(record.looked_at > 0);
        assert_eq!(record.looked_at % DAY_SECS, 0, "changes at most once a day");
        assert!(!record.incomplete);

        // The same release again costs nothing.
        process(&mut probe, &info, &mut record, false, &mut tally).unwrap();
        assert_eq!(probe.reads, 1);
    }

    #[test]
    fn a_repo_that_fails_keeps_its_last_known_result_and_gets_retried() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let good = process(
            &mut probe,
            &repo_with_release("v1", "Addon-1.zip"),
            &mut record,
            false,
            &mut tally,
        );

        // A new release that can't be read, and the old one has left the list.
        probe
            .zips
            .insert("Addon-2.zip".into(), Err("HTTP 503".into()));
        let info = repo_with_release("v2", "Addon-2.zip");
        let kept = process(&mut probe, &info, &mut record, false, &mut tally);
        assert_eq!(kept, good);
        assert_eq!(record.entry, good);
        assert!(record.incomplete, "the next run must try again");
        assert_eq!(tally.kept_last_known, 1);
        assert!(
            !record.releases.contains_key("v2"),
            "a failure isn't remembered as a verdict"
        );
    }

    #[test]
    fn a_failure_with_nothing_known_is_just_skipped() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Err("HTTP 503".into()));
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let info = repo_with_release("v1", "Addon-1.zip");
        assert_eq!(
            process(&mut probe, &info, &mut record, false, &mut tally),
            None
        );
        assert!(record.incomplete);
        assert_eq!(tally.kept_last_known, 0);
    }

    #[test]
    fn a_repo_without_a_forever_build_is_counted_and_forgotten() {
        let mut probe = ScriptedProbe::default();
        probe.zips.insert("Addon-1.zip".into(), Ok(retail_scan()));
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let info = repo_with_release("v1", "Addon-1.zip");
        assert_eq!(
            process(&mut probe, &info, &mut record, false, &mut tally),
            None
        );
        assert_eq!(tally.no_build, 1);
        assert!(!record.incomplete);
        assert_eq!(record.entry, None);
    }

    #[test]
    fn dropping_forever_turns_a_listed_addon_grey() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let info = repo_with_release("v1", "Addon-1.zip");
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        process(&mut probe, &info, &mut record, false, &mut tally).unwrap();

        // The author re-uploads the same release, and now its TOC has no Forever.
        let mut changed = info.clone();
        changed.releases[0].assets[0].size = 20;
        probe.zips.insert("Addon-1.zip".into(), Ok(retail_scan()));
        let grey = process(&mut probe, &changed, &mut record, false, &mut tally).unwrap();
        assert!(grey.no_longer_forever);
        assert!(record.entry.as_ref().unwrap().no_longer_forever);
    }

    #[test]
    fn a_deleted_release_takes_the_addon_with_it() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        process(
            &mut probe,
            &repo_with_release("v1", "Addon-1.zip"),
            &mut record,
            false,
            &mut tally,
        )
        .unwrap();
        let empty = RepoInfo {
            name: "a/Addon".into(),
            ..RepoInfo::default()
        };
        assert_eq!(
            process(&mut probe, &empty, &mut record, false, &mut tally),
            None
        );
        assert_eq!(record.entry, None);
    }

    #[test]
    fn a_placeholder_title_is_junk_unless_someone_picked_it() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("MyAddon")));
        let info = repo_with_release("v1", "Addon-1.zip");
        let mut tally = Tally::default();
        assert_eq!(
            process(
                &mut probe,
                &info,
                &mut RepoState::default(),
                false,
                &mut tally
            ),
            None
        );
        assert_eq!(tally.placeholder, 1);
        assert!(
            process(
                &mut probe,
                &info,
                &mut RepoState::default(),
                true,
                &mut tally
            )
            .is_some()
        );
    }

    struct ScriptedWowi {
        result: Result<Vec<u8>, String>,
        calls: u32,
    }

    impl wowi::WowiProbe for ScriptedWowi {
        fn list_files(&mut self, _: u64) -> Result<Vec<String>, String> {
            self.calls += 1;
            Ok(vec!["Foo/Foo.toc".to_string()])
        }
        fn download(&mut self, _: u64) -> Result<Vec<u8>, String> {
            self.calls += 1;
            self.result.clone()
        }
        fn details(&mut self, _: u64) -> Result<(Option<String>, Option<String>), String> {
            self.calls += 1;
            Ok((Some("b".repeat(32)), None))
        }
    }

    fn wowi_zip(interface: &str) -> Vec<u8> {
        use std::io::Cursor;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("Foo/Foo.toc", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(format!("## Interface: {interface}\n").as_bytes())
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn wowi_row(version: &str, downloads: u64) -> wowi::Row {
        wowi::Row {
            id: 42,
            version: version.into(),
            name: "Foo".into(),
            downloads,
            ..wowi::Row::default()
        }
    }

    fn scan_rows(
        rows: &[wowi::Row],
        state: &mut State,
        probe: &mut ScriptedWowi,
        stop: bool,
        tally: &mut Tally,
    ) -> Vec<Candidate> {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver {
            path: dir.path().join("state.json"),
            last: Instant::now(),
        };
        scan_wowi_rows(rows, state, probe, &move || stop, &mut saver, tally)
    }

    #[test]
    fn wowi_files_are_only_read_when_their_version_changes() {
        let mut state = State::empty();
        let mut tally = Tally::default();
        let mut probe = ScriptedWowi {
            result: Ok(wowi_zip("16001")),
            calls: 0,
        };

        let first = scan_rows(
            &[wowi_row("1.0", 100)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );
        assert_eq!(first.len(), 1);
        let calls = probe.calls;
        assert_eq!(calls, 3, "list, download, details");

        let again = scan_rows(
            &[wowi_row("1.0", 500)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );
        assert_eq!(probe.calls, calls, "same version, no requests");
        assert_eq!(again[0].addon.downloads, 500, "but the counts stay fresh");

        let newer = scan_rows(
            &[wowi_row("2.0", 500)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );
        assert!(probe.calls > calls);
        assert_eq!(newer[0].addon.stable.as_ref().unwrap().version, "2.0");
    }

    #[test]
    fn wowi_negatives_are_remembered_too() {
        let mut state = State::empty();
        let mut tally = Tally::default();
        let mut probe = ScriptedWowi {
            result: Ok(wowi_zip("120105")),
            calls: 0,
        };
        assert!(
            scan_rows(
                &[wowi_row("1.0", 1)],
                &mut state,
                &mut probe,
                false,
                &mut tally
            )
            .is_empty()
        );
        let calls = probe.calls;
        assert!(
            scan_rows(
                &[wowi_row("1.0", 1)],
                &mut state,
                &mut probe,
                false,
                &mut tally
            )
            .is_empty()
        );
        assert_eq!(probe.calls, calls);
    }

    #[test]
    fn a_wowi_zip_with_too_many_folders_is_remembered_not_downloaded_every_run() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for at in 0..=crate::zipscan::MAX_FOLDERS {
            writer
                .start_file(
                    format!("Pack{at}/Pack{at}.toc"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"## Interface: 16001\n").unwrap();
        }
        let mut state = State::empty();
        let mut tally = Tally::default();
        let mut probe = ScriptedWowi {
            result: Ok(writer.finish().unwrap().into_inner()),
            calls: 0,
        };
        let row = [wowi_row("1.0", 1)];
        assert!(scan_rows(&row, &mut state, &mut probe, false, &mut tally).is_empty());
        let calls = probe.calls;
        assert!(calls > 0);
        assert!(scan_rows(&row, &mut state, &mut probe, false, &mut tally).is_empty());
        assert_eq!(probe.calls, calls, "the same version costs nothing again");
        assert_eq!(state.wowi[&42].entry, None);
    }

    #[test]
    fn a_wowi_failure_keeps_the_entry_without_its_old_checksum_and_retries_later() {
        let mut state = State::empty();
        let mut tally = Tally::default();
        let mut probe = ScriptedWowi {
            result: Ok(wowi_zip("16001")),
            calls: 0,
        };
        scan_rows(
            &[wowi_row("1.0", 1)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );

        probe.result = Err("HTTP 503".to_string());
        let kept = scan_rows(
            &[wowi_row("2.0", 1)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );
        assert_eq!(kept.len(), 1);
        let release = kept[0].addon.stable.as_ref().unwrap();
        assert_eq!(release.md5, None, "the URL now serves a different file");
        assert_eq!(release.size, 0);
        assert_eq!(tally.kept_last_known, 1);
        assert_eq!(
            state.wowi[&42].version, "1.0",
            "not marked as read, so it's tried again"
        );
    }

    #[test]
    fn running_out_of_time_on_wowi_makes_no_requests_and_keeps_what_it_can() {
        let mut state = State::empty();
        let mut tally = Tally::default();
        let mut probe = ScriptedWowi {
            result: Ok(wowi_zip("16001")),
            calls: 0,
        };
        assert!(
            scan_rows(
                &[wowi_row("1.0", 1)],
                &mut state,
                &mut probe,
                true,
                &mut tally
            )
            .is_empty()
        );
        assert_eq!(probe.calls, 0);

        scan_rows(
            &[wowi_row("1.0", 1)],
            &mut state,
            &mut probe,
            false,
            &mut tally,
        );
        let calls = probe.calls;
        let kept = scan_rows(
            &[wowi_row("2.0", 1)],
            &mut state,
            &mut probe,
            true,
            &mut tally,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(probe.calls, calls);
    }

    #[test]
    fn progress_is_saved_during_long_loops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog-state.json");
        let mut saver = Saver {
            path: path.clone(),
            last: Instant::now(),
        };
        let mut state = State::empty();
        state.searched_at = 7;
        saver.maybe_save(&state);
        assert!(!path.exists(), "not yet");
        saver.last = Instant::now()
            .checked_sub(SAVE_EVERY + Duration::from_secs(1))
            .unwrap();
        saver.maybe_save(&state);
        assert_eq!(
            State::load(&path).searched_at,
            7,
            "a cut-off run still leaves this behind"
        );
    }

    #[test]
    fn a_previous_catalog_that_cant_be_read_stops_the_build_unless_a_maintainer_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_previous(None, false).unwrap().is_none(),
            "the very first build"
        );

        let missing = dir.path().join("missing.json");
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json").unwrap();
        for path in [&missing, &bad] {
            let error = load_previous(Some(path), false).unwrap_err();
            assert!(error.contains("nothing is being published"), "{error}");
            assert!(
                load_previous(Some(path), true).unwrap().is_none(),
                "accept_smaller starts over"
            );
        }

        let good = dir.path().join("good.json");
        std::fs::write(&good, br#"{"schema":1,"generated":1,"addons":[]}"#).unwrap();
        let catalog = load_previous(Some(&good), false).unwrap().unwrap();
        assert!(catalog.addons.is_empty());
    }

    #[test]
    fn gzip_size_is_plausible() {
        let text = vec![b'a'; 100_000];
        let small = gzipped_len(&text).unwrap();
        assert!(small > 0 && small < 1000);
    }

    // One product per entry.

    fn scan_of(folders: &[&str], title: &str) -> ZipScan {
        ZipScan {
            folders: folders.iter().map(|f| f.to_string()).collect(),
            forever: true,
            title: Some(title.into()),
            ..ZipScan::default()
        }
    }

    /// Releases of `a/Addon`, newest first, one zip each.
    fn repo_with_releases(releases: &[(&str, &str)]) -> RepoInfo {
        let mut info = repo_with_release("unused", "unused.zip");
        info.releases = releases
            .iter()
            .enumerate()
            .map(|(at, (tag, zip))| {
                let mut release = repo_with_release(tag, zip).releases.remove(0);
                release.published -= i64::try_from(at).unwrap() * 100;
                release
            })
            .collect();
        info
    }

    /// The last catalog, listing these entries with these folders.
    fn listed(entries: &[(&str, &[&str])]) -> Catalog {
        Catalog {
            schema: CATALOG_SCHEMA,
            addons: entries
                .iter()
                .map(|(id, folders)| Addon {
                    id: (*id).to_string(),
                    name: "Listed".into(),
                    folders: folders.iter().map(|f| f.to_string()).collect(),
                    stable: Some(Release {
                        version: "v1".into(),
                        url: "https://github.com/a/Addon/releases/download/v1/Addon-1.zip".into(),
                        ..Release::default()
                    }),
                    ..Addon::default()
                })
                .collect(),
            ..Catalog::default()
        }
    }

    fn process_pinned(
        probe: &mut ScriptedProbe,
        info: &RepoInfo,
        record: &mut RepoState,
        pins: &Pins<'_>,
        tally: &mut Tally,
    ) -> Option<Candidate> {
        process_asked_as(probe, &info.name, info, record, pins, tally)
    }

    /// The full look at a repository GitHub was asked about as `asked`, which it may since
    /// have renamed to `info.name`.
    fn process_asked_as(
        probe: &mut ScriptedProbe,
        asked: &str,
        info: &RepoInfo,
        record: &mut RepoState,
        pins: &Pins<'_>,
        tally: &mut Tally,
    ) -> Option<Candidate> {
        let look = Look {
            asked,
            picked: false,
            pins,
            out_of_time: &|| false,
        };
        process_repo(probe, info, record, &look, tally)
    }

    /// `curated.toml` with one `[pins]` entry.
    fn pinned_by_hand(id: &str, main: &str, was: &[&str]) -> Curated {
        Curated {
            pins: BTreeMap::from([(
                id.to_string(),
                crate::curated::PinSpec {
                    main: Some(main.into()),
                    was: was.iter().map(|f| f.to_string()).collect(),
                },
            )]),
            ..Curated::default()
        }
    }

    /// A repository whose addon is `Addon`, and whose newest release is a module of its own.
    fn module_then_addon() -> (ScriptedProbe, RepoInfo) {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Module-2.zip".into(), Ok(scan_of(&["Module"], "Module")));
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let info = repo_with_releases(&[("module-v2", "Module-2.zip"), ("v1", "Addon-1.zip")]);
        (probe, info)
    }

    fn version_of(entry: &Candidate) -> &str {
        &entry.addon.stable.as_ref().unwrap().version
    }

    #[test]
    fn a_newer_release_of_another_addon_is_skipped_and_the_older_one_is_listed() {
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let entry = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally).unwrap();
        assert_eq!(version_of(&entry), "v1");
        assert_eq!(entry.addon.folders, ["Addon"]);
        assert!(!record.incomplete);
        assert!(
            !record.releases["module-v2"].confirmed_negative,
            "another addon's release isn't a negative"
        );
        assert_eq!(record.newest_tag.as_deref(), Some("module-v2"));
        assert_eq!(
            tally.passed_over,
            [
                "gh:a/Addon lists v1, passed over newer module-v2, which doesn't ship addon. If module-v2 holds the addon this entry should list, set [pins.\"gh:a/Addon\"] in curated.toml"
            ],
            "the summary says so, in case the newer one is a rename"
        );
        assert_eq!(tally.no_build, 0);
    }

    #[test]
    fn a_rename_with_its_old_build_still_in_reach_is_named_in_the_summary() {
        // Azerdle: v0.1.2-beta shipped WordOfWarcraft, then v0.2.0-beta renamed it, and the
        // old build is still listed, so nothing else would ever say so.
        let mut probe = ScriptedProbe::default();
        probe.zips.insert(
            "Azerdle-v0.2.0-beta-forever.zip".into(),
            Ok(scan_of(&["Azerdle"], "Azerdle")),
        );
        probe.zips.insert(
            "Word-Of-Warcraft-v0.1.2-beta-forever.zip".into(),
            Ok(scan_of(&["WordOfWarcraft"], "Word of Warcraft")),
        );
        let info = repo_with_releases(&[
            ("v0.2.0-beta", "Azerdle-v0.2.0-beta-forever.zip"),
            ("v0.1.2-beta", "Word-Of-Warcraft-v0.1.2-beta-forever.zip"),
        ]);
        let last = listed(&[("gh:a/Addon", &["WordOfWarcraft"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        for run in 0..2 {
            let mut tally = Tally::default();
            let entry = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally).unwrap();
            assert_eq!(entry.addon.folders, ["WordOfWarcraft"]);
            assert_eq!(tally.passed_over.len(), 1, "run {run}");
            assert!(
                tally.passed_over[0].starts_with(
                    "gh:a/Addon lists v0.1.2-beta, passed over newer v0.2.0-beta, which doesn't ship WordOfWarcraft. If v0.2.0-beta holds"
                ),
                "{}",
                tally.passed_over[0]
            );
        }
        assert_eq!(probe.reads, 2, "both read once, then cached");
    }

    #[test]
    fn a_quick_look_that_finds_nothing_new_still_names_a_passed_over_release() {
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut state = State::empty();
        let record = state.note_repo("a/Addon", "seed");
        process_pinned(&mut probe, &info, record, &pins, &mut Tally::default()).unwrap();

        let mut heads = heads_of(&["a/Addon"]);
        heads.found.get_mut("a/Addon").unwrap().releases[0].tag = "module-v2".into();
        let mut tally = Tally::default();
        let (entries, to_read) = triage_heads(
            &heads,
            &mut state,
            &Curated::default(),
            &HandPicked::default(),
            unix_now(),
            &mut tally,
        );
        assert!(to_read.is_empty(), "nothing new to read");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            tally.passed_over,
            [
                "gh:a/Addon lists v1, passed over newer module-v2, which doesn't ship Addon. If module-v2 holds the addon this entry should list, set [pins.\"gh:a/Addon\"] in curated.toml"
            ]
        );
    }

    #[test]
    fn an_older_release_of_another_addon_isnt_worth_a_summary_line() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-2b.zip".into(), Ok(forever_scan("Good Thing")));
        probe
            .zips
            .insert("Module-1.zip".into(), Ok(scan_of(&["Module"], "Module")));
        let mut info =
            repo_with_releases(&[("v2-beta", "Addon-2b.zip"), ("module-v1", "Module-1.zip")]);
        info.releases[0].prerelease = true;
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut tally = Tally::default();
        let entry = process_pinned(
            &mut probe,
            &info,
            &mut RepoState::default(),
            &pins,
            &mut tally,
        )
        .unwrap();
        assert_eq!(entry.addon.beta.as_ref().unwrap().version, "v2-beta");
        assert!(
            tally.passed_over.is_empty(),
            "what it lists is newer than the module: {:?}",
            tally.passed_over
        );
    }

    #[test]
    fn with_nothing_matching_the_last_build_is_kept_and_reported() {
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let good =
            process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default()).unwrap();

        // The addon's own release has left the list's newest fifteen, and only the module's
        // is in reach, so the addon's file can still be there.
        let mut only_module = repo_with_releases(&[("module-v2", "Module-2.zip")]);
        only_module.releases =
            std::iter::repeat_n(only_module.releases[0].clone(), RELEASES_FETCHED)
                .enumerate()
                .map(|(at, mut release)| {
                    release.tag = format!("module-v{}", RELEASES_FETCHED + 1 - at);
                    release
                })
                .collect();
        let mut tally = Tally::default();
        let kept = process_pinned(&mut probe, &only_module, &mut record, &pins, &mut tally);
        assert_eq!(kept.as_ref(), Some(&good));
        assert_eq!(record.entry.as_ref(), Some(&good));
        assert!(!record.incomplete, "nothing failed");
        assert_eq!(
            tally.passed_over,
            [
                "gh:a/Addon keeps v1, its last build, passed over newer module-v16, which doesn't ship Addon. If module-v16 holds the addon this entry should list, set [pins.\"gh:a/Addon\"] in curated.toml"
            ]
        );
        assert_eq!(tally.no_build, 0);
    }

    #[test]
    fn a_last_build_whose_file_is_gone_isnt_kept() {
        // A rename whose old release was deleted: only the renamed build is left, and the
        // list isn't full, so the old file can't be there any more.
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Old-1.zip".into(), Ok(scan_of(&["Old"], "Old")));
        probe
            .zips
            .insert("New-2.zip".into(), Ok(scan_of(&["New"], "New")));
        let last = listed(&[("gh:a/Addon", &["Old"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        process_pinned(
            &mut probe,
            &repo_with_releases(&[("v1", "Old-1.zip")]),
            &mut record,
            &pins,
            &mut Tally::default(),
        )
        .unwrap();
        let mut tally = Tally::default();
        let only_new = repo_with_releases(&[("v2", "New-2.zip")]);
        let kept = process_pinned(&mut probe, &only_new, &mut record, &pins, &mut tally);
        assert_eq!(kept, None, "its download would only fail");
        assert_eq!(record.entry, None);
        assert!(
            tally.passed_over[0].starts_with(
                "gh:a/Addon is left out, as its last build's file is gone, passed over newer v2"
            ),
            "{:?}",
            tally.passed_over
        );
    }

    #[test]
    fn with_nothing_in_the_scan_cache_nothing_is_rebuilt_from_the_last_catalog() {
        // The published entry has lost what decides its category, greyness and forks, and
        // its downloads include a WoWInterface listing's, so it can't stand in for a build.
        let (mut probe, _) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let only_module = repo_with_releases(&[("module-v2", "Module-2.zip")]);
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let kept = process_pinned(&mut probe, &only_module, &mut record, &pins, &mut tally);
        assert_eq!(kept, None);
        assert_eq!(record.entry, None);
        assert!(
            tally.passed_over[0].starts_with(
                "gh:a/Addon is left out, with no build of its own to keep, passed over newer module-v2"
            ),
            "{:?}",
            tally.passed_over
        );
    }

    #[test]
    fn a_brand_new_entry_with_no_last_catalog_still_takes_the_newest() {
        for last in [None, Some(listed(&[("gh:someone/Else", &["Addon"])]))] {
            let (mut probe, info) = module_then_addon();
            let pins = Pins::new(last.as_ref(), &Curated::default());
            let mut tally = Tally::default();
            let entry = process_pinned(
                &mut probe,
                &info,
                &mut RepoState::default(),
                &pins,
                &mut tally,
            )
            .unwrap();
            assert_eq!(entry.addon.folders, ["Module"]);
            assert!(tally.passed_over.is_empty());
        }
    }

    #[test]
    fn a_pin_in_curated_toml_brings_back_the_addon_its_data_pack_had_replaced() {
        // foreversixty: the last catalog lists its nightly data pack, which took the entry's
        // place the day before, and the addon releases several times a day besides.
        let mut probe = ScriptedProbe::default();
        let addon = "ForeverSixty-v2.0.35-forever.zip";
        let data = "ForeverSixtyData-data-v2026.11.08-forever.zip";
        probe.zips.insert(
            addon.into(),
            Ok(scan_of(
                &["ForeverSixty", "ForeverSixtyInbox"],
                "Forever Sixty",
            )),
        );
        probe.zips.insert(
            data.into(),
            Ok(scan_of(&["ForeverSixtyData"], "Forever Sixty Data")),
        );
        let last = listed(&[("gh:a/Addon", &["ForeverSixtyData"])]);

        // Without a pin written down, the last catalog's data pack is what the entry stays.
        let data_newest =
            repo_with_releases(&[("data-v2026.11.08", data), ("addon-v2.0.35", addon)]);
        let addon_newest =
            repo_with_releases(&[("addon-v2.0.35", addon), ("data-v2026.11.08", data)]);
        let unpinned = Pins::new(Some(&last), &Curated::default());
        let entry = process_pinned(
            &mut probe,
            &addon_newest,
            &mut RepoState::default(),
            &unpinned,
            &mut Tally::default(),
        )
        .unwrap();
        assert_eq!(entry.addon.folders, ["ForeverSixtyData"]);

        // [pins] names the addon, whichever of the two released last.
        let curated = pinned_by_hand("gh:a/Addon", "ForeverSixty", &[]);
        let pins = Pins::new(Some(&last), &curated);
        for info in [&data_newest, &addon_newest] {
            let mut record = RepoState {
                entry: Some(real_entry(
                    "a/Addon",
                    "Forever Sixty Data",
                    &["ForeverSixtyData"],
                    1,
                )),
                ..RepoState::default()
            };
            let mut tally = Tally::default();
            let entry = process_pinned(&mut probe, info, &mut record, &pins, &mut tally).unwrap();
            assert_eq!(entry.addon.folders, ["ForeverSixty", "ForeverSixtyInbox"]);
            assert!(entry.addon.stable.as_ref().unwrap().url.ends_with(addon));
            assert_eq!(record.entry.as_ref(), Some(&entry));
        }
        let mut tally = Tally::default();
        process_pinned(
            &mut probe,
            &data_newest,
            &mut RepoState::default(),
            &pins,
            &mut tally,
        );
        assert_eq!(
            tally.passed_over,
            [
                "gh:a/Addon lists addon-v2.0.35, passed over newer data-v2026.11.08, which doesn't ship ForeverSixty, the main folder [pins] in curated.toml gives it"
            ]
        );
    }

    #[test]
    fn a_pin_written_down_lets_a_rename_through_and_vouches_for_the_old_folder() {
        // The addon renamed its main folder from Addon to Module, and a person said so.
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let curated = pinned_by_hand("gh:a/Addon", "Module", &["Addon"]);
        let pins = Pins::new(Some(&last), &curated);
        let mut tally = Tally::default();
        let entry = process_pinned(
            &mut probe,
            &info,
            &mut RepoState::default(),
            &pins,
            &mut tally,
        )
        .unwrap();
        assert_eq!(entry.addon.folders, ["Module"]);
        assert!(tally.passed_over.is_empty());

        let scanned = Scanned {
            repos: vec![entry],
            ..Scanned::default()
        };
        let settled = settle(
            scanned,
            &curated,
            &HandPicked::default(),
            Some(&last),
            &Remembered::default(),
            BUILT_AT,
        );
        let renamed = entry_of(&settled.candidates, "gh:a/Addon");
        assert_eq!(
            renamed.addon.formerly,
            ["Addon"],
            "so copies installed as Addon still get its updates"
        );
        assert_eq!(settled.vouched, ["gh:a/Addon was Addon"]);
    }

    /// The full look at `to_read`, asked the way `run_github` asks it: under exactly the names
    /// the quick look's triage gave it.
    #[allow(clippy::too_many_arguments)]
    fn full_look(
        to_read: &[String],
        info: &RepoInfo,
        state: &mut State,
        curated: &Curated,
        pins: &Pins<'_>,
        probe: &mut ScriptedProbe,
        tally: &mut Tally,
    ) -> Vec<Candidate> {
        let mut batch = BatchResult::default();
        for name in to_read {
            batch.found.insert(name.clone(), info.clone());
        }
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver {
            path: dir.path().join("state.json"),
            last: Instant::now(),
        };
        read_releases(
            &batch,
            &SplitReleases::new(),
            state,
            curated,
            pins,
            probe,
            &|| false,
            &mut saver,
            tally,
        )
    }

    /// The quick look on a run where GitHub answers for `a/Addon` as `a/Renamed`, whose newest
    /// tag is `newest`. Returns what it names for the full look.
    fn renamed_on_github(state: &mut State, curated: &Curated, newest: &str) -> Vec<String> {
        let mut heads = heads_of(&["a/Addon"]);
        let head = heads.found.get_mut("a/Addon").unwrap();
        head.name = "a/Renamed".into();
        head.releases[0].tag = newest.into();
        let (unchanged, to_read) = triage_heads(
            &heads,
            state,
            curated,
            &HandPicked::default(),
            unix_now(),
            &mut Tally::default(),
        );
        assert!(unchanged.is_empty());
        to_read
    }

    #[test]
    fn a_rename_the_quick_look_finds_keeps_its_pin_through_the_full_look() {
        // The last run read a/Addon when its own v1 was its only release.
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut state = State::empty();
        let record = state.note_repo("a/Addon", "seed");
        let v1_only = repo_with_releases(&[("v1", "Addon-1.zip")]);
        process_pinned(&mut probe, &v1_only, record, &pins, &mut Tally::default()).unwrap();

        // This run GitHub answers for it as a/Renamed, and a module of its own is the newest.
        let to_read = renamed_on_github(&mut state, &Curated::default(), "module-v2");
        assert_eq!(
            to_read,
            ["a/Renamed"],
            "the full look asks for the new name"
        );
        let renamed = RepoInfo {
            name: "a/Renamed".into(),
            ..info
        };
        let mut tally = Tally::default();
        let entries = full_look(
            &to_read,
            &renamed,
            &mut state,
            &Curated::default(),
            &pins,
            &mut probe,
            &mut tally,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].addon.id, "gh:a/Renamed");
        assert_eq!(
            entries[0].addon.folders,
            ["Addon"],
            "still the addon, not its module: {:?}",
            tally.passed_over
        );
        assert_eq!(tally.passed_over.len(), 1, "and the module is named");
    }

    #[test]
    fn a_rename_alone_while_a_module_is_newest_keeps_the_pin() {
        // The module is already the newest release, passed over and cached as another addon's,
        // when GitHub renames the repository. No new release comes with the rename.
        let (mut probe, info) = module_then_addon();
        let renamed = RepoInfo {
            name: "a/Renamed".into(),
            ..info.clone()
        };
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let from_the_catalog = Pins::new(Some(&last), &Curated::default());
        let curated = pinned_by_hand("gh:a/Addon", "Addon", &[]);
        let written = Pins::new(None, &curated);
        // The pin from the last catalog, and one written under the old name with no catalog.
        for (pins, curated) in [
            (&from_the_catalog, &Curated::default()),
            (&written, &curated),
        ] {
            let mut state = State::empty();
            let record = state.note_repo("a/Addon", "seed");
            let entry =
                process_pinned(&mut probe, &info, record, pins, &mut Tally::default()).unwrap();
            assert_eq!(entry.addon.folders, ["Addon"]);
            assert_eq!(
                record.releases["module-v2"].skipped_for.as_deref(),
                Some("Addon")
            );

            let to_read = renamed_on_github(&mut state, curated, "module-v2");
            assert_eq!(to_read, ["a/Renamed"], "a rename always gets a full look");
            let reads = probe.reads;
            let mut tally = Tally::default();
            let entries = full_look(
                &to_read, &renamed, &mut state, curated, pins, &mut probe, &mut tally,
            );
            assert_eq!(
                entries[0].addon.folders,
                ["Addon"],
                "{:?}",
                tally.passed_over
            );
            assert_eq!(probe.reads, reads, "the cached verdicts still answer");
        }
    }

    #[test]
    fn with_no_last_catalog_to_read_the_scan_cache_says_which_addon_an_entry_is() {
        // accept_smaller over a catalog.json that can't be read: the scan cache still knows.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("catalog.json");
        std::fs::write(&bad, b"not json").unwrap();
        let previous = load_previous(Some(&bad), true).unwrap();
        assert!(previous.is_none());
        let pins = Pins::new(previous.as_ref(), &Curated::default());
        let (mut probe, info) = module_then_addon();
        let mut record = RepoState {
            entry: Some(real_entry("a/Addon", "Good Thing", &["Addon"], 1)),
            ..RepoState::default()
        };
        let entry =
            process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default()).unwrap();
        assert_eq!(entry.addon.folders, ["Addon"], "not the newer module");

        // A readable last catalog that doesn't list it means a new entry, whatever the cache has.
        let unlisted = listed(&[]);
        let pins = Pins::new(Some(&unlisted), &Curated::default());
        let entry =
            process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default()).unwrap();
        assert_eq!(entry.addon.folders, ["Module"]);
    }

    #[test]
    fn a_last_result_thats_another_addon_is_never_kept_for_this_one() {
        // [pins] brings back the addon, and its newest release can't be read this run: the
        // data pack in the scan cache must not stay listed in the meantime.
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("ForeverSixty-2.zip".into(), Err("HTTP 503".into()));
        let info = repo_with_releases(&[("addon-v2", "ForeverSixty-2.zip")]);
        let curated = pinned_by_hand("gh:a/Addon", "ForeverSixty", &[]);
        let pins = Pins::new(None, &curated);
        let mut record = RepoState {
            entry: Some(real_entry(
                "a/Addon",
                "Forever Sixty Data",
                &["ForeverSixtyData"],
                1,
            )),
            ..RepoState::default()
        };
        let mut tally = Tally::default();
        let entry = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally);
        assert_eq!(entry, None);
        assert!(record.incomplete, "tried again next run");
        assert_eq!(tally.kept_last_known, 0);
    }

    #[test]
    fn a_pin_written_down_gets_a_quiet_repository_read_again() {
        let mut state = State::empty();
        let record = state.note_repo("a/Addon", "seed");
        record.entry = Some(real_entry(
            "a/Addon",
            "Forever Sixty Data",
            &["ForeverSixtyData"],
            1,
        ));
        record.newest_tag = Some("v1".into());
        record.looked_at = unix_now();
        let heads = heads_of(&["a/Addon"]);
        let read = |state: &mut State, curated: &Curated| {
            let (_, to_read) = triage_heads(
                &heads,
                state,
                curated,
                &HandPicked::default(),
                unix_now(),
                &mut Tally::default(),
            );
            to_read
        };
        assert!(
            read(&mut state, &Curated::default()).is_empty(),
            "nothing new"
        );
        assert_eq!(
            read(
                &mut state,
                &pinned_by_hand("gh:a/addon", "ForeverSixty", &[])
            ),
            ["a/Addon"]
        );
        assert!(
            read(
                &mut state,
                &pinned_by_hand("gh:a/Addon", "ForeverSixtyData", &[])
            )
            .is_empty(),
            "a pin it already meets changes nothing"
        );
    }

    fn sized_asset(name: &str, size: u64) -> GhAsset {
        GhAsset {
            name: name.into(),
            size,
            url: format!("https://github.com/a/Addon/{name}"),
            ..GhAsset::default()
        }
    }

    #[test]
    fn the_pinned_addons_own_zip_is_read_before_five_bigger_packs() {
        // Wayfarer: v0.7.1 shipped only the addon, and v1.0.0 added five voice and radio packs,
        // each twenty times its size.
        let mut probe = ScriptedProbe::default();
        let mut assets = Vec::new();
        for (at, pack) in ["Narrator", "Horde", "Alliance", "Shared", "Radio"]
            .iter()
            .enumerate()
        {
            let name = format!("Wayfarer_Voices_{pack}-1.0.0.zip");
            let folder = format!("Wayfarer_Voices_{pack}");
            probe
                .zips
                .insert(name.clone(), Ok(scan_of(&[folder.as_str()], pack)));
            assets.push(sized_asset(&name, 600 - u64::try_from(at).unwrap() * 10));
        }
        probe.zips.insert(
            "Wayfarer-1.0.0.zip".into(),
            Ok(scan_of(&["Wayfarer"], "Wayfarer")),
        );
        assets.push(sized_asset("Wayfarer-1.0.0.zip", 27));
        probe.zips.insert(
            "Wayfarer-0.7.1.zip".into(),
            Ok(scan_of(&["Wayfarer"], "Wayfarer")),
        );
        let mut info =
            repo_with_releases(&[("v1.0.0", "unused.zip"), ("v0.7.1", "Wayfarer-0.7.1.zip")]);
        info.releases[0].assets = assets;
        let last = listed(&[("gh:a/Addon", &["Wayfarer"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let entry =
            process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default()).unwrap();
        assert_eq!(version_of(&entry), "v1.0.0");
        assert_eq!(entry.addon.folders, ["Wayfarer"]);
        assert_eq!(
            probe.reads, 1,
            "its own zip, named like it, was the first one read"
        );
    }

    #[test]
    fn the_pinned_addons_zip_is_found_past_five_bigger_ones_whatever_its_name() {
        // Wayfarer's shape, with the addon's own zip named for the release.
        let mut probe = ScriptedProbe::default();
        let mut assets = Vec::new();
        for (at, pack) in ["Narrator", "Horde", "Alliance", "Shared", "Radio"]
            .iter()
            .enumerate()
        {
            let name = format!("Voices_{pack}-1.0.0.zip");
            let folder = format!("Wayfarer_Voices_{pack}");
            probe
                .zips
                .insert(name.clone(), Ok(scan_of(&[folder.as_str()], pack)));
            assets.push(sized_asset(&name, 600 - u64::try_from(at).unwrap() * 10));
        }
        probe.zips.insert(
            "Release-1.0.0.zip".into(),
            Ok(scan_of(&["Wayfarer"], "Wayfarer")),
        );
        assets.push(sized_asset("Release-1.0.0.zip", 27));
        probe.zips.insert(
            "Release-0.7.1.zip".into(),
            Ok(scan_of(&["Wayfarer"], "Wayfarer")),
        );
        let mut info =
            repo_with_releases(&[("v1.0.0", "unused.zip"), ("v0.7.1", "Release-0.7.1.zip")]);
        info.releases[0].assets = assets;
        let last = listed(&[("gh:a/Addon", &["Wayfarer"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let mut versions = Vec::new();
        for _ in 0..3 {
            let mut tally = Tally::default();
            let entry = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally).unwrap();
            versions.push(version_of(&entry).to_string());
            assert!(tally.left_unread.is_empty());
        }
        assert_eq!(versions, ["v1.0.0", "v1.0.0", "v1.0.0"]);
        assert_eq!(
            probe.reads, 6,
            "the five packs and its own, on the first run only"
        );
        assert!(!record.incomplete);
    }

    /// A release of `count` zips named for Forever, each another addon's, the first biggest.
    fn other_addons_release(probe: &mut ScriptedProbe, tag: &str, count: u64) -> GhRelease {
        let mut release = repo_with_release(tag, "unused.zip").releases.remove(0);
        release.assets = (0..count)
            .map(|at| {
                let name = format!("Other-{tag}-{at}-forever.zip");
                let folder = format!("Other{at}");
                probe
                    .zips
                    .insert(name.clone(), Ok(scan_of(&[folder.as_str()], "Other")));
                sized_asset(&name, 600 - at)
            })
            .collect();
        release
    }

    #[test]
    fn a_newer_release_of_six_other_addons_zips_is_read_once_and_settled() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let mut info = repo_with_releases(&[("voices-v2", "unused.zip"), ("v1", "Addon-1.zip")]);
        let voices = other_addons_release(&mut probe, "voices-v2", 6);
        info.releases[0].assets = voices.assets;
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let mut reads = Vec::new();
        for _ in 0..3 {
            let entry =
                process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default())
                    .unwrap();
            assert_eq!(entry.addon.folders, ["Addon"]);
            assert!(!record.incomplete);
            reads.push(probe.reads);
        }
        assert_eq!(
            reads,
            [7, 7, 7],
            "all six and the addon's own, then nothing"
        );
        assert_eq!(
            record.releases["voices-v2"].skipped_for.as_deref(),
            Some("Addon")
        );
    }

    #[test]
    fn fifteen_releases_of_other_addons_zips_cost_reads_once_not_every_run() {
        // A listed entry's repository fills its fifteen newest releases with six zips of other
        // folders each, and then does nothing more.
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        process_pinned(
            &mut probe,
            &repo_with_releases(&[("v1", "Addon-1.zip")]),
            &mut record,
            &pins,
            &mut Tally::default(),
        )
        .unwrap();
        let mut info = repo_with_releases(&[]);
        info.releases = (0..RELEASES_FETCHED as u64)
            .map(|n| {
                let mut release = other_addons_release(&mut probe, &format!("x{n}"), 6);
                release.published += i64::try_from(n).unwrap() + 10_000;
                release
            })
            .collect();
        let mut per_run = Vec::new();
        for _ in 0..3 {
            let before = probe.reads;
            let entry =
                process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default());
            assert_eq!(entry.unwrap().addon.folders, ["Addon"], "kept listed");
            assert!(!record.incomplete);
            per_run.push(probe.reads - before);
        }
        assert_eq!(per_run, [90, 0, 0]);
    }

    #[test]
    fn a_release_past_twenty_other_addons_zips_is_settled_and_named_with_what_was_left() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let mut info = repo_with_releases(&[("voices-v2", "unused.zip"), ("v1", "Addon-1.zip")]);
        info.releases[0].assets = other_addons_release(&mut probe, "voices-v2", 26).assets;
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        for run in 0..2 {
            let mut tally = Tally::default();
            let entry = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally).unwrap();
            assert_eq!(version_of(&entry), "v1");
            assert_eq!(
                tally.left_unread,
                [
                    "gh:a/Addon passed over voices-v2 with 6 of its zips unread and no Forever build of Addon among the ones read. It isn't read again until its files or this entry's main folder change"
                ],
                "run {run}"
            );
        }
        assert_eq!(
            probe.reads, 21,
            "twenty of the release and the addon's own, once"
        );
        assert!(!record.incomplete);
        assert_eq!(record.releases["voices-v2"].unread, 6);
    }

    #[test]
    fn former_main_folders_go_into_formerly_as_the_design_says() {
        let mut w = world();
        // Questie, with no pin to keep it first, puts a new folder first and still ships the
        // old main one. A repository can hold several addons, so that's for a person to say.
        w.repos[0].addon.folders = vec!["QuestieCore".into(), "Questie".into(), "QuestieDB".into()];
        // PeaversCommons is a GitHub entry whose new build doesn't ship its old main folder,
        // which could just as well be another addon of the repository's.
        w.repos[3].addon.folders = vec!["PeaversKit".into()];
        // TomTom's listing renames its folder, and a listing holds one addon.
        w.wowi[0].addon.folders = vec!["TomTomForever".into()];
        // A person wrote down Helpers' old name.
        w.curated.pins = pinned_by_hand("gh:zebra/Helpers", "Helpers", &["HelpersOld"]).pins;
        w.previous.addons[1].formerly = vec!["DetailsOld".into()];
        let (published, _dir) = w.build(vec![]);
        assert!(
            addon_of(&published, "gh:Questie/Questie")
                .formerly
                .is_empty()
        );
        assert!(
            addon_of(&published, "gh:Peavers/PeaversCommons")
                .formerly
                .is_empty()
        );
        assert_eq!(addon_of(&published, "wowi:7032").formerly, ["TomTom"]);
        assert_eq!(
            addon_of(&published, "gh:zebra/Helpers").formerly,
            ["HelpersOld"]
        );
        assert_eq!(
            addon_of(&published, "gh:Tercioo/Details").formerly,
            ["DetailsOld"],
            "carried over from the last catalog"
        );

        let settled = w.settle(vec![]);
        assert_eq!(
            settled.vouched,
            ["wowi:7032 was TomTom", "gh:zebra/Helpers was HelpersOld"]
        );
        assert_eq!(
            settled.moved,
            [
                "gh:Questie/Questie lists QuestieCore first now, not Questie. If they're the same addon, write was = [\"Questie\"] under [pins.\"gh:Questie/Questie\"] in curated.toml",
                "gh:Peavers/PeaversCommons lists PeaversKit first now, not PeaversCommons. If they're the same addon, write was = [\"PeaversCommons\"] under [pins.\"gh:Peavers/PeaversCommons\"] in curated.toml"
            ]
        );
    }

    #[test]
    fn an_update_that_adds_folders_is_still_taken_with_the_pinned_folder_first() {
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        for folders in [
            &["Addon", "Addon_Extra"][..],
            &["Addon_Options", "Addon"][..],
        ] {
            let mut probe = ScriptedProbe::default();
            probe
                .zips
                .insert("Addon-2.zip".into(), Ok(scan_of(folders, "Good Thing")));
            probe
                .zips
                .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
            let info = repo_with_releases(&[("v2", "Addon-2.zip"), ("v1", "Addon-1.zip")]);
            let mut tally = Tally::default();
            let entry = process_pinned(
                &mut probe,
                &info,
                &mut RepoState::default(),
                &pins,
                &mut tally,
            )
            .unwrap();
            assert_eq!(version_of(&entry), "v2", "{folders:?}");
            assert_eq!(entry.addon.folders[0], "Addon", "{folders:?}");
            assert_eq!(entry.addon.folders.len(), 2);
            assert_eq!(probe.reads, 1, "the older release wasn't needed");
        }
    }

    /// A zip with these folders, each with a TOC for Forever titled like the folder.
    fn zip_of(folders: &[&str]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for folder in folders {
            writer
                .start_file(
                    format!("{folder}/{folder}.toc"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(format!("## Interface: 16001\n## Title: {folder}\n").as_bytes())
                .unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// A full look at a repository through the real scan, and the settle step after it, with
    /// `previous` as the last catalog. Returns the settled entry and the summary lines.
    fn look_and_settle(
        probe: &mut ZipFiles,
        info: &RepoInfo,
        previous: &Catalog,
        curated: &Curated,
    ) -> (Candidate, Tally, Settled) {
        let pins = Pins::new(Some(previous), curated);
        let look = Look {
            asked: &info.name,
            picked: false,
            pins: &pins,
            out_of_time: &|| false,
        };
        let mut tally = Tally::default();
        let entry = process_repo(probe, info, &mut RepoState::default(), &look, &mut tally);
        let settled = settle(
            Scanned {
                repos: entry.into_iter().collect(),
                ..Scanned::default()
            },
            curated,
            &HandPicked::default(),
            Some(previous),
            &Remembered::default(),
            BUILT_AT,
        );
        let id = format!("gh:{}", info.name);
        let entry = entry_of(&settled.candidates, &id).clone();
        (entry, tally, settled)
    }

    fn catalog_with(addon: &Addon) -> Catalog {
        Catalog {
            schema: CATALOG_SCHEMA,
            addons: vec![addon.clone()],
            ..Catalog::default()
        }
    }

    #[test]
    fn a_bundle_of_two_addons_keeps_the_entry_on_its_own_and_vouches_for_neither() {
        // Ready2Quest's repository is listed as QuestCompanion. One release bundles both, and the
        // scan would put the folder named like the repository first. Then Ready2Quest releases
        // on its own.
        const R2Q: &str = "LapassetAlexis/Ready2Quest";
        let mut probe = ZipFiles::default();
        probe.zips.insert(
            "Ready2Quest-2.0.zip".into(),
            zip_of(&["Ready2Quest", "QuestCompanion"]),
        );
        probe
            .zips
            .insert("Ready2Quest-3.0.zip".into(), zip_of(&["Ready2Quest"]));
        let mut bundle = repo_with_releases(&[("v2.0", "Ready2Quest-2.0.zip")]);
        bundle.name = R2Q.into();
        let last = listed(&[("gh:LapassetAlexis/Ready2Quest", &["QuestCompanion"])]);
        let (first, _, settled) = look_and_settle(&mut probe, &bundle, &last, &Curated::default());
        assert_eq!(first.addon.folders, ["QuestCompanion", "Ready2Quest"]);
        assert_eq!(first.addon.name, "QuestCompanion", "from its own TOC");
        assert!(first.addon.formerly.is_empty());
        assert!(settled.vouched.is_empty() && settled.moved.is_empty());

        // The next build reads the catalog this one published.
        let mut alone = repo_with_releases(&[
            ("v3.0", "Ready2Quest-3.0.zip"),
            ("v2.0", "Ready2Quest-2.0.zip"),
        ]);
        alone.name = R2Q.into();
        let (second, tally, _) = look_and_settle(
            &mut probe,
            &alone,
            &catalog_with(&first.addon),
            &Curated::default(),
        );
        assert_eq!(version_of(&second), "v2.0");
        assert_eq!(second.addon.folders, ["QuestCompanion", "Ready2Quest"]);
        assert!(second.addon.formerly.is_empty());
        assert_eq!(
            tally.passed_over,
            [
                "gh:LapassetAlexis/Ready2Quest lists v2.0, passed over newer v3.0, which doesn't ship QuestCompanion. If v3.0 holds the addon this entry should list, set [pins.\"gh:LapassetAlexis/Ready2Quest\"] in curated.toml"
            ]
        );
    }

    #[test]
    fn a_main_folder_moved_in_two_steps_waits_for_a_person_to_say_so() {
        // HogHeals becomes HogTron: first a build with both, then one with HogTron alone.
        let mut probe = ZipFiles::default();
        probe
            .zips
            .insert("HogTron-0.2.zip".into(), zip_of(&["HogTron", "HogHeals"]));
        probe
            .zips
            .insert("HogTron-0.3.zip".into(), zip_of(&["HogTron"]));
        let mut both = repo_with_releases(&[("v0.2", "HogTron-0.2.zip")]);
        both.name = "hog/HogTron".into();
        let last = listed(&[("gh:hog/HogTron", &["HogHeals"])]);
        let (first, _, _) = look_and_settle(&mut probe, &both, &last, &Curated::default());
        assert_eq!(first.addon.folders, ["HogHeals", "HogTron"]);
        assert!(first.addon.formerly.is_empty());

        let mut moved =
            repo_with_releases(&[("v0.3", "HogTron-0.3.zip"), ("v0.2", "HogTron-0.2.zip")]);
        moved.name = "hog/HogTron".into();
        let published = catalog_with(&first.addon);
        let (second, tally, _) =
            look_and_settle(&mut probe, &moved, &published, &Curated::default());
        assert_eq!(version_of(&second), "v0.2", "HogTron alone is passed over");
        assert!(
            tally.passed_over[0].starts_with(
                "gh:hog/HogTron lists v0.2, passed over newer v0.3, which doesn't ship HogHeals."
            ),
            "{:?}",
            tally.passed_over
        );

        // A person says it's the same addon.
        let curated = pinned_by_hand("gh:hog/HogTron", "HogTron", &["HogHeals"]);
        let (third, tally, settled) = look_and_settle(&mut probe, &moved, &published, &curated);
        assert_eq!(version_of(&third), "v0.3");
        assert_eq!(third.addon.folders, ["HogTron"]);
        assert_eq!(third.addon.formerly, ["HogHeals"]);
        assert!(tally.passed_over.is_empty());
        assert_eq!(settled.vouched, ["gh:hog/HogTron was HogHeals"]);
        assert!(settled.moved.is_empty(), "a person has said so");
    }

    // Release lines.

    const SPOKEN: &str = "rusty-key/spoken-wow";

    fn spoken_split() -> SplitSpec {
        SplitSpec {
            repo: SPOKEN.into(),
            main: "spoken".into(),
            lines: vec!["quests-audio".into(), "bosses-audio".into()],
        }
    }

    /// A release under a prefix, as the fetch hands it over: the prefix already cut off.
    fn line_release(prefix: &str, version: &str, zip: &str, age: i64, downloads: u64) -> GhRelease {
        GhRelease {
            tag: version.into(),
            published: detect::FOREVER_EPOCH + 100_000 - age,
            assets: vec![GhAsset {
                name: zip.into(),
                size: 10,
                downloads,
                url: format!(
                    "https://github.com/{SPOKEN}/releases/download/{prefix}/{version}/{zip}"
                ),
                sha256: Some("c".repeat(64)),
            }],
            ..GhRelease::default()
        }
    }

    fn spoken_info() -> RepoInfo {
        let newest = |tag: &str| GhRelease {
            tag: tag.into(),
            ..GhRelease::default()
        };
        RepoInfo {
            name: SPOKEN.into(),
            description: Some("Spoken reads quests aloud.".into()),
            stars: 87,
            topics: vec!["wow-addon".into()],
            is_fork: true,
            fork_of: Some("someone/original".into()),
            // The repository-wide window, newest first, where the desktop app comes first.
            releases: vec![newest("manager/v0.1.11"), newest("bosses-audio/v1.0.0")],
            ..RepoInfo::default()
        }
    }

    fn spoken_fetched() -> PrefixReleases {
        PrefixReleases::from([
            (
                "spoken".to_string(),
                vec![line_release(
                    "spoken",
                    "v3.8.3",
                    "Spoken-3.8.3.zip",
                    50,
                    6000,
                )],
            ),
            (
                "quests-audio".to_string(),
                vec![
                    line_release(
                        "quests-audio",
                        "v2.5.0",
                        "SpokenQuestsAudioAll-2.5.0.zip",
                        10,
                        1394,
                    ),
                    line_release(
                        "quests-audio",
                        "v2.4.0",
                        "SpokenQuestsAudioAll-2.4.0.zip",
                        90,
                        400,
                    ),
                ],
            ),
            (
                "bosses-audio".to_string(),
                vec![line_release(
                    "bosses-audio",
                    "v1.0.0",
                    "SpokenBossesAudio-1.0.0.zip",
                    5,
                    503,
                )],
            ),
        ])
    }

    fn spoken_probe() -> ScriptedProbe {
        let mut probe = ScriptedProbe::default();
        probe.zips.insert(
            "Spoken-3.8.3.zip".into(),
            Ok(scan_of(&["Spoken", "Spoken_Quests"], "Spoken")),
        );
        let quests = [
            "SpokenQuestsAudioAlliance",
            "SpokenQuestsAudioGossip",
            "SpokenQuestsAudioHorde",
            "SpokenQuestsAudioShared",
        ];
        for zip in [
            "SpokenQuestsAudioAll-2.5.0.zip",
            "SpokenQuestsAudioAll-2.4.0.zip",
        ] {
            probe.zips.insert(
                zip.into(),
                Ok(scan_of(&quests, "Spoken Audio: Quests - Alliance")),
            );
        }
        probe.zips.insert(
            "SpokenBossesAudio-1.0.0.zip".into(),
            Ok(scan_of(&["SpokenBossesAudio"], "Spoken Audio: Bosses")),
        );
        probe
    }

    fn split_once(
        probe: &mut ScriptedProbe,
        fetched: &Result<PrefixReleases, String>,
        record: &mut RepoState,
        pins: &Pins<'_>,
        tally: &mut Tally,
    ) -> Vec<Candidate> {
        let look = Look {
            asked: SPOKEN,
            picked: true,
            pins,
            out_of_time: &|| false,
        };
        process_split(
            probe,
            &spoken_info(),
            &spoken_split(),
            Some(fetched),
            record,
            &look,
            tally,
        )
    }

    fn entry_of<'a>(entries: &'a [Candidate], id: &str) -> &'a Candidate {
        entries
            .iter()
            .find(|c| c.addon.id == id)
            .unwrap_or_else(|| panic!("{id} isn't listed"))
    }

    #[test]
    fn a_split_repository_lists_each_line_under_its_own_id() {
        let mut probe = spoken_probe();
        let mut record = RepoState {
            known_downloads: 34_402,
            ..RepoState::default()
        };
        let mut tally = Tally::default();
        let fetched = Ok(spoken_fetched());
        let entries = split_once(
            &mut probe,
            &fetched,
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        assert_eq!(entries.len(), 3);

        let spoken = entry_of(&entries, "gh:rusty-key/spoken-wow");
        assert_eq!(version_of(spoken), "v3.8.3");
        assert_eq!(spoken.addon.stars, 87);
        assert_eq!(
            spoken.addon.downloads, 6000,
            "only its own zips, not the repository's count that has every pack in it"
        );
        assert_eq!(
            spoken.addon.website.as_deref(),
            Some("https://github.com/rusty-key/spoken-wow")
        );

        let quests = entry_of(&entries, "gh:rusty-key/spoken-wow/quests-audio");
        assert_eq!(
            version_of(quests),
            "v2.5.0",
            "the prefix isn't part of the version"
        );
        assert_eq!(quests.addon.folders.len(), 4);
        assert_eq!(
            quests.addon.downloads, 1790,
            "its own two releases' zips, 1394 and 400"
        );
        assert_eq!(quests.addon.stars, 0, "the repository's stars count once");
        assert_eq!(quests.fork_of, None);
        assert!(quests.topics.is_empty());
        assert_eq!(
            quests.addon.summary, "",
            "the repository's text is Spoken's"
        );
        assert_eq!(
            quests.addon.source,
            Source::Github {
                repo: SPOKEN.into()
            }
        );
        assert_eq!(
            quests.addon.website.as_deref(),
            Some("https://github.com/rusty-key/spoken-wow/releases/tag/quests-audio/v2.5.0")
        );
        let stable = quests.addon.stable.as_ref().unwrap();
        assert!(
            stable
                .url
                .ends_with("/quests-audio/v2.5.0/SpokenQuestsAudioAll-2.5.0.zip")
        );
        assert_eq!(stable.sha256, Some("c".repeat(64)));

        assert_eq!(
            record.newest_tag.as_deref(),
            Some("manager/v0.1.11"),
            "the quick look still notices a release under any prefix"
        );
        assert_eq!(probe.reads, 3, "the desktop app's zips are never read");
        assert!(record.lines["quests-audio"].releases.contains_key("v2.5.0"));
        assert!(record.releases.contains_key("v3.8.3"));

        let again = split_once(
            &mut probe,
            &fetched,
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        assert_eq!(again, entries);
        assert_eq!(probe.reads, 3, "the second look reads nothing");
    }

    #[test]
    fn the_main_entry_never_takes_another_lines_build() {
        let mut probe = spoken_probe();
        probe.zips.insert(
            "Spoken-3.7.0.zip".into(),
            Ok(scan_of(&["Spoken", "Spoken_Quests"], "Spoken")),
        );
        let mut fetched = spoken_fetched();
        fetched.insert(
            "spoken".into(),
            vec![line_release(
                "spoken",
                "v3.7.0",
                "Spoken-3.7.0.zip",
                500,
                10,
            )],
        );
        let mut tally = Tally::default();
        let entries = split_once(
            &mut probe,
            &Ok(fetched),
            &mut RepoState::default(),
            &Pins::default(),
            &mut tally,
        );
        let spoken = entry_of(&entries, "gh:rusty-key/spoken-wow");
        assert_eq!(
            version_of(spoken),
            "v3.7.0",
            "bosses-audio/v1.0.0 is newer, and still not Spoken's"
        );
        assert_eq!(spoken.addon.folders, ["Spoken", "Spoken_Quests"]);
    }

    #[test]
    fn a_line_prefix_is_never_a_prerelease_or_flavor() {
        let split = SplitSpec {
            repo: SPOKEN.into(),
            main: "nightly".into(),
            lines: vec!["classic-audio".into()],
        };
        let fetched = Ok(PrefixReleases::from([
            (
                "nightly".to_string(),
                vec![line_release("nightly", "v1.0.0", "Main.zip", 1, 1)],
            ),
            (
                "classic-audio".to_string(),
                vec![line_release("classic-audio", "v1.0.0", "Pack.zip", 1, 1)],
            ),
        ]));
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Main.zip".into(), Ok(scan_of(&["Main"], "Main")));
        probe
            .zips
            .insert("Pack.zip".into(), Ok(scan_of(&["Pack"], "Pack")));
        let look = Look {
            asked: SPOKEN,
            picked: true,
            pins: &Pins::default(),
            out_of_time: &|| false,
        };
        let mut tally = Tally::default();
        let entries = process_split(
            &mut probe,
            &spoken_info(),
            &split,
            Some(&fetched),
            &mut RepoState::default(),
            &look,
            &mut tally,
        );
        assert_eq!(
            entries.len(),
            2,
            "both read, though classic is a flavor word"
        );
        for entry in &entries {
            assert!(
                entry.addon.stable.is_some(),
                "{}: stable, though nightly isn't",
                entry.addon.id
            );
            assert!(entry.addon.beta.is_none());
        }
    }

    #[test]
    fn one_line_failing_keeps_its_last_known_entry_and_the_rest_carry_on() {
        let mut probe = spoken_probe();
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let first = split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        let before = entry_of(&first, "gh:rusty-key/spoken-wow/quests-audio").clone();

        let mut fetched = spoken_fetched();
        fetched.insert(
            "quests-audio".into(),
            vec![line_release(
                "quests-audio",
                "v2.6.0",
                "SpokenQuestsAudioAll-2.6.0.zip",
                1,
                0,
            )],
        );
        probe.zips.insert(
            "SpokenQuestsAudioAll-2.6.0.zip".into(),
            Err("HTTP 503".into()),
        );
        let entries = split_once(
            &mut probe,
            &Ok(fetched),
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        assert_eq!(
            entry_of(&entries, "gh:rusty-key/spoken-wow/quests-audio"),
            &before
        );
        assert!(record.lines["quests-audio"].incomplete);
        assert!(!record.incomplete, "Spoken itself was read fine");
        assert!(!record.lines["bosses-audio"].incomplete);
        assert!(
            needs_full_look(&record, &spoken_info(), unix_now()),
            "the next run tries the line again"
        );
    }

    #[test]
    fn a_line_without_a_forever_build_doesnt_grey_the_main_entry() {
        let mut probe = spoken_probe();
        probe.zips.insert(
            "SpokenBossesAudio-1.0.0.zip".into(),
            Ok(ZipScan {
                folders: vec!["SpokenBossesAudio".into()],
                ..ZipScan::default()
            }),
        );
        let mut tally = Tally::default();
        let entries = split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut RepoState::default(),
            &Pins::default(),
            &mut tally,
        );
        assert!(
            entries
                .iter()
                .all(|c| !c.addon.id.ends_with("bosses-audio"))
        );
        let spoken = entry_of(&entries, "gh:rusty-key/spoken-wow");
        assert!(!spoken.no_longer_forever);
        assert_eq!(tally.no_build, 1);
    }

    #[test]
    fn a_split_repository_that_cant_be_fetched_keeps_everything_it_had() {
        let mut probe = spoken_probe();
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let first = split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        let kept = split_once(
            &mut probe,
            &Err("HTTP 502".into()),
            &mut record,
            &Pins::default(),
            &mut tally,
        );
        assert_eq!(kept, first);
        assert!(record.incomplete);
        assert!(record.lines.values().all(|line| line.incomplete));
        assert_eq!(tally.kept_last_known, 3);
    }

    #[test]
    fn a_lines_tag_prefix_decides_its_addon_whatever_the_last_catalog_listed() {
        let mut probe = spoken_probe();
        let mut record = RepoState::default();
        split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &Pins::default(),
            &mut Tally::default(),
        );

        // The author moves the bosses pack to a new folder under the same prefix.
        let mut fetched = spoken_fetched();
        fetched.insert(
            "bosses-audio".into(),
            vec![line_release(
                "bosses-audio",
                "v2.0.0",
                "SpokenRaidsAudio-2.0.0.zip",
                1,
                0,
            )],
        );
        probe.zips.insert(
            "SpokenRaidsAudio-2.0.0.zip".into(),
            Ok(scan_of(&["SpokenRaidsAudio"], "Spoken Audio: Raids")),
        );
        let last = listed(&[(
            "gh:rusty-key/spoken-wow/bosses-audio",
            &["SpokenBossesAudio"],
        )]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut tally = Tally::default();
        let entries = split_once(&mut probe, &Ok(fetched), &mut record, &pins, &mut tally);
        let bosses = entry_of(&entries, "gh:rusty-key/spoken-wow/bosses-audio");
        assert_eq!(bosses.addon.folders, ["SpokenRaidsAudio"]);
        assert!(tally.passed_over.is_empty());
    }

    const QUEST_FOLDERS: [&str; 4] = [
        "SpokenQuestsAudioAlliance",
        "SpokenQuestsAudioGossip",
        "SpokenQuestsAudioHorde",
        "SpokenQuestsAudioShared",
    ];

    /// A release as the repository-wide window has it, its whole tag included.
    fn window_release(tag: &str, zip: &str, age: i64) -> GhRelease {
        let (prefix, version) = tag.split_once('/').unwrap();
        let mut release = line_release(prefix, version, zip, age, 0);
        release.tag = tag.into();
        release
    }

    #[test]
    fn a_split_repositorys_own_entry_comes_back_from_the_voice_pack_the_last_catalog_listed() {
        // The builder before [[split]] took the repository's newest Forever release, which was
        // a quests pack for most of October, so the published catalog can list the pack as
        // gh:rusty-key/spoken-wow when this change first runs.
        let mut probe = spoken_probe();
        probe.zips.insert(
            "SpokenQuestsAudioAll-2.6.0.zip".into(),
            Ok(scan_of(&QUEST_FOLDERS, "Spoken Audio: Quests - Alliance")),
        );
        let old_window = RepoInfo {
            releases: vec![
                window_release("quests-audio/v2.6.0", "SpokenQuestsAudioAll-2.6.0.zip", 1),
                window_release("spoken/v3.8.3", "Spoken-3.8.3.zip", 50),
            ],
            ..spoken_info()
        };
        let mut record = RepoState::default();
        let hijacked = process(
            &mut probe,
            &old_window,
            &mut record,
            true,
            &mut Tally::default(),
        )
        .unwrap();
        assert_eq!(hijacked.addon.folders[0], "SpokenQuestsAudioAlliance");
        let last = Catalog {
            schema: CATALOG_SCHEMA,
            addons: vec![hijacked.addon.clone()],
            ..Catalog::default()
        };

        // The first run with [[split]], pins and all.
        let pins = Pins::new(Some(&last), &spoken_curated());
        let mut tally = Tally::default();
        let entries = split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &pins,
            &mut tally,
        );
        let main = entry_of(&entries, "gh:rusty-key/spoken-wow");
        assert_eq!(main.addon.folders, ["Spoken", "Spoken_Quests"]);
        assert_eq!(version_of(main), "v3.8.3");
        assert!(tally.passed_over.is_empty());
        assert!(
            record.releases.values().all(|v| v.skipped_for.is_none()),
            "nothing is cached as another addon's: {:?}",
            record.releases
        );

        // Through settle and the gate: the pack's folder never becomes one Spoken was.
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = vec![spoken_split()];
        w.curated.reserved = QUEST_FOLDERS.iter().map(|f| f.to_string()).collect();
        w.curated.packs = vec![crate::curated::PackSpec {
            addon: "gh:rusty-key/spoken-wow".into(),
            name: "Quest voices".into(),
            needed: true,
            ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
            met_by: vec!["SpokenQuestsAudio".into()],
        }];
        w.previous.addons.push(hijacked.addon.clone());
        let (published, _dir) = w.build(entries);
        let spoken = addon_of(&published, "gh:rusty-key/spoken-wow");
        assert_eq!(spoken.folders[0], "Spoken");
        assert!(spoken.formerly.is_empty(), "{:?}", spoken.formerly);
        assert_eq!(spoken.packs.len(), 1);
        assert_eq!(spoken.packs[0].met_by, ["SpokenQuestsAudio"]);
        let quests = addon_of(&published, "gh:rusty-key/spoken-wow/quests-audio");
        assert_ne!(
            quests.stable, spoken.stable,
            "not the same zip under two ids"
        );
    }

    fn spoken_curated() -> Curated {
        Curated {
            split: vec![spoken_split()],
            ..Curated::default()
        }
    }

    #[test]
    fn an_unchanged_split_repository_still_lists_its_lines_and_answers_for_them() {
        let mut state = State::empty();
        let record = state.note_repo(SPOKEN, "seed");
        let mut tally = Tally::default();
        split_once(
            &mut spoken_probe(),
            &Ok(spoken_fetched()),
            record,
            &Pins::default(),
            &mut tally,
        );
        record.lines.insert("retired".into(), LineState::default());
        let mut heads = heads_of(&[SPOKEN]);
        heads.found.get_mut(SPOKEN).unwrap().releases[0].tag = "manager/v0.1.11".into();
        heads.found.get_mut(SPOKEN).unwrap().stars = 90;

        let mut tally = Tally::default();
        let (entries, to_read) = triage_heads(
            &heads,
            &mut state,
            &spoken_curated(),
            &HandPicked::default(),
            unix_now(),
            &mut tally,
        );
        assert!(to_read.is_empty());
        let ids: Vec<&str> = entries.iter().map(|c| c.addon.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gh:rusty-key/spoken-wow",
                "gh:rusty-key/spoken-wow/quests-audio",
                "gh:rusty-key/spoken-wow/bosses-audio"
            ],
            "and nothing for a line that isn't configured any more"
        );
        assert_eq!(entries[0].addon.stars, 90);
        assert_eq!(entries[1].addon.stars, 0);
        for id in [
            "gh:rusty-key/spoken-wow",
            "gh:rusty-key/spoken-wow/quests-audio",
            "gh:rusty-key/spoken-wow/bosses-audio",
        ] {
            assert!(tally.answered.contains(id), "{id}");
        }
    }

    #[test]
    fn a_split_repository_that_failed_or_went_answers_for_its_lines_the_same_way() {
        let curated = spoken_curated();
        let mut state = State::empty();
        let record = state.note_repo(SPOKEN, "seed");
        split_once(
            &mut spoken_probe(),
            &Ok(spoken_fetched()),
            record,
            &Pins::default(),
            &mut Tally::default(),
        );

        let failed = BatchResult {
            failed: vec![SPOKEN.into()],
            ..BatchResult::default()
        };
        let mut tally = Tally::default();
        let (entries, _) = triage_heads(
            &failed,
            &mut state,
            &curated,
            &HandPicked::default(),
            1,
            &mut tally,
        );
        assert_eq!(entries.len(), 3, "the last-known entries of all three");
        assert!(tally.answered.is_empty(), "a failure answers nothing");

        let gone = BatchResult {
            gone: vec![SPOKEN.into()],
            ..BatchResult::default()
        };
        let mut tally = Tally::default();
        triage_heads(
            &gone,
            &mut state,
            &curated,
            &HandPicked::default(),
            1,
            &mut tally,
        );
        assert!(
            tally
                .answered
                .contains("gh:rusty-key/spoken-wow/quests-audio")
        );
        assert!(
            tally
                .answered
                .contains("gh:rusty-key/spoken-wow/bosses-audio")
        );
        assert!(!state.repos.contains_key(SPOKEN));
    }

    #[test]
    fn a_line_never_read_gets_its_repository_read_even_when_nothing_new_was_released() {
        // Spoken as the last build left it, before [[split]] was written.
        let mut state = State::empty();
        let record = state.note_repo(SPOKEN, "seed");
        record.entry = Some(real_entry(SPOKEN, "Spoken", &["Spoken"], 1));
        record.newest_tag = Some("manager/v0.1.11".into());
        record.looked_at = unix_now();
        let mut heads = heads_of(&[SPOKEN]);
        heads.found.get_mut(SPOKEN).unwrap().releases[0].tag = "manager/v0.1.11".into();

        let read = |state: &mut State, curated: &Curated| {
            let mut tally = Tally::default();
            let (_, to_read) = triage_heads(
                &heads,
                state,
                curated,
                &HandPicked::default(),
                unix_now(),
                &mut tally,
            );
            to_read
        };
        assert!(
            read(&mut state, &Curated::default()).is_empty(),
            "unsplit, nothing has changed"
        );
        assert_eq!(read(&mut state, &spoken_curated()), [SPOKEN]);

        let record = state.repos.get_mut(SPOKEN).unwrap();
        for line in &spoken_split().lines {
            record.lines.insert(line.clone(), LineState::default());
        }
        assert!(
            read(&mut state, &spoken_curated()).is_empty(),
            "once each line has been read, it's quiet again"
        );
    }

    #[test]
    fn a_split_repository_renamed_on_github_keeps_what_it_had_until_the_config_follows() {
        const NEW: &str = "rusty-key/spoken";
        let curated = spoken_curated();
        let mut state = State::empty();
        let record = state.note_repo(SPOKEN, "seed");
        split_once(
            &mut spoken_probe(),
            &Ok(spoken_fetched()),
            record,
            &Pins::default(),
            &mut Tally::default(),
        );

        // GitHub answers for the old name with the new one, and [[split]] still has the old.
        // Read under the new name, it would be one addon again: whichever released last.
        let mut heads = heads_of(&[SPOKEN]);
        heads.found.get_mut(SPOKEN).unwrap().name = NEW.into();
        let mut tally = Tally::default();
        let (entries, to_read) = triage_heads(
            &heads,
            &mut state,
            &curated,
            &HandPicked::default(),
            unix_now(),
            &mut tally,
        );
        assert!(to_read.is_empty(), "never read unsplit under the new name");
        let ids: Vec<&str> = entries.iter().map(|c| c.addon.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gh:rusty-key/spoken-wow",
                "gh:rusty-key/spoken-wow/quests-audio",
                "gh:rusty-key/spoken-wow/bosses-audio"
            ]
        );
        assert_eq!(state.repos[SPOKEN].lines.len(), 2, "its lines stay too");
        assert!(!state.repos.contains_key(NEW));
        assert!(tally.answered.is_empty(), "like a fetch that failed");
        assert_eq!(tally.kept_last_known, 3);
        assert_eq!(
            tally.renamed_splits,
            [
                "rusty-key/spoken-wow is rusty-key/spoken on GitHub now. It keeps what it had until seeds.toml and curated.toml use the new name."
            ]
        );

        // Renamed between the quick look and the full one, it's kept the same way.
        let mut batch = BatchResult::default();
        batch.found.insert(
            SPOKEN.into(),
            RepoInfo {
                name: NEW.into(),
                ..spoken_info()
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver {
            path: dir.path().join("state.json"),
            last: Instant::now(),
        };
        let mut tally = Tally::default();
        let entries = read_releases(
            &batch,
            &SplitReleases::new(),
            &mut state,
            &curated,
            &Pins::default(),
            &mut spoken_probe(),
            &|| false,
            &mut saver,
            &mut tally,
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(state.repos[SPOKEN].lines.len(), 2);
        assert!(tally.answered.is_empty());

        // Once the config has the new name, it's read under that one as usual.
        let followed = Curated {
            split: vec![SplitSpec {
                repo: NEW.into(),
                ..spoken_split()
            }],
            ..Curated::default()
        };
        let (_, to_read) = triage_heads(
            &heads,
            &mut state,
            &followed,
            &HandPicked::default(),
            unix_now(),
            &mut Tally::default(),
        );
        assert_eq!(to_read, [NEW]);
        assert_eq!(state.repos[NEW].lines.len(), 2, "its lines moved with it");
    }

    #[test]
    fn listed_lines_get_the_daily_relook_when_the_main_has_no_build() {
        // A line's file can be replaced under the same tag, which only a full look notices.
        let mut probe = spoken_probe();
        probe.zips.insert(
            "Spoken-3.8.3.zip".into(),
            Ok(ZipScan {
                folders: vec!["Spoken".into()],
                ..ZipScan::default()
            }),
        );
        let mut record = RepoState::default();
        let entries = split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &Pins::default(),
            &mut Tally::default(),
        );
        assert!(
            record.entry.is_none(),
            "the main prefix has no Forever build"
        );
        assert_eq!(entries.len(), 2, "both lines are listed");
        let head = RepoInfo {
            releases: vec![GhRelease {
                tag: record.newest_tag.clone().unwrap(),
                ..GhRelease::default()
            }],
            ..RepoInfo::default()
        };
        assert!(!needs_full_look(&record, &head, record.looked_at + 1));
        assert!(needs_full_look(
            &record,
            &head,
            record.looked_at + RELOOK_LISTED_SECS
        ));
        assert!(
            needs_full_look(&record, &RepoInfo::default(), record.looked_at),
            "and with its releases gone, a look takes the lines away"
        );
    }

    #[test]
    fn the_full_look_reads_a_split_repository_by_prefix_and_answers_line_by_line() {
        let curated = spoken_curated();
        let mut state = State::empty();
        state.note_repo(SPOKEN, "seed");
        let mut batch = BatchResult::default();
        batch.found.insert(SPOKEN.into(), spoken_info());
        let mut probe = spoken_probe();
        probe
            .zips
            .insert("SpokenBossesAudio-1.0.0.zip".into(), Err("HTTP 503".into()));
        let split_releases = SplitReleases::from([(SPOKEN.to_string(), Ok(spoken_fetched()))]);
        let dir = tempfile::tempdir().unwrap();
        let mut saver = Saver {
            path: dir.path().join("state.json"),
            last: Instant::now(),
        };
        let mut tally = Tally::default();
        let entries = read_releases(
            &batch,
            &split_releases,
            &mut state,
            &curated,
            &Pins::default(),
            &mut probe,
            &|| false,
            &mut saver,
            &mut tally,
        );
        assert_eq!(entries.len(), 2);
        assert!(tally.answered.contains("gh:rusty-key/spoken-wow"));
        assert!(
            tally
                .answered
                .contains("gh:rusty-key/spoken-wow/quests-audio")
        );
        assert!(
            !tally
                .answered
                .contains("gh:rusty-key/spoken-wow/bosses-audio"),
            "its read failed"
        );

        // Taken out of [[split]], the repository is one addon again, and its lines' results go.
        let mut tally = Tally::default();
        read_releases(
            &batch,
            &SplitReleases::new(),
            &mut state,
            &Curated::default(),
            &Pins::default(),
            &mut probe,
            &|| false,
            &mut saver,
            &mut tally,
        );
        assert!(state.repos[SPOKEN].lines.is_empty());
    }

    /// Reads zips for real, from memory, through the same scan the live builder uses.
    #[derive(Default)]
    struct ZipFiles {
        zips: BTreeMap<String, Vec<u8>>,
    }

    impl Probe for ZipFiles {
        fn release_json(&mut self, _: &GhAsset) -> Result<String, String> {
            Err("not used".to_string())
        }
        fn read_zip(
            &mut self,
            asset: &GhAsset,
            hints: &[&str],
            pin: Option<&str>,
        ) -> Result<ZipScan, String> {
            let bytes = self.zips.get(&asset.name).ok_or("no such zip")?;
            scan_zip(std::io::Cursor::new(bytes.clone()), hints, pin).map_err(|e| e.to_string())
        }
    }

    /// The quests pack as it really is: four folders, each with a TOC naming only itself.
    fn quests_pack_zip() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for part in ["Alliance", "Horde", "Shared", "Gossip"] {
            let folder = format!("SpokenQuestsAudio{part}");
            let title = if part == "Gossip" {
                "Gossip".to_string()
            } else {
                format!("Quests - {part}")
            };
            writer
                .start_file(
                    format!("{folder}/{folder}.toc"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    format!(
                        "## Interface: 11509, 20506, 16001\n## Title: |cffb48cffSpoken|r Audio: {title}\n## Notes: Narrated quests and gossip from the Vanilla era, for Spoken Quests.\n## LoadOnDemand: 1\n\nModule.lua\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            writer
                .start_file(
                    format!("{folder}/Module.lua"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"-- registers the voices\n").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn the_quests_pack_goes_by_its_hand_given_name_and_lists_all_four_folders() {
        let mut probe = ZipFiles::default();
        probe
            .zips
            .insert("SpokenQuestsAudioAll-2.5.0.zip".into(), quests_pack_zip());
        let split = SplitSpec {
            repo: SPOKEN.into(),
            main: "spoken".into(),
            lines: vec!["quests-audio".into()],
        };
        let fetched = Ok(PrefixReleases::from([
            ("spoken".to_string(), vec![]),
            (
                "quests-audio".to_string(),
                vec![line_release(
                    "quests-audio",
                    "v2.5.0",
                    "SpokenQuestsAudioAll-2.5.0.zip",
                    1,
                    1394,
                )],
            ),
        ]));
        let look = Look {
            asked: SPOKEN,
            picked: true,
            pins: &Pins::default(),
            out_of_time: &|| false,
        };
        let entries = process_split(
            &mut probe,
            &spoken_info(),
            &split,
            Some(&fetched),
            &mut RepoState::default(),
            &look,
            &mut Tally::default(),
        );
        let quests = entry_of(&entries, "gh:rusty-key/spoken-wow/quests-audio");
        assert_eq!(
            quests.addon.name, "Spoken Audio: Quests - Alliance",
            "the zip's name runs into the Alliance folder's, so its TOC is the one read"
        );
        let mut folders = quests.addon.folders.clone();
        assert_eq!(folders[0], "SpokenQuestsAudioAlliance", "the main folder");
        folders.sort();
        assert_eq!(
            folders,
            [
                "SpokenQuestsAudioAlliance",
                "SpokenQuestsAudioGossip",
                "SpokenQuestsAudioHorde",
                "SpokenQuestsAudioShared"
            ]
        );

        let curated = Curated {
            split: vec![split],
            overrides: BTreeMap::from([(
                "gh:rusty-key/spoken-wow/quests-audio".to_string(),
                Override {
                    name: Some("Spoken Audio: Quests".into()),
                    summary: Some("English voices for Spoken.".into()),
                    category: Some("quests".into()),
                    boost: None,
                },
            )]),
            ..Curated::default()
        };
        let picks = HandPicked::new(&Seeds::default(), &curated);
        let scanned = Scanned {
            repos: entries,
            ..Scanned::default()
        };
        let settled = settle(
            scanned,
            &curated,
            &picks,
            None,
            &Remembered::default(),
            BUILT_AT,
        );
        let quests = entry_of(&settled.candidates, "gh:rusty-key/spoken-wow/quests-audio");
        assert_eq!(quests.addon.name, "Spoken Audio: Quests");
        assert_eq!(quests.addon.summary, "English voices for Spoken.");
        assert_eq!(quests.addon.category, "quests");
        assert_eq!(quests.addon.folders.len(), 4);
        assert!(quests.addon.reviewed && quests.addon.trusted);
    }

    #[test]
    fn spokens_packs_are_listed_reviewed_and_offered_on_its_page_and_their_folders_are_its_own() {
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = vec![spoken_split()];
        w.curated.reserved = vec![
            "SpokenQuestsAudioAlliance".into(),
            "SpokenBossesAudio".into(),
        ];
        w.curated.packs = vec![
            crate::curated::PackSpec {
                addon: "gh:rusty-key/spoken-wow".into(),
                name: "Quest voices".into(),
                needed: true,
                ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
                met_by: vec!["SpokenQuestsAudio".into()],
            },
            crate::curated::PackSpec {
                addon: "gh:rusty-key/spoken-wow".into(),
                name: "Boss voices".into(),
                needed: false,
                ids: vec!["gh:rusty-key/spoken-wow/bosses-audio".into()],
                met_by: vec!["SpokenBossesAudio".into()],
            },
        ];
        for (line, name) in [
            ("quests-audio", "Spoken Audio: Quests"),
            ("bosses-audio", "Spoken Audio: Bosses"),
        ] {
            w.curated.overrides.insert(
                SplitSpec::line_id(SPOKEN, line),
                Override {
                    name: Some(name.into()),
                    ..Override::default()
                },
            );
        }
        let spoken = real_entry(SPOKEN, "Spoken", &["Spoken", "Spoken_Quests"], 6000);
        w.previous.addons.push(spoken.addon.clone());
        let line = |id: &str, title: &str, folders: &[&str]| {
            let mut c = addon_candidate(id, title, 1394);
            c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
            c.addon.source = Source::Github {
                repo: SPOKEN.into(),
            };
            // Small, and known to be.
            c.addon.stable.as_mut().unwrap().size = 40_000_000;
            c
        };
        let quests = line(
            "gh:rusty-key/spoken-wow/quests-audio",
            "Spoken Audio: Quests - Alliance",
            &["SpokenQuestsAudioAlliance", "SpokenQuestsAudioGossip"],
        );
        let bosses = line(
            "gh:rusty-key/spoken-wow/bosses-audio",
            "Spoken Audio: Bosses",
            &["SpokenBossesAudio"],
        );
        let squatter = claim_of(
            "zebra/Voices",
            "Free Voices",
            &["SpokenQuestsAudioGossip"],
            1,
        );
        let namesake = claim_of("zebra/Quests", "Spoken Audio: Quests", &["ZebraQuests"], 1);
        let (published, _dir) = w.build(vec![spoken, quests, bosses, squatter, namesake]);

        for id in [
            "gh:rusty-key/spoken-wow/quests-audio",
            "gh:rusty-key/spoken-wow/bosses-audio",
        ] {
            let listed = addon_of(&published, id);
            assert!(listed.reviewed && listed.trusted, "{id}");
            assert!(!is_held(&published, id));
        }
        assert_eq!(
            addon_of(&published, "gh:rusty-key/spoken-wow/quests-audio").name,
            "Spoken Audio: Quests"
        );
        let packs = &addon_of(&published, "gh:rusty-key/spoken-wow").packs;
        assert_eq!(packs.len(), 2);
        assert!(packs[0].needed && !packs[1].needed);
        assert!(
            is_held(&published, "gh:zebra/Voices"),
            "a pack's folder is the pack's"
        );
        assert!(is_held(&published, "gh:zebra/Quests"), "and so is its name");
    }

    // What an entry was: vouches, gaps and claims.

    fn held_reason<'a>(published: &'a Published, id: &str) -> &'a str {
        let row = published.held.iter().find(|h| h.id == id);
        row.map_or_else(|| panic!("{id} wasn't held"), |row| row.reason.as_str())
    }

    /// TomTom's listing, which seeds.toml names, renames its folder, so from this build on the
    /// catalog vouches that copies installed as TomTom are this addon.
    fn tomtom_renamed() -> World {
        let mut w = world();
        w.wowi[0].addon.folders = vec!["TomTomForever".into()];
        w
    }

    #[test]
    fn a_strangers_new_repository_shipping_a_renamed_addons_old_folder_is_held() {
        let mut w = tomtom_renamed();
        let (renamed, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&renamed, "wowi:7032").formerly, ["TomTom"]);
        w.after(&renamed);

        // A stranger's brand-new repository ships a folder called TomTom.
        let stranger = || claim_of("zebra/Waypoints", "Zebra Waypoints", &["TomTom"], 1);
        let (squatted, _d1) = w.build(vec![stranger()]);
        assert_eq!(
            held_reason(&squatted, "gh:zebra/Waypoints"),
            "its folder TomTom was wowi:7032's main folder"
        );
        assert_eq!(addon_of(&squatted, "wowi:7032").formerly, ["TomTom"]);
        w.after(&squatted);

        // Excluded, and then gone, the vouch is the same.
        w.curated.exclude.ids.push("gh:zebra/Waypoints".into());
        let (excluded, _d2) = w.build(vec![stranger()]);
        assert_eq!(addon_of(&excluded, "wowi:7032").formerly, ["TomTom"]);
        w.after(&excluded);
        let (gone, _d3) = w.build(vec![]);
        assert_eq!(addon_of(&gone, "wowi:7032").formerly, ["TomTom"]);
    }

    #[test]
    fn a_stranger_waiting_on_the_folder_is_held_when_the_rename_comes() {
        let mut w = world();
        let fork = || claim_of("zebra/TomTomClassic", "TomTom Classic", &["TomTom"], 1);
        let (before, _d0) = w.build(vec![fork()]);
        assert!(
            is_held(&before, "gh:zebra/TomTomClassic"),
            "TomTom's folder"
        );
        w.after(&before);

        // The rename build itself: the folder TomTom moved away from is still TomTom's.
        w.wowi[0].addon.folders = vec!["TomTomForever".into()];
        let (renamed, _d1) = w.build(vec![fork()]);
        assert_eq!(
            held_reason(&renamed, "gh:zebra/TomTomClassic"),
            "its folder TomTom was wowi:7032's main folder"
        );
        assert_eq!(addon_of(&renamed, "wowi:7032").formerly, ["TomTom"]);
    }

    #[test]
    fn a_written_was_stays_vouched_for_with_a_stranger_shipping_it() {
        // Helpers renamed its folder, and a person wrote the old one down.
        let mut w = world();
        w.curated.pins = pinned_by_hand("gh:zebra/Helpers", "HelpersForever", &["Helpers"]).pins;
        let at = w
            .repos
            .iter()
            .position(|c| c.addon.id == "gh:zebra/Helpers")
            .unwrap();
        w.repos[at].addon.folders = vec!["HelpersForever".into()];
        let (first, _d1) = w.build(vec![]);
        assert_eq!(addon_of(&first, "gh:zebra/Helpers").formerly, ["Helpers"]);
        w.after(&first);

        let stranger = claim_of("nobody/helpers-old", "Old Helpers", &["Helpers"], 1);
        let (second, _d2) = w.build(vec![stranger]);
        assert_eq!(
            held_reason(&second, "gh:nobody/helpers-old"),
            "its folder Helpers was gh:zebra/Helpers's main folder"
        );
        assert_eq!(addon_of(&second, "gh:zebra/Helpers").formerly, ["Helpers"]);
    }

    #[test]
    fn a_strangers_new_repository_shipping_the_old_folder_while_the_addon_is_away_is_held() {
        let mut w = tomtom_renamed();
        let (renamed, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&renamed, "wowi:7032").formerly, ["TomTom"]);
        w.after(&renamed);

        // The listing is missing from the next build, and a stranger's new repository ships
        // TomTom. The listing's claim is still there.
        let tomtom = w.wowi.remove(0);
        let stranger = || claim_of("zebra/Waypoints", "Zebra Waypoints", &["TomTom"], 1);
        w.now += DAY_SECS;
        let (gap, _d1) = w.build(vec![stranger()]);
        assert!(!gap.catalog.addons.iter().any(|a| a.id == "wowi:7032"));
        assert_eq!(
            held_reason(&gap, "gh:zebra/Waypoints"),
            "its folder TomTom was wowi:7032's main folder"
        );
        w.after(&gap);

        // Over 30 days after it was last published, the claim is gone, and so the stranger is
        // listed.
        w.now += 30 * DAY_SECS;
        let (later, _d2) = w.build(vec![stranger()]);
        assert!(!is_held(&later, "gh:zebra/Waypoints"));
        w.after(&later);

        // Back again, the listing ranks ahead of the stranger, which is held, and the vouch is
        // back.
        w.wowi.insert(0, tomtom);
        w.now += DAY_SECS;
        let (back, _d3) = w.build(vec![stranger()]);
        assert_eq!(
            held_reason(&back, "gh:zebra/Waypoints"),
            "its folder TomTom was wowi:7032's main folder"
        );
        assert_eq!(addon_of(&back, "wowi:7032").formerly, ["TomTom"]);
    }

    #[test]
    fn an_established_repository_only_now_shipping_the_old_folder_is_held_however_old() {
        // Helpers is from before ages were kept, as TomTom's listing is, and adds a TomTom
        // folder after the rename. Its claim to that folder is from today.
        let mut w = tomtom_renamed();
        let (renamed, _d0) = w.build(vec![]);
        w.after(&renamed);
        let at = w
            .repos
            .iter()
            .position(|c| c.addon.id == "gh:zebra/Helpers")
            .unwrap();
        w.repos[at].addon.folders = vec!["Helpers".into(), "TomTom".into()];
        let (grown, _d1) = w.build(vec![]);
        assert_eq!(
            held_reason(&grown, "gh:zebra/Helpers"),
            "its folder TomTom was wowi:7032's main folder"
        );
        assert_eq!(addon_of(&grown, "wowi:7032").formerly, ["TomTom"]);
    }

    #[test]
    fn a_vouched_folder_goes_to_a_hand_picked_entry_or_a_listed_one_that_ranks_level() {
        // A repository listed beside TomTom from before ages were kept, shipping a TomTom
        // folder. The two are level, so it keeps the folder and the vouch goes.
        let mut w = tomtom_renamed();
        let pins = real_entry("zebra/Pins", "Map Pins", &["MapPins", "TomTom"], 1);
        w.repos.push(pins.clone());
        w.previous.addons.push(pins.addon.clone());
        let settled = w.settle(vec![]);
        assert_eq!(
            settled.unvouched,
            ["wowi:7032 no longer vouches for TomTom, which gh:zebra/Pins ships"]
        );
        assert!(settled.vouched.is_empty(), "{:?}", settled.vouched);
        let (published, _d0) = w.build(vec![]);
        assert!(addon_of(&published, "wowi:7032").formerly.is_empty());
        assert!(!is_held(&published, "gh:zebra/Pins"));

        // A seeded repository that starts shipping it wins it too.
        let mut w = tomtom_renamed();
        let (renamed, _d1) = w.build(vec![]);
        w.after(&renamed);
        w.seeds.repos.push("zebra/Waypoints".into());
        let seeded = || claim_of("zebra/Waypoints", "Zebra Waypoints", &["TomTom"], 1);
        let settled = w.settle(vec![seeded()]);
        assert_eq!(
            settled.unvouched,
            ["wowi:7032 no longer vouches for TomTom, which gh:zebra/Waypoints ships"]
        );

        // While the listing is away it's listed, and when the listing is back, the vouch goes.
        let tomtom = w.wowi.remove(0);
        let (gap, _d2) = w.build(vec![seeded()]);
        assert!(!is_held(&gap, "gh:zebra/Waypoints"));
        w.after(&gap);
        w.wowi.insert(0, tomtom);
        let settled = w.settle(vec![seeded()]);
        assert_eq!(
            settled.unvouched,
            ["wowi:7032 no longer vouches for TomTom, which gh:zebra/Waypoints ships"]
        );
        let (published, _d3) = w.build(vec![seeded()]);
        assert!(!is_held(&published, "gh:zebra/Waypoints"));
        assert!(addon_of(&published, "wowi:7032").formerly.is_empty());

        // Once it's gone, a person can write the vouch back.
        w.after(&published);
        w.seeds.repos.pop();
        w.curated.pins.insert(
            "wowi:7032".into(),
            crate::curated::PinSpec {
                main: None,
                was: vec!["TomTom".into()],
            },
        );
        let (written_back, _d3) = w.build(vec![]);
        assert_eq!(addon_of(&written_back, "wowi:7032").formerly, ["TomTom"]);
    }

    #[test]
    fn a_vouch_survives_a_build_the_entry_is_missing_from() {
        let mut w = tomtom_renamed();
        let (first, _d1) = w.build(vec![]);
        assert_eq!(addon_of(&first, "wowi:7032").formerly, ["TomTom"]);
        w.after(&first);

        // One build without the listing, then it's back.
        let tomtom = w.wowi.remove(0);
        let (second, _d2) = w.build(vec![]);
        assert!(!second.catalog.addons.iter().any(|a| a.id == "wowi:7032"));
        w.after(&second);
        w.wowi.insert(0, tomtom);
        let (third, _d3) = w.build(vec![]);
        assert_eq!(addon_of(&third, "wowi:7032").formerly, ["TomTom"]);

        // Back under a new folder after the gap, it vouches for the one it had before.
        w.after(&third);
        w.wowi.remove(0);
        let (fourth, _d4) = w.build(vec![]);
        w.after(&fourth);
        let moved = listing_entry(7032, "TomTom", &["TomTomClassic"], 3_550_000);
        w.wowi.insert(0, moved);
        let (fifth, _d5) = w.build(vec![]);
        assert_eq!(
            addon_of(&fifth, "wowi:7032").formerly,
            ["TomTomForever", "TomTom"]
        );
    }

    #[test]
    fn an_entry_left_out_for_a_build_never_comes_back_as_its_repositorys_other_addon() {
        // The addon's own release was deleted, and only its module's is in reach.
        let (mut probe, info) = module_then_addon();
        let only_module = RepoInfo {
            releases: vec![info.releases[0].clone()],
            ..info.clone()
        };
        let catalog = listed(&[("gh:a/Addon", &["Addon"])]);
        let mut remembered = Remembered::default();
        remembered.note(&catalog);
        let mut record = RepoState {
            entry: Some(real_entry("a/Addon", "Good Thing", &["Addon"], 1)),
            ..RepoState::default()
        };
        let pins = Pins::new(Some(&catalog), &Curated::default()).remembering(&remembered);
        let mut tally = Tally::default();
        let left_out = process_pinned(&mut probe, &only_module, &mut record, &pins, &mut tally);
        assert_eq!(left_out, None);
        assert_eq!(
            tally.passed_over,
            [
                "gh:a/Addon is left out, as its last build's file is gone, passed over newer module-v2, which doesn't ship Addon. It stays out until a build that ships Addon is in reach, or [pins.\"gh:a/Addon\"] in curated.toml says which folder it should list"
            ]
        );

        // The catalog that build published doesn't list it, and it stays out, with or without
        // that catalog to read.
        let without = listed(&[("gh:someone/Else", &["Else"])]);
        remembered.note(&without);
        for last in [Some(&without), None] {
            let pins = Pins::new(last, &Curated::default()).remembering(&remembered);
            let mut tally = Tally::default();
            let back = process_pinned(&mut probe, &only_module, &mut record, &pins, &mut tally);
            assert_eq!(back, None, "never the module");
            assert!(
                tally.passed_over[0].starts_with(
                    "gh:a/Addon is left out, with no build of its own to keep, passed over newer module-v2"
                ),
                "{:?}",
                tally.passed_over
            );
        }

        // With its own build in reach again, it's listed again.
        let pins = Pins::new(Some(&without), &Curated::default()).remembering(&remembered);
        let back = process_pinned(&mut probe, &info, &mut record, &pins, &mut Tally::default());
        assert_eq!(
            back.map(|c| c.addon.folders),
            Some(vec!["Addon".to_string()])
        );
    }

    #[test]
    fn packs_over_100_mb_go_out_trusted_but_not_reviewed_and_are_named_once() {
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = vec![spoken_split()];
        w.curated.packs = vec![crate::curated::PackSpec {
            addon: "gh:rusty-key/spoken-wow".into(),
            name: "Quest voices".into(),
            needed: true,
            ids: vec![
                "gh:rusty-key/spoken-wow/quests-audio".into(),
                "wowi:98".into(),
                "wowi:99".into(),
            ],
            met_by: vec!["SpokenQuestsAudio".into()],
        }];
        let sized = |mut candidate: Candidate, size: u64| {
            candidate.addon.stable.as_mut().unwrap().size = size;
            candidate
        };
        let spoken = sized(real_entry(SPOKEN, "Spoken", &["Spoken"], 6000), 72_000_000);
        let mut quests = real_entry(SPOKEN, "Spoken Audio: Quests", &QUEST_FOLDERS, 1394);
        quests.addon.id = SplitSpec::line_id(SPOKEN, "quests-audio");
        let mut bosses = real_entry(SPOKEN, "Spoken Audio: Bosses", &["SpokenBossesAudio"], 503);
        bosses.addon.id = SplitSpec::line_id(SPOKEN, "bosses-audio");
        // Small as a stable build, and over 100 MB as a beta.
        let mut bosses = sized(bosses, 50_000_000);
        let mut beta = bosses.addon.stable.clone().unwrap();
        beta.size = 157_070_901;
        bosses.addon.beta = Some(beta);
        w.wowi.push(sized(
            listing_entry(98, "Small Voices", &["SmallVoices"], 10),
            5_000_000,
        ));
        w.wowi.push(sized(
            listing_entry(99, "Big Voices", &["BigVoices"], 10),
            200_000_000,
        ));
        let extra = vec![spoken, sized(quests, 1_479_966_451), bosses];
        let unreviewed = w.settle(extra.clone()).unreviewed;
        assert!(unreviewed.unknown_size.is_empty(), "every size is known");
        let mut named = unreviewed.big;
        named.sort();
        assert_eq!(
            named,
            [
                "gh:rusty-key/spoken-wow/bosses-audio, a 157070901-byte download",
                "gh:rusty-key/spoken-wow/quests-audio, a 1479966451-byte download",
                "wowi:99, a 200000000-byte download"
            ],
            "each once, though the quests pack is a line and in a group"
        );
        let (published, _dir) = w.build(extra);
        for id in [
            "gh:rusty-key/spoken-wow/quests-audio",
            "gh:rusty-key/spoken-wow/bosses-audio",
            "wowi:99",
        ] {
            let pack = addon_of(&published, id);
            assert!(pack.trusted && !pack.reviewed, "{id}");
        }
        let small = addon_of(&published, "wowi:98");
        assert!(small.trusted && small.reviewed, "under 100 MB");
        let spoken = addon_of(&published, "gh:rusty-key/spoken-wow");
        assert!(spoken.reviewed, "the addon the packs are for isn't a pack");
        assert_eq!(spoken.packs[0].ids.len(), 3, "trusted is all a pack needs");
    }

    #[test]
    fn a_packs_name_without_its_language_is_claimed_too() {
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = vec![spoken_split()];
        w.curated.overrides.insert(
            SplitSpec::line_id(SPOKEN, "quests-audio"),
            Override {
                name: Some("Spoken Audio: Quests (English)".into()),
                ..Override::default()
            },
        );
        let mut quests = real_entry(SPOKEN, "Spoken Audio: Quests - Alliance", &QUEST_FOLDERS, 1);
        quests.addon.id = SplitSpec::line_id(SPOKEN, "quests-audio");
        let (published, _dir) = w.build(vec![
            quests,
            claim_of("zebra/Bare", "Spoken Audio: Quests", &["ZebraBare"], 1),
            claim_of("zebra/Folded", "spoken audio - quests", &["ZebraFolded"], 1),
            claim_of(
                "zebra/English",
                "Spoken Audio: Quests (English)",
                &["ZebraEn"],
                1,
            ),
            claim_of(
                "zebra/German",
                "Spoken Audio: Quests (German)",
                &["ZebraDe"],
                1,
            ),
        ]);
        for id in ["gh:zebra/Bare", "gh:zebra/Folded", "gh:zebra/English"] {
            assert_eq!(
                held_reason(&published, id),
                "its name is already taken by gh:rusty-key/spoken-wow/quests-audio",
                "{id}"
            );
        }
        assert!(!is_held(&published, "gh:zebra/German"), "another name");
    }

    #[test]
    fn the_real_config_claims_each_packs_name_and_lets_community_quest_packs_meet_its_group() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog");
        let real = crate::curated::load_curated(&root.join("curated.toml")).unwrap();
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = real.split.clone();
        w.curated.packs = real.packs.clone();
        w.curated.reserved = real.reserved.clone();
        for (id, edit) in &real.overrides {
            if id.starts_with("gh:rusty-key/") {
                w.curated.overrides.insert(id.clone(), edit.clone());
            }
        }
        let spoken = real_entry(
            SPOKEN,
            "Spoken",
            &[
                "Spoken",
                "SpokenContributions",
                "Spoken_Quests",
                "Spoken_Readables",
                "Spoken_Places",
                "Spoken_Bosses",
            ],
            6000,
        );
        w.previous.addons.push(spoken.addon.clone());
        let line = |name: &str, title: &str, folders: &[&str]| {
            let mut c = real_entry(SPOKEN, title, folders, 100);
            c.addon.id = SplitSpec::line_id(SPOKEN, name);
            c
        };
        let extra = vec![
            spoken,
            line(
                "quests-audio",
                "Spoken Audio: Quests - Alliance",
                &QUEST_FOLDERS,
            ),
            line(
                "places-audio",
                "Spoken Audio: Places",
                &["SpokenPlacesAudio"],
            ),
            line(
                "readables-audio",
                "Spoken Audio: Readables",
                &["SpokenReadablesAudio"],
            ),
            line(
                "bosses-audio",
                "Spoken Audio: Bosses",
                &["SpokenBossesAudio"],
            ),
            // Strangers using the packs' names, bare and as the config spells them. "Spoken
            // Audio: Places" is the Places pack's own TOC title.
            claim_of("zebra/Quests", "Spoken Audio: Quests", &["ZebraQuests"], 1),
            claim_of(
                "zebra/QuestsEn",
                "Spoken Audio: Quests (English)",
                &["ZebraQuestsEn"],
                1,
            ),
            claim_of("zebra/Places", "Spoken Audio: Places", &["ZebraPlaces"], 1),
        ];
        let (published, _dir) = w.build(extra);
        for id in ["gh:zebra/Quests", "gh:zebra/QuestsEn", "gh:zebra/Places"] {
            assert!(is_held(&published, id), "{id}");
        }
        assert_eq!(
            addon_of(&published, "gh:rusty-key/spoken-wow/quests-audio").name,
            "Spoken Audio: Quests (English)"
        );

        // The community packs known today are quest packs, and meet the quest voices group.
        let spoken = addon_of(&published, "gh:rusty-key/spoken-wow");
        assert_eq!(spoken.packs.len(), 4);
        let quests = &spoken.packs[0];
        assert_eq!(quests.name, "Quest voices");
        for folder in [
            "SpokenCommunity_ivrit",
            "SpokenCommunity_polski",
            "SpokenQuestsAudio_deDE",
        ] {
            let lower = folder.to_lowercase();
            assert!(
                quests
                    .met_by
                    .iter()
                    .any(|p| lower.starts_with(&p.to_lowercase())),
                "{folder}: {:?}",
                quests.met_by
            );
        }
    }

    // A repository GitHub renames, a held or missing entry's claims, and what the summary says.

    /// Helpers moved its main folder to HelpersForever, and a person wrote the old one down. One
    /// build later GitHub renames the repository, so from then on every build reads it as
    /// gh:zebra/HelpersForever, with the old id kept on the scan cache's last result. `seeded`:
    /// seeds.toml names it under its old name. `move_pin`: a person moves [pins] to the new name.
    /// `still_ships_helpers`: it still ships Helpers beside its main folder. A stranger's copy
    /// named Helpers, more popular than it, waits from the rename on. Returns its age before the
    /// rename, and the 31 daily builds after it.
    fn after_github_renamed_helpers(
        seeded: bool,
        move_pin: bool,
        still_ships_helpers: bool,
    ) -> (i64, Vec<Published>, Vec<Settled>) {
        let mut w = world();
        if seeded {
            w.seeds.repos.push("zebra/Helpers".into());
        }
        w.curated.pins = pinned_by_hand("gh:zebra/Helpers", "HelpersForever", &["Helpers"]).pins;
        let at = w
            .repos
            .iter()
            .position(|c| c.addon.id == "gh:zebra/Helpers")
            .unwrap();
        w.repos[at].addon.folders = vec!["HelpersForever".into(), "Helpers".into()];
        let (first, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&first, "gh:zebra/Helpers").formerly, ["Helpers"]);
        let age = addon_of(&first, "gh:zebra/Helpers").created;
        w.after(&first);

        let mut renamed = w.repos.remove(at);
        renamed.addon.id = "gh:zebra/HelpersForever".into();
        renamed.addon.source = Source::Github {
            repo: "zebra/HelpersForever".into(),
        };
        renamed.renamed_from = vec!["gh:zebra/Helpers".into()];
        if !still_ships_helpers {
            renamed.addon.folders = vec!["HelpersForever".into()];
        }
        w.repos.push(renamed);
        if move_pin {
            w.curated.pins =
                pinned_by_hand("gh:zebra/HelpersForever", "HelpersForever", &["Helpers"]).pins;
        }
        let copy = || claim_of("nobody/Helpers", "Helpers", &["HelpersCopy"], 9_000_000);
        let mut builds = Vec::new();
        let mut settled = Vec::new();
        for _ in 1..=31 {
            w.now += DAY_SECS;
            settled.push(w.settle(vec![copy()]));
            let (built, _d) = w.build(vec![copy()]);
            w.after(&built);
            builds.push(built);
        }
        (age, builds, settled)
    }

    #[test]
    fn a_repository_github_renamed_is_listed_under_its_new_name_with_the_folder_it_vouched_for() {
        for (seeded, move_pin, ships) in [
            (false, true, true),
            (false, false, true),
            (true, true, true),
            (false, true, false),
        ] {
            let case = format!("seeded {seeded}, pin moved {move_pin}, ships Helpers {ships}");
            let (age, builds, settled) = after_github_renamed_helpers(seeded, move_pin, ships);
            let held: Vec<String> = builds
                .iter()
                .filter_map(|b| b.held.iter().find(|h| h.id == "gh:zebra/HelpersForever"))
                .map(|h| h.reason.clone())
                .collect();
            assert!(held.is_empty(), "{case}: held {held:?}");
            for (day, built) in builds.iter().enumerate() {
                let renamed = addon_of(built, "gh:zebra/HelpersForever");
                assert_eq!(renamed.formerly, ["Helpers"], "{case}, day {}", day + 1);
                // Its age goes with it, so the more popular copy never gets ahead of it.
                assert_eq!(renamed.created, age, "{case}");
                assert!(is_held(built, "gh:nobody/Helpers"), "{case}");
            }
            for settled in &settled {
                assert!(settled.vouched.is_empty(), "{case}: {:?}", settled.vouched);
                let lines = &settled.renamed;
                if seeded {
                    assert_eq!(
                        lines,
                        &[
                            "gh:zebra/Helpers is gh:zebra/HelpersForever on GitHub now. It isn't hand-picked under that name until seeds.toml or curated.toml uses it"
                        ]
                    );
                } else {
                    assert!(lines.is_empty(), "{case}: {lines:?}");
                }
            }
        }
    }

    #[test]
    fn a_repository_github_renamed_keeps_the_id_it_had_on_every_later_result() {
        // The last run read a/Addon when its own v1 was its only release, and a person wrote its
        // pin down under that name.
        let (mut probe, info) = module_then_addon();
        let curated = pinned_by_hand("gh:a/Addon", "Addon", &[]);
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &curated);
        let mut state = State::empty();
        let record = state.note_repo("a/Addon", "seed");
        let v1_only = repo_with_releases(&[("v1", "Addon-1.zip")]);
        process_pinned(&mut probe, &v1_only, record, &pins, &mut Tally::default()).unwrap();
        assert!(
            state.repos["a/Addon"]
                .entry
                .as_ref()
                .unwrap()
                .renamed_from
                .is_empty()
        );

        // GitHub answers for it as a/Renamed, and its module is the newest release.
        let to_read = renamed_on_github(&mut state, &curated, "module-v2");
        let renamed = RepoInfo {
            name: "a/Renamed".into(),
            ..info
        };
        let entries = full_look(
            &to_read,
            &renamed,
            &mut state,
            &curated,
            &pins,
            &mut probe,
            &mut Tally::default(),
        );
        assert_eq!(entries[0].addon.id, "gh:a/Renamed");
        assert_eq!(entries[0].renamed_from, ["gh:a/Addon"]);

        // Later runs read it under its new name, and the catalog of the run between them left it
        // out. The old id is still on the result, so the pin written under it still holds.
        let unlisted = listed(&[]);
        let pins = Pins::new(Some(&unlisted), &curated);
        for run in 0..2 {
            let record = state.repos.get_mut("a/Renamed").unwrap();
            let mut tally = Tally::default();
            let entry = process_pinned(&mut probe, &renamed, record, &pins, &mut tally).unwrap();
            assert_eq!(entry.addon.folders, ["Addon"], "run {run}: not its module");
            assert_eq!(entry.renamed_from, ["gh:a/Addon"], "run {run}");
            assert_eq!(record.entry.as_ref(), Some(&entry));
        }
    }

    #[test]
    fn a_seed_still_naming_a_renamed_repository_leaves_its_record_alone() {
        // seeds.toml still says a/Addon, so every run notes that name again beside the record
        // under GitHub's new name, and GitHub answers for both as a/Renamed.
        let mut state = State::empty();
        let record = state.note_repo("a/Renamed", "strongbox");
        let mut entry = real_entry("a/Renamed", "Good Thing", &["Addon"], 1);
        entry.renamed_from = vec!["gh:a/Addon".into()];
        record.entry = Some(entry.clone());
        record.newest_tag = Some("v1".into());
        record.looked_at = unix_now();
        for run in 0..2 {
            state.note_repo("a/Addon", "seed");
            let mut heads = heads_of(&["a/Addon", "a/Renamed"]);
            heads.found.get_mut("a/Addon").unwrap().name = "a/Renamed".into();
            let (entries, to_read) = triage_heads(
                &heads,
                &mut state,
                &Curated::default(),
                &HandPicked::default(),
                unix_now(),
                &mut Tally::default(),
            );
            assert!(to_read.is_empty(), "run {run}: nothing new");
            assert_eq!(entries, [entry.clone()], "run {run}");
            assert!(!state.repos.contains_key("a/Addon"), "run {run}");
            let kept = &state.repos["a/Renamed"];
            assert_eq!(kept.entry.as_ref(), Some(&entry), "run {run}");
            assert!(kept.found_by.contains("seed"), "run {run}");
        }
    }

    #[test]
    fn a_library_github_renamed_still_provides_what_listed_addons_require() {
        let mut w = world();
        let lib = real_entry("zebra/LibFoo", "LibFoo", &["LibFoo"], 10);
        let mut needs = real_entry("someone/NeedsLib", "Needs Lib", &["NeedsLib"], 10);
        needs.addon.deps = vec!["LibFoo".into()];
        w.previous.addons.push(lib.addon.clone());
        w.previous.addons.push(needs.addon.clone());
        w.repos.push(needs);
        let mut renamed = lib;
        renamed.addon.id = "gh:zebra/LibFooForever".into();
        renamed.addon.source = Source::Github {
            repo: "zebra/LibFooForever".into(),
        };
        renamed.renamed_from = vec!["gh:zebra/LibFoo".into()];
        let (published, _d) = w.build(vec![renamed]);
        assert!(
            !is_held(&published, "gh:zebra/LibFooForever"),
            "{:?}",
            published.held
        );
        assert_eq!(
            addon_of(&published, "gh:zebra/LibFooForever").created,
            merge::BASELINE_CREATED
        );
    }

    /// A listing nobody picked, published once with FooBar as its main folder, which then moves
    /// its main folder to one an older entry has, so it's held from then on and no catalog lists
    /// it again. Each of the 45 daily builds after that also has `shipper`, or PeaversCommons
    /// with FooBar added when `shipper` is `None`. Returns the days `id` was held, and the last
    /// reason.
    fn held_beside_a_listing_held_for_good(
        shipper: Option<Candidate>,
        id: &str,
    ) -> (Vec<u32>, String) {
        let mut w = world();
        w.wowi.push(listing_entry(500, "Foo Bar", &["FooBar"], 10));
        let (first, _d0) = w.build(vec![]);
        assert!(!is_held(&first, "wowi:500"));
        w.after(&first);
        let at = w
            .wowi
            .iter()
            .position(|c| c.addon.id == "wowi:500")
            .unwrap();
        w.wowi[at].addon.folders = vec!["Helpers".into()];
        if shipper.is_none() {
            let peavers = w
                .repos
                .iter()
                .position(|c| c.addon.id == "gh:Peavers/PeaversCommons")
                .unwrap();
            w.repos[peavers].addon.folders = vec!["PeaversCommons".into(), "FooBar".into()];
        }
        let mut held = Vec::new();
        let mut reason = String::new();
        for day in 1..=45 {
            w.now += DAY_SECS;
            let (built, _d) = w.build(shipper.iter().cloned().collect());
            assert!(is_held(&built, "wowi:500"), "day {day}");
            if is_held(&built, id) {
                held.push(day);
                reason = held_reason(&built, id).to_string();
            }
            w.after(&built);
        }
        (held, reason)
    }

    #[test]
    fn a_held_entrys_vouched_folder_stops_holding_others_a_month_after_it_was_last_published() {
        let newcomer = claim_of("someone/FooBar", "Foo Bar Reborn", &["FooBar"], 1);
        let (held, reason) =
            held_beside_a_listing_held_for_good(Some(newcomer), "gh:someone/FooBar");
        let (peavers, peavers_reason) =
            held_beside_a_listing_held_for_good(None, "gh:Peavers/PeaversCommons");
        // Held while the listing still has the folder as a claim, and not a day after.
        let month: Vec<u32> = (1..=29).collect();
        assert_eq!(held, month, "{reason}");
        assert_eq!(reason, "its folder FooBar was wowi:500's main folder");
        assert_eq!(peavers, month, "{peavers_reason}");
        assert_eq!(
            peavers_reason,
            "its folder FooBar was wowi:500's main folder"
        );
    }

    #[test]
    fn a_release_with_a_zip_for_another_flavor_isnt_said_to_be_all_another_addons() {
        let mut probe = ScriptedProbe::default();
        probe
            .zips
            .insert("Addon-1.zip".into(), Ok(forever_scan("Good Thing")));
        let mut info = repo_with_releases(&[("voices-v2", "unused.zip"), ("v1", "Addon-1.zip")]);
        let mut voices = other_addons_release(&mut probe, "voices-v2", 23);
        // The biggest zip, read first, is the addon's own build for another flavor.
        probe
            .zips
            .insert(voices.assets[0].name.clone(), Ok(retail_scan()));
        info.releases[0].assets = std::mem::take(&mut voices.assets);
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut state = State::empty();
        let record = state.note_repo("a/Addon", "seed");
        let mut tally = Tally::default();
        let entry = process_pinned(&mut probe, &info, record, &pins, &mut tally).unwrap();
        assert_eq!(version_of(&entry), "v1");
        assert_eq!(
            tally.left_unread,
            [
                "gh:a/Addon passed over voices-v2 with 3 of its zips unread and no Forever build of Addon among the ones read. It isn't read again until its files or this entry's main folder change"
            ]
        );
        let passed_over = "gh:a/Addon lists v1, passed over newer voices-v2, which doesn't ship Addon in the zips read. If voices-v2 holds the addon this entry should list, set [pins.\"gh:a/Addon\"] in curated.toml";
        assert_eq!(tally.passed_over, [passed_over]);

        // A quick look that finds nothing new says the same.
        let mut heads = heads_of(&["a/Addon"]);
        heads.found.get_mut("a/Addon").unwrap().releases[0].tag = "voices-v2".into();
        let mut tally = Tally::default();
        let (entries, to_read) = triage_heads(
            &heads,
            &mut state,
            &Curated::default(),
            &HandPicked::default(),
            unix_now(),
            &mut tally,
        );
        assert!(to_read.is_empty());
        assert_eq!(entries.len(), 1);
        assert_eq!(tally.passed_over, [passed_over]);
    }

    #[test]
    fn a_claim_the_scan_cache_remembers_names_the_entry_as_the_catalog_spells_it() {
        // Helpers moved its main folder, and a person wrote the old one down.
        let mut w = world();
        w.curated.pins = pinned_by_hand("gh:zebra/Helpers", "HelpersForever", &["Helpers"]).pins;
        let at = w
            .repos
            .iter()
            .position(|c| c.addon.id == "gh:zebra/Helpers")
            .unwrap();
        w.repos[at].addon.folders = vec!["HelpersForever".into()];
        let (first, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&first, "gh:zebra/Helpers").formerly, ["Helpers"]);
        w.after(&first);

        // It's missing from the next two builds, and a stranger's repository ships Helpers. The
        // claim comes from the last catalog, then from the scan cache.
        w.repos.remove(at);
        let stranger = || claim_of("nobody/helpers-old", "Old Helpers", &["Helpers"], 1);
        for _ in 0..2 {
            w.now += DAY_SECS;
            let (built, _d) = w.build(vec![stranger()]);
            assert_eq!(
                held_reason(&built, "gh:nobody/helpers-old"),
                "its folder Helpers was gh:zebra/Helpers's main folder"
            );
            w.after(&built);
        }
    }

    #[test]
    fn the_scan_cache_remembers_the_same_through_builds_with_nothing_new() {
        let mut w = world();
        w.wowi[0].addon.folders = vec!["TomTomForever".into()];
        let mut saved = Vec::new();
        for _ in 0..3 {
            // Every three hours, as the workflow runs.
            w.now += 3 * 60 * 60;
            let (built, _d) = w.build(vec![]);
            w.after(&built);
            saved.push(serde_json::to_string(&w.remembered).unwrap());
        }
        assert_eq!(saved[0], saved[1]);
        assert_eq!(saved[1], saved[2]);
        let tomtom = w.remembered.get("wowi:7032").unwrap();
        assert_eq!(tomtom.at, BUILT_AT.div_euclid(DAY_SECS) * DAY_SECS);
    }

    #[test]
    fn a_build_from_the_last_builds_output_with_nothing_new_is_the_same() {
        // A renamed listing, a written was, a pin that lets a rename through, and a stranger
        // held for a vouched folder, read the same way build after build.
        let mut w = tomtom_renamed();
        w.curated.pins = pinned_by_hand("gh:zebra/Helpers", "HelpersForever", &["Helpers"]).pins;
        let at = w
            .repos
            .iter()
            .position(|c| c.addon.id == "gh:zebra/Helpers")
            .unwrap();
        w.repos[at].addon.folders = vec!["HelpersForever".into(), "Helpers".into()];
        let stranger = || claim_of("zebra/Waypoints", "Zebra Waypoints", &["TomTom"], 1);
        let mut outputs = Vec::new();
        for _ in 0..4 {
            w.now += DAY_SECS;
            let (built, _d) = w.build(vec![stranger()]);
            let addons = serde_json::to_value(&built.catalog.addons).unwrap();
            let held: Vec<(String, String)> = built
                .held
                .iter()
                .map(|h| (h.id.clone(), h.reason.clone()))
                .collect();
            outputs.push((addons, held));
            w.after(&built);
        }
        assert_eq!(outputs[1], outputs[2]);
        assert_eq!(outputs[2], outputs[3]);
    }

    #[test]
    fn formerly_settles_after_a_rename_even_with_eight_folders_written_in_was() {
        // TomTom's listing renames its folder, and a person wrote eight older names in was, the
        // most [pins] takes.
        let mut w = tomtom_renamed();
        let was: Vec<String> = (1..=8).map(|n| format!("TomTomOld{n}")).collect();
        w.curated.pins.insert(
            "wowi:7032".into(),
            crate::curated::PinSpec {
                main: None,
                was: was.clone(),
            },
        );
        let mut lists = Vec::new();
        for build in 0..10 {
            w.now += DAY_SECS;
            let settled = w.settle(vec![]);
            if build == 0 {
                assert_eq!(settled.vouched.len(), 8, "{:?}", settled.vouched);
            } else {
                assert!(settled.vouched.is_empty(), "{:?}", settled.vouched);
            }
            assert_eq!(
                settled.cut,
                [
                    "wowi:7032 was TomTomOld8, as [pins] says, but it isn't vouched for: formerly keeps the 8 most recent"
                ]
            );
            let (built, _d) = w.build(vec![]);
            lists.push(addon_of(&built, "wowi:7032").formerly.clone());
            w.after(&built);
        }
        let mut wanted = vec!["TomTom".to_string()];
        wanted.extend(was.iter().take(7).cloned());
        for (build, list) in lists.iter().enumerate() {
            assert_eq!(list, &wanted, "build {build}");
        }
    }

    #[test]
    fn a_listing_held_for_one_build_keeps_its_name_and_main_folder_from_a_newcomer() {
        // A listing nobody seeded, and a stranger who has listed a folder named like the one
        // the listing will add next, as many addons add a <Name>_Options folder.
        let mut w = world();
        let wayfarer = listing_entry(5555, "Wayfarer", &["Wayfarer"], 1_000);
        let bait = claim_of("zebra/Options", "Zebra Options", &["Wayfarer_Options"], 1);
        w.previous.addons.push(wayfarer.addon.clone());
        w.previous.addons.push(bait.addon.clone());
        w.repos.push(bait);
        w.wowi.push(wayfarer);

        // The stranger's copy, with the listing's name and main folder, waits: held, build
        // after build, and costing nothing.
        let squatter = || claim_of("zebra/Wayfinder", "Wayfarer", &["Wayfarer"], 1);
        let (waiting, _dw) = w.build(vec![squatter()]);
        assert_eq!(
            held_reason(&waiting, "gh:zebra/Wayfinder"),
            "its name is already taken by wowi:5555"
        );
        w.after(&waiting);

        // The listing adds that folder, so this build holds it, and the copy still waits.
        w.wowi.last_mut().unwrap().addon.folders =
            vec!["Wayfarer".into(), "Wayfarer_Options".into()];
        w.now += DAY_SECS;
        let (held_once, _d0) = w.build(vec![squatter()]);
        assert_eq!(
            held_reason(&held_once, "wowi:5555"),
            "it just added the folder Wayfarer_Options, which gh:zebra/Options already lists"
        );
        assert_eq!(
            held_reason(&held_once, "gh:zebra/Wayfinder"),
            "its name is wowi:5555's, which isn't listed in this build"
        );
        w.after(&held_once);

        // The listing drops the folder again, as it was before, and comes back as it was.
        w.wowi.last_mut().unwrap().addon.folders = vec!["Wayfarer".into()];
        w.now += DAY_SECS;
        let (back, _d1) = w.build(vec![squatter()]);
        assert!(!is_held(&back, "wowi:5555"), "{:?}", back.held);
        let listing = addon_of(&back, "wowi:5555");
        assert_eq!(listing.created, merge::BASELINE_CREATED);
        assert!(listing.trusted);
        assert_eq!(
            held_reason(&back, "gh:zebra/Wayfinder"),
            "its name is already taken by wowi:5555"
        );
    }

    #[test]
    fn a_listing_held_for_adding_a_taken_folder_stays_held_while_it_ships_it() {
        let mut w = world();
        let wayfarer = listing_entry(5555, "Wayfarer", &["Wayfarer"], 1_000);
        let bait = claim_of("zebra/Options", "Zebra Options", &["Wayfarer_Options"], 1);
        w.previous.addons.push(wayfarer.addon.clone());
        w.previous.addons.push(bait.addon.clone());
        w.repos.push(bait);
        w.wowi.push(wayfarer);
        let (first, _d) = w.build(vec![]);
        w.after(&first);
        // It keeps the folder it added, and keeps its age while it's held, so it's judged by the
        // folders it was last published with.
        w.wowi.last_mut().unwrap().addon.folders =
            vec!["Wayfarer".into(), "Wayfarer_Options".into()];
        for day in 1..=3 {
            w.now += DAY_SECS;
            let (built, _d) = w.build(vec![]);
            assert_eq!(
                held_reason(&built, "wowi:5555"),
                "it just added the folder Wayfarer_Options, which gh:zebra/Options already lists",
                "day {day}"
            );
            w.after(&built);
        }
    }

    /// A target listed before ages were kept and a copy with its name and main folder held behind
    /// it, then one build the target is missing from, then the target back. Returns each build.
    fn a_copy_waiting_on(target: Candidate, is_listing: bool, copy: Candidate) -> Vec<Published> {
        let mut w = world();
        let id = target.addon.id.clone();
        w.previous.addons.push(target.addon.clone());
        let add = |w: &mut World| {
            if is_listing {
                w.wowi.push(target.clone());
            } else {
                w.repos.push(target.clone());
            }
        };
        add(&mut w);
        let mut builds = Vec::new();
        let (waiting, _d) = w.build(vec![copy.clone()]);
        w.after(&waiting);
        builds.push(waiting);
        // The gap: for a listing, an upload with no Forever build. For a repository, left out.
        w.wowi.retain(|c| c.addon.id != id);
        w.repos.retain(|c| c.addon.id != id);
        w.now += DAY_SECS;
        let (gap, _d) = w.build(vec![copy.clone()]);
        w.after(&gap);
        builds.push(gap);
        add(&mut w);
        for day in [1, 2, 15, 45] {
            w.now = BUILT_AT + (day + 1) * DAY_SECS;
            let (back, _d) = w.build(vec![copy.clone()]);
            w.after(&back);
            builds.push(back);
        }
        builds
    }

    #[test]
    fn a_listing_or_repository_missing_for_one_build_keeps_its_name_and_main_folder_from_a_copy() {
        let copy = claim_of("zebra/Wayfinder", "Wayfarer", &["Wayfarer"], 9_000_000);
        for (target, is_listing) in [
            (listing_entry(5555, "Wayfarer", &["Wayfarer"], 1_000), true),
            (
                real_entry("alice/Wayfarer", "Wayfarer", &["Wayfarer"], 1_000),
                false,
            ),
        ] {
            let id = target.addon.id.clone();
            let builds = a_copy_waiting_on(target, is_listing, copy.clone());
            assert_eq!(
                held_reason(&builds[1], "gh:zebra/Wayfinder"),
                format!("its name is {id}'s, which isn't listed in this build"),
                "the gap build"
            );
            for (at, built) in builds.iter().enumerate() {
                assert!(is_held(built, "gh:zebra/Wayfinder"), "{id}, build {at}");
                if at != 1 {
                    let target = addon_of(built, &id);
                    assert_eq!(target.created, merge::BASELINE_CREATED, "{id}, build {at}");
                    assert!(target.trusted, "{id}, build {at}");
                }
            }
        }
    }

    #[test]
    fn a_hand_picked_listing_missing_for_one_build_keeps_its_name_from_a_copy() {
        // TomTom's listing is seeded, and a copy waits behind it.
        let copy = claim_of("zebra/TomTom", "TomTom", &["TomTom"], 9_000_000);
        let tomtom = listing_entry(7032, "TomTom", &["TomTom"], 3_550_000);
        let mut w = world();
        let (waiting, _d) = w.build(vec![copy.clone()]);
        assert!(is_held(&waiting, "gh:zebra/TomTom"));
        w.after(&waiting);
        w.wowi.retain(|c| c.addon.id != "wowi:7032");
        w.now += DAY_SECS;
        let (gap, _d) = w.build(vec![copy.clone()]);
        assert_eq!(
            held_reason(&gap, "gh:zebra/TomTom"),
            "its name is wowi:7032's, which isn't listed in this build"
        );
        w.after(&gap);
        w.wowi.insert(0, tomtom);
        w.now += DAY_SECS;
        let (back, _d) = w.build(vec![copy]);
        assert!(!is_held(&back, "wowi:7032"));
        assert_eq!(
            held_reason(&back, "gh:zebra/TomTom"),
            "its name is already taken by wowi:7032"
        );
    }

    #[test]
    fn a_new_repository_from_the_same_owner_can_take_the_place_of_one_that_left() {
        // The real move from gh:MaddisonM79/ryna-wow_forever to gh:MaddisonM79/ryna_agent, from
        // one live catalog to the next.
        let mut w = world();
        let old = real_entry("MaddisonM79/ryna-wow_forever", "Ryna", &["Ryna"], 100);
        w.previous.addons.push(old.addon.clone());
        w.repos.push(old);
        let (first, _d) = w.build(vec![]);
        w.after(&first);
        w.repos
            .retain(|c| c.addon.id != "gh:MaddisonM79/ryna-wow_forever");
        w.now += DAY_SECS;
        let moved = real_entry(
            "maddisonm79/ryna_agent",
            "Ryna",
            &["Ryna", "Ryna_Chronicle"],
            1,
        );
        let (published, _d) = w.build(vec![moved]);
        assert!(
            !is_held(&published, "gh:maddisonm79/ryna_agent"),
            "{:?}",
            published.held
        );

        // Another owner's copy is held.
        let copy = real_entry("zebra/ryna", "Ryna", &["Ryna"], 9_000_000);
        let (published, _d) = w.build(vec![copy]);
        assert_eq!(
            held_reason(&published, "gh:zebra/ryna"),
            "its name is gh:MaddisonM79/ryna-wow_forever's, which isn't listed in this build"
        );
        let copy = real_entry("zebra/other", "Something Else", &["Ryna"], 9_000_000);
        let (published, _d) = w.build(vec![copy]);
        assert_eq!(
            held_reason(&published, "gh:zebra/other"),
            "its main folder Ryna is gh:MaddisonM79/ryna-wow_forever's, which isn't listed in this build"
        );
    }

    #[test]
    fn a_fork_dropped_beside_its_parent_doesnt_hold_the_parent() {
        // A fork was listed while its parent had no Forever build. Now the parent has one, so the
        // fork goes, with the same folders as the parent, and the parent takes its place.
        let mut w = world();
        let mut fork = real_entry("fork/Stream", "Stream", &["Stream"], 100);
        fork.fork_of = Some("up/Stream".into());
        w.previous.addons.push(fork.addon.clone());
        w.repos.push(fork);
        let (first, _d) = w.build(vec![]);
        w.after(&first);
        w.now += DAY_SECS;
        let parent = real_entry("up/Stream", "Stream", &["Stream"], 1);
        let settled = w.settle(vec![parent.clone()]);
        assert_eq!(settled.forks, ["gh:fork/Stream"]);
        let (published, _d) = w.build(vec![parent]);
        assert!(!is_held(&published, "gh:up/Stream"), "{:?}", published.held);
    }

    /// The world with a WoWInterface listing nobody seeded, listed from before ages were kept,
    /// that renames its folder from Wayfarer to WayfarerForever in the first build.
    fn wayfarer_renamed() -> (World, Candidate) {
        let mut w = world();
        let wayfarer = listing_entry(5555, "Wayfarer", &["Wayfarer"], 1_000);
        w.previous.addons.push(wayfarer.addon.clone());
        let mut renamed = wayfarer;
        renamed.addon.folders = vec!["WayfarerForever".into()];
        w.wowi.push(renamed.clone());
        (w, renamed)
    }

    #[test]
    fn a_stranger_cant_take_a_renamed_listings_old_folder_by_case_look_alikes_or_a_gap() {
        let (mut w, renamed) = wayfarer_renamed();
        let (first, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&first, "wowi:5555").formerly, ["Wayfarer"]);
        w.after(&first);

        // The listing is no hand-picked one, and its rename made it new, so these strangers are
        // all older than it or as new, and none of them was listed with the folder.
        let strangers = || {
            vec![
                claim_of("zebra/Caps", "Zebra Caps", &["ZebraCaps", "WAYFARER"], 1),
                // A Cyrillic a.
                claim_of(
                    "zebra/Alike",
                    "Zebra Alike",
                    &["ZebraAlike", "W\u{0430}yfarer"],
                    1,
                ),
                claim_of("zebra/Plain", "Zebra Plain", &["wayfarer"], 1),
            ]
        };
        let (next, _d1) = w.build(strangers());
        for id in ["gh:zebra/Caps", "gh:zebra/Alike", "gh:zebra/Plain"] {
            assert!(is_held(&next, id), "{id}");
        }
        assert_eq!(addon_of(&next, "wowi:5555").formerly, ["Wayfarer"]);
        w.after(&next);

        // A build without the listing, and the same strangers try again.
        w.wowi.retain(|c| c.addon.id != "wowi:5555");
        w.now += DAY_SECS;
        let (gap, _d2) = w.build(strangers());
        for id in ["gh:zebra/Caps", "gh:zebra/Alike", "gh:zebra/Plain"] {
            assert!(is_held(&gap, id), "{id} during the gap");
        }
        w.after(&gap);

        // Back, the vouch is still there and the strangers are still held.
        w.wowi.push(renamed);
        w.now += DAY_SECS;
        let (back, _d3) = w.build(strangers());
        for id in ["gh:zebra/Caps", "gh:zebra/Alike", "gh:zebra/Plain"] {
            assert!(is_held(&back, id), "{id} after the gap");
        }
        assert_eq!(addon_of(&back, "wowi:5555").formerly, ["Wayfarer"]);
    }

    #[test]
    fn an_excluded_listing_cant_hold_anything_with_the_folders_it_was_vouched_for() {
        // The listing renamed itself away from Wayfarer, and a maintainer then excludes it. The
        // real Wayfarer, a repository nobody seeded, ships Wayfarer, and another goes by the
        // listing's name.
        let (mut w, _renamed) = wayfarer_renamed();
        let (first, _d0) = w.build(vec![]);
        assert_eq!(addon_of(&first, "wowi:5555").formerly, ["Wayfarer"]);
        w.after(&first);
        w.curated.exclude.ids.push("wowi:5555".into());
        let others = || {
            vec![
                claim_of("wayfarer/Wayfarer", "Wayfarer Maps", &["Wayfarer"], 1),
                claim_of("wayfarer/Classic", "Wayfarer", &["WayfarerClassic"], 1),
            ]
        };
        for days in [1, 20] {
            w.now += days * DAY_SECS;
            let (built, _d) = w.build(others());
            assert!(built.held.is_empty(), "{days} days on: {:?}", built.held);
            w.after(&built);
        }
    }

    #[test]
    fn every_catalog_the_gate_lets_out_meets_the_catalog_checks_former_folder_rule() {
        // Whatever reaches the gate with a vouch for a folder another entry ships, in any case or
        // look-alike spelling, the catalog goes out without it.
        let release = |name: &str| Release {
            version: "1.0".into(),
            url: format!("https://github.com/zebra/{name}/releases/download/v1/{name}.zip"),
            size: 10,
            ..Release::default()
        };
        let entry = |repo: &str, folders: &[&str], formerly: &[&str]| {
            let mut c = real_entry(repo, repo.split('/').nth(1).unwrap(), folders, 1);
            c.addon.formerly = formerly.iter().map(|f| f.to_string()).collect();
            c.addon.stable = Some(release(repo.split('/').nth(1).unwrap()));
            c.addon.category = "misc".into();
            c
        };
        let candidates = vec![
            entry(
                "zebra/Renamed",
                &["RenamedNow"],
                &["QUESTIE", "W\u{0430}yfarer", "\u{0397}ogHeals", "Older"],
            ),
            entry("zebra/Questie", &["Questie"], &[]),
            entry("zebra/Wayfarer", &["Wayfarer"], &[]),
            entry("zebra/Hogs", &["\u{03B7}ogHeals"], &[]),
        ];
        let dir = tempfile::tempdir().unwrap();
        let published = publish(
            candidates,
            vec![],
            &GateContext {
                curated: &Curated::default(),
                picks: &HandPicked::default(),
                previous: None,
                answered: &BTreeSet::new(),
                accept_smaller: false,
            },
            dir.path(),
        )
        .unwrap();
        assert_eq!(published.catalog.addons.len(), 4, "nothing held for it");
        assert_eq!(addon_of(&published, "gh:zebra/Renamed").formerly, ["Older"]);
        assert!(crate::gate::formerly_shipped_elsewhere(&published.catalog.addons).is_empty());
    }

    #[test]
    fn a_pack_whose_size_isnt_known_goes_out_unreviewed_and_is_named_for_it() {
        let mut w = world();
        w.seeds.repos.push(SPOKEN.into());
        w.curated.split = vec![spoken_split()];
        let mut quests = real_entry(SPOKEN, "Spoken Audio: Quests", &QUEST_FOLDERS, 1394);
        quests.addon.id = SplitSpec::line_id(SPOKEN, "quests-audio");
        let settled = w.settle(vec![quests.clone()]);
        assert_eq!(
            settled.unreviewed.unknown_size,
            ["gh:rusty-key/spoken-wow/quests-audio, whose stable download's size isn't known"]
        );
        assert!(settled.unreviewed.big.is_empty());
        let (published, _dir) = w.build(vec![quests]);
        let pack = addon_of(&published, "gh:rusty-key/spoken-wow/quests-audio");
        assert!(pack.trusted && !pack.reviewed);
    }
}
