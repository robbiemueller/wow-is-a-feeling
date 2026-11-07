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
use crate::model::{Candidate, Held, ZipScan};
use crate::rangezip::{HttpRange, RangeReader};
use crate::state::{RepoState, State, write_atomic};
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

    fn read_zip(&mut self, asset: &GhAsset, hints: &[&str]) -> Result<ZipScan, String> {
        let source = HttpRange::new(self.http, Channel::Asset, &asset.url, Some(asset.size));
        match scan_zip(RangeReader::new(source, asset.size), hints) {
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

/// A previous entry for a repository that now shows no Forever build. It stays,
/// marked grey, as long as its file can still exist.
fn grey_previous(previous: &Candidate, info: &RepoInfo) -> Option<Candidate> {
    let url = previous
        .addon
        .stable
        .as_ref()
        .or(previous.addon.beta.as_ref())?
        .url
        .as_str();
    let still_listed = info
        .releases
        .iter()
        .flat_map(|r| r.assets.iter())
        .any(|a| a.url == url);
    if still_listed || info.releases.len() >= RELEASES_FETCHED {
        let mut grey = previous.clone();
        grey.no_longer_forever = true;
        Some(grey)
    } else {
        None
    }
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
    /// Entries kept at their last build, because every newer release is another addon's.
    pinned: Vec<String>,
    /// Entries `allow_new_main` let change their main folder.
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
    pin: Option<Pin<'a>>,
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

/// What an entry keeps when every newer release is another addon's: its last result, or the
/// entry as the last catalog listed it when the scan cache has nothing that's still this addon.
/// A source install isn't kept here: `[[source]]` reads it fresh, and a packaged entry under
/// the same id would replace that.
fn kept_build(last: Option<&Candidate>, pin: Pin<'_>) -> Option<Candidate> {
    if let Some(entry) = last.filter(|entry| detect::ships(&entry.addon.folders, Some(pin.main))) {
        return Some(entry.clone());
    }
    let release = pin.last.stable.as_ref().or(pin.last.beta.as_ref())?;
    release.moves.is_empty().then(|| Candidate {
        addon: pin.last.clone(),
        ..Candidate::default()
    })
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
    let pin = product.pin;
    let chosen = detect::choose_builds(
        repo_name,
        &info.releases,
        verdicts,
        pin.map(|pin| pin.main),
        probe,
        out_of_time,
    );
    for problem in &chosen.errors {
        eprintln!("{} {problem}", product.id);
    }
    let incomplete = !chosen.errors.is_empty();
    let skipped = chosen.skipped.as_deref().map(|tag| product.full_tag(tag));

    let entry = match github_candidate(info, &chosen, product.known_downloads) {
        Some(entry) if merge::is_placeholder_name(&entry.addon.name) && !product.picked => {
            tally.placeholder += 1;
            eprintln!("{}: skipped, its title is only a placeholder", product.id);
            None
        }
        Some(entry) => {
            if let Some(tag) = &skipped {
                eprintln!("{}: {tag} is another addon's, passed it over", product.id);
            }
            Some(product.own(entry, &chosen))
        }
        None if incomplete => {
            if last.is_some() {
                tally.kept_last_known += 1;
            }
            last.cloned()
        }
        None => match (pin, skipped) {
            (Some(pin), Some(tag)) => {
                let kept = kept_build(last, pin);
                if kept.is_some() {
                    tally.pinned.push(format!(
                        "{} keeps {}, as {tag} is another addon's",
                        product.id, pin.main
                    ));
                }
                kept
            }
            _ => {
                tally.no_build += 1;
                last.and_then(|previous| grey_previous(previous, info))
            }
        },
    };
    (entry, incomplete)
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
    let product = Product {
        pin: look.pins.get(&id),
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
/// `fetched` is each prefix's releases, or why they couldn't be fetched.
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
    let id = format!("gh:{}", info.name);
    let main = Product {
        pin: look.pins.get(&id),
        id,
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
        let id = SplitSpec::line_id(&info.name, line);
        let product = Product {
            pin: look.pins.get(&id),
            id,
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
    // No releases, yet something is listed: the tag comparison can't see this when a
    // rename and a deleted release fall between two runs, so the entry would stay for good.
    let Some(newest) = newest else {
        return record.entry.is_some();
    };
    if is_rolling_tag(newest) {
        return true;
    }
    let relook = if record.entry.is_some() {
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
    for (asked, head) in &heads.found {
        let split = split_of(curated, asked, &head.name);
        if !seen.insert(head.name.to_ascii_lowercase()) || curated.excludes_repo(&head.name) {
            state.repos.remove(asked);
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
        } else if needs_full_look(record, head, now) || has_unread_lines(record, split) {
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
            entries.extend(listed_entries(record, split));
        }
    }
    (entries, to_read)
}

/// Each split repository's releases, prefix by prefix, keyed by the name it was asked for in
/// lower case. A failure stays one, and that repository keeps what it had.
type SplitReleases = BTreeMap<String, Result<PrefixReleases, String>>;

fn fetch_split_releases(http: &Http, batch: &BatchResult, curated: &Curated) -> SplitReleases {
    batch
        .found
        .iter()
        .filter_map(|(asked, info)| {
            let split = split_of(curated, asked, &info.name)?;
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
        let record = state.repos.entry(asked.clone()).or_default();
        let look = Look {
            picked: hand_picked.contains(&asked.to_ascii_lowercase()),
            pins,
            out_of_time,
        };
        match split_of(curated, asked, &info.name) {
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
}

/// Everything between the scans and the gate: who merges with whom, who owns which name
/// and folder, and what the app may pick by itself. No network and no clock of its own.
pub fn settle(
    scanned: Scanned,
    curated: &Curated,
    picks: &HandPicked,
    previous: Option<&Catalog>,
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
    merge::stamp_ages(&mut candidates, previous, picks, now);
    let reserved = merge::Reserved::new(curated);
    let rules = merge::ClaimRules {
        picks,
        reserved: &reserved,
        previous,
    };
    let (mut candidates, held) = merge::hold_collisions(candidates, &rules);
    merge::settle_trust(&mut candidates, picks, &curated.merges, now);
    finish_candidates(&mut candidates, curated);
    Settled {
        candidates,
        held,
        forks,
        merged,
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

/// Entries `allow_new_main` let stop shipping the main folder the last catalog listed them
/// with. Each should come out of that list once this catalog is out, or the entry is unpinned
/// for good and a later release of another addon could take its place.
fn renamed_entries(
    candidates: &[Candidate],
    previous: Option<&Catalog>,
    curated: &Curated,
) -> Vec<String> {
    let listed = previous.map_or(&[][..], |catalog| catalog.addons.as_slice());
    candidates
        .iter()
        .filter(|c| curated.allows_new_main(&c.addon.id))
        .filter_map(|c| {
            let last = listed
                .iter()
                .find(|addon| addon.id.eq_ignore_ascii_case(&c.addon.id))?;
            let old = last.folders.first()?;
            let new = c.addon.folders.first()?;
            (!detect::ships(&c.addon.folders, Some(old)))
                .then(|| format!("{} went from {old} to {new}", c.addon.id))
        })
        .collect()
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
    if !tally.pinned.is_empty() {
        println!(
            "kept at their last build, because everything newer is another addon's: {}. A real rename goes in allow_new_main in curated.toml.",
            tally.pinned.len()
        );
        for line in &tally.pinned {
            println!("    {line}");
        }
    }
    if !tally.renamed.is_empty() {
        println!(
            "main folders changed through allow_new_main: {}. Take them out of it once this catalog is out.",
            tally.renamed.len()
        );
        for line in &tally.renamed {
            println!("    {line}");
        }
    }
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
    let pins = Pins::new(previous.as_ref(), &curated);
    std::fs::create_dir_all(&options.out)
        .map_err(|e| format!("Couldn't create {}: {e}", options.out.display()))?;

    let state_out = options.out.join("catalog-state.json");
    let mut state = State::load(options.state.as_deref().unwrap_or(&state_out));
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
    tally.renamed = renamed_entries(&packaged, previous.as_ref(), &curated);

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
    let settled = settle(scanned, &curated, &picks, previous.as_ref(), unix_now());
    for id in &settled.forks {
        eprintln!(
            "{id}: dropped, a fork with the same folders as its parent or a better-known sibling"
        );
    }
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
        }
    }

    impl World {
        /// One build with these extra repositories in the scan results.
        fn build(&self, extra: Vec<Candidate>) -> (Published, tempfile::TempDir) {
            let picks = HandPicked::new(&self.seeds, &self.curated);
            let mut repos = self.repos.clone();
            repos.extend(extra);
            let scanned = Scanned {
                repos,
                wowi: self.wowi.clone(),
                direct: vec![],
            };
            let settled = settle(
                scanned,
                &self.curated,
                &picks,
                Some(&self.previous),
                BUILT_AT,
            );
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
            (published, dir)
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
        fn read_zip(&mut self, asset: &GhAsset, _: &[&str]) -> Result<ZipScan, String> {
            self.reads += 1;
            self.zips
                .get(&asset.name)
                .cloned()
                .unwrap_or_else(|| Err("no such zip".to_string()))
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
        let look = Look {
            picked: false,
            pins,
            out_of_time: &|| false,
        };
        process_repo(probe, info, record, &look, tally)
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
        assert!(tally.pinned.is_empty(), "nothing was kept back");
        assert!(!record.incomplete);
        assert!(
            !record.releases["module-v2"].confirmed_negative,
            "another addon's release isn't a negative"
        );
        assert_eq!(record.newest_tag.as_deref(), Some("module-v2"));
    }

    #[test]
    fn with_nothing_matching_the_last_build_is_kept_and_reported() {
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let pins = Pins::new(Some(&last), &Curated::default());
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        let good = process_pinned(&mut probe, &info, &mut record, &pins, &mut tally).unwrap();

        // The addon's own release has left the list, and only the module's is in reach.
        let only_module = repo_with_releases(&[("module-v2", "Module-2.zip")]);
        let kept = process_pinned(&mut probe, &only_module, &mut record, &pins, &mut tally);
        assert_eq!(kept.as_ref(), Some(&good));
        assert_eq!(record.entry.as_ref(), Some(&good));
        assert!(!record.incomplete, "nothing failed");
        assert_eq!(
            tally.pinned,
            ["gh:a/Addon keeps Addon, as module-v2 is another addon's"]
        );
        assert_eq!(tally.no_build, 0);

        // With nothing in the scan cache that's still the addon, the last catalog's entry stays.
        let mut lost = RepoState::default();
        let from_catalog =
            process_pinned(&mut probe, &only_module, &mut lost, &pins, &mut tally).unwrap();
        assert_eq!(from_catalog.addon, last.addons[0]);
        assert_eq!(tally.pinned.len(), 2);

        // Unless it was a source install, which [[source]] reads fresh instead.
        let mut source = last.clone();
        source.addons[0].stable.as_mut().unwrap().moves = vec![feeling_core::catalog::Move {
            from: "*/".into(),
            to: "Addon".into(),
        }];
        let pins = Pins::new(Some(&source), &Curated::default());
        let mut tally = Tally::default();
        let none = process_pinned(
            &mut probe,
            &only_module,
            &mut RepoState::default(),
            &pins,
            &mut tally,
        );
        assert_eq!(none, None);
        assert!(tally.pinned.is_empty());
    }

    #[test]
    fn allow_new_main_lets_a_rename_through() {
        let (mut probe, info) = module_then_addon();
        let last = listed(&[("gh:a/Addon", &["Addon"])]);
        let curated = Curated {
            allow_new_main: vec!["gh:a/addon".into()],
            ..Curated::default()
        };
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
        assert_eq!(
            renamed_entries(&[entry], Some(&last), &curated),
            ["gh:a/Addon went from Addon to Module"],
            "and the summary says to take it out of the list again"
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
            assert!(tally.pinned.is_empty());
        }
    }

    #[test]
    fn an_update_that_adds_folders_or_moves_its_main_folder_is_still_taken() {
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
            assert_eq!(entry.addon.folders, folders);
            assert_eq!(probe.reads, 1, "the older release wasn't needed");
        }
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
    fn lines_are_pinned_like_any_entry() {
        let mut probe = spoken_probe();
        let mut record = RepoState::default();
        let mut tally = Tally::default();
        split_once(
            &mut probe,
            &Ok(spoken_fetched()),
            &mut record,
            &Pins::default(),
            &mut tally,
        );

        // The author reuses the prefix for a different pack.
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
        let entries = split_once(&mut probe, &Ok(fetched), &mut record, &pins, &mut tally);
        let bosses = entry_of(&entries, "gh:rusty-key/spoken-wow/bosses-audio");
        assert_eq!(bosses.addon.folders, ["SpokenBossesAudio"]);
        assert_eq!(
            tally.pinned,
            [
                "gh:rusty-key/spoken-wow/bosses-audio keeps SpokenBossesAudio, as bosses-audio/v2.0.0 is another addon's"
            ]
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
    fn a_renamed_split_repository_answers_for_its_lines_under_the_old_name() {
        let mut state = State::empty();
        state.note_repo(SPOKEN, "seed");
        let mut heads = heads_of(&[SPOKEN]);
        heads.found.get_mut(SPOKEN).unwrap().name = "rusty-key/spoken".into();
        let mut tally = Tally::default();
        let (_, to_read) = triage_heads(
            &heads,
            &mut state,
            &spoken_curated(),
            &HandPicked::default(),
            1,
            &mut tally,
        );
        assert_eq!(to_read, ["rusty-key/spoken"]);
        assert!(tally.answered.contains("gh:rusty-key/spoken-wow"));
        assert!(
            tally
                .answered
                .contains("gh:rusty-key/spoken-wow/quests-audio")
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
        fn read_zip(&mut self, asset: &GhAsset, hints: &[&str]) -> Result<ZipScan, String> {
            let bytes = self.zips.get(&asset.name).ok_or("no such zip")?;
            scan_zip(std::io::Cursor::new(bytes.clone()), hints).map_err(|e| e.to_string())
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
        let settled = settle(scanned, &curated, &picks, None, BUILT_AT);
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
            },
            crate::curated::PackSpec {
                addon: "gh:rusty-key/spoken-wow".into(),
                name: "Boss voices".into(),
                needed: false,
                ids: vec!["gh:rusty-key/spoken-wow/bosses-audio".into()],
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
}
