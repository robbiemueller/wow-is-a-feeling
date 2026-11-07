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
use crate::curated::{self, Curated, Seeds};
use crate::detect::{self, Build, Chosen, GhAsset, Probe};
use crate::gate::run_gate;
use crate::github::{self, RepoInfo};
use crate::http::{Channel, Http, unix_now};
use crate::merge;
use crate::model::{Candidate, ZipScan};
use crate::rangezip::{HttpRange, RangeReader};
use crate::state::{RepoState, State, write_atomic};
use crate::text::{
    clean_author, clean_name, clean_summary, clean_version, round_sig, valid_repo_name,
};
use crate::tier::assign_tiers;
use crate::wowi::{self, WowiState};
use crate::zipscan::{ScanError, scan_zip};

/// Topic and code search only find new repositories, so once a day is plenty.
const SEARCH_EVERY_SECS: i64 = 20 * 60 * 60;
/// How often progress is written to the state file.
const SAVE_EVERY: Duration = Duration::from_secs(120);
/// Significant digits kept for download and star counts in the published file.
const COUNT_DIGITS: u32 = 3;
/// How many releases GraphQL returns per repository. A list this long may hide older ones.
const RELEASES_FETCHED: usize = 15;

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
    if summary.is_empty() {
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
    let release = |build: &Build| Release {
        version: clean_version(&build.tag, &[main, repo_name]),
        url: build.asset.url.clone(),
        size: build.asset.size,
        date: build.published,
        sha256: build.asset.sha256.clone(),
        ..Release::default()
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
}

fn process_repo(
    http: &Http,
    info: &RepoInfo,
    record: &mut RepoState,
    hand_picked: bool,
    out_of_time: &dyn Fn() -> bool,
    tally: &mut Tally,
) -> Option<Candidate> {
    record.license.clone_from(&info.license);
    if let Some(reason) = merge::repo_rejection(info, hand_picked) {
        *tally.rejected.entry(reason.to_string()).or_default() += 1;
        record.entry = None;
        record.releases.clear();
        return None;
    }

    let repo_name = info.name.split('/').nth(1).unwrap_or(&info.name);
    let mut probe = LiveProbe { http };
    let chosen = detect::choose_builds(
        repo_name,
        &info.releases,
        &mut record.releases,
        &mut probe,
        out_of_time,
    );
    for problem in &chosen.errors {
        eprintln!("{} {problem}", info.name);
    }

    match github_candidate(info, &chosen, record.known_downloads) {
        Some(entry) if merge::is_placeholder_name(&entry.addon.name) && !hand_picked => {
            tally.placeholder += 1;
            eprintln!("{}: skipped, its title is only a placeholder", info.name);
            record.entry = None;
            None
        }
        Some(entry) => {
            record.entry = Some(entry.clone());
            Some(entry)
        }
        None if !chosen.errors.is_empty() => {
            if record.entry.is_some() {
                tally.kept_last_known += 1;
            }
            record.entry.clone()
        }
        None => {
            tally.no_build += 1;
            let grey = record
                .entry
                .as_ref()
                .and_then(|previous| grey_previous(previous, info));
            record.entry.clone_from(&grey);
            grey
        }
    }
}

fn run_github(
    http: &Http,
    state: &mut State,
    names: &[String],
    curated: &Curated,
    out_of_time: &dyn Fn() -> bool,
    saver: &mut Saver,
    tally: &mut Tally,
) -> Vec<Candidate> {
    let batch = github::fetch_repos(http, names);
    eprintln!(
        "graphql: {} repositories found, {} gone, {} failed",
        batch.found.len(),
        batch.gone.len(),
        batch.failed.len()
    );
    tally.gone += u32::try_from(batch.gone.len()).unwrap_or(u32::MAX);
    for name in &batch.gone {
        state.repos.remove(name);
    }
    let hand_picked = curated.named_repos();

    let mut entries = Vec::new();
    for name in &batch.failed {
        if let Some(entry) = state
            .repos
            .get(name)
            .and_then(|record| record.entry.clone())
        {
            tally.kept_last_known += 1;
            entries.push(entry);
        }
    }

    let mut processed = BTreeSet::new();
    let total = batch.found.len();
    for (done, (asked, info)) in batch.found.iter().enumerate() {
        if done % 100 == 0 {
            eprintln!("releases: {done} of {total} repositories");
        }
        if !processed.insert(info.name.to_ascii_lowercase()) || curated.excludes_repo(&info.name) {
            state.repos.remove(asked);
            continue;
        }
        state.rename_repo(asked, &info.name);
        let record = state.repos.entry(info.name.clone()).or_default();
        let picked = hand_picked.contains(&info.name.to_ascii_lowercase());
        entries.extend(process_repo(http, info, record, picked, out_of_time, tally));
        saver.maybe_save(state);
    }
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
    let mut entries = Vec::new();
    let total = selected.len();
    for (done, row) in selected.iter().enumerate() {
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
        match wowi::examine(row, &mut probe) {
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
    state.wowi.retain(|id, _| listed.contains(id));
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

fn previous_count(path: Option<&Path>) -> Option<usize> {
    let path = path?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(problem) => {
            eprintln!("{}: {problem}. Skipping the size check.", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<Catalog>(&bytes) {
        Ok(catalog) => Some(catalog.addons.len()),
        Err(problem) => {
            eprintln!("{}: {problem}. Skipping the size check.", path.display());
            None
        }
    }
}

fn gzipped_len(bytes: &[u8]) -> Result<usize, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).map_err(|e| e.to_string())?;
    Ok(encoder.finish().map_err(|e| e.to_string())?.len())
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
        "kept last-known results for {} that failed this run",
        tally.kept_last_known
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
    std::fs::create_dir_all(&options.out)
        .map_err(|e| format!("Couldn't create {}: {e}", options.out.display()))?;

    let state_out = options.out.join("catalog-state.json");
    let mut state = State::load(options.state.as_deref().unwrap_or(&state_out));
    let mut saver = Saver {
        path: state_out,
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

    let (github_entries, dropped) = merge::drop_redundant_forks(packaged);
    for id in &dropped {
        eprintln!("{id}: dropped, a fork with the same folders as a listed repository");
    }
    let (mut candidates, merged) = merge::merge_wowi(github_entries, wowi_entries);
    candidates.extend(direct);
    candidates.retain(|c| !curated.exclude.ids.contains(&c.addon.id));
    finish_candidates(&mut candidates, &curated);

    let (catalog, raw, gz) = publish(
        candidates,
        &curated,
        options.previous.as_deref(),
        &options.out,
    )?;
    print_summary(&catalog, raw, gz, merged, &tally, &http, started.elapsed());
    Ok(())
}

/// Check the finished list and write `catalog.json`. When the gate refuses, nothing
/// is written and whatever was published before stays as it is.
pub fn publish(
    candidates: Vec<Candidate>,
    curated: &Curated,
    previous: Option<&Path>,
    out: &Path,
) -> Result<(Catalog, usize, usize), String> {
    let catalog = assemble(candidates, curated);
    let (catalog, warnings) = run_gate(catalog, &curated.gaps, previous_count(previous))?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let bytes =
        serde_json::to_vec(&catalog).map_err(|e| format!("Couldn't encode the catalog: {e}"))?;
    write_atomic(&out.join("catalog.json"), &bytes)?;
    let gz = gzipped_len(&bytes)?;
    Ok((catalog, bytes.len(), gz))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use feeling_core::catalog::{Addon, Kit};

    use super::*;
    use crate::curated::{GapSpec, Override};
    use crate::detect::GhRelease;

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
            !json.contains("unrelated"),
            "our bookkeeping isn't published"
        );
        assert!(!json.contains('\n'), "the file is minified");
    }

    fn publishable(id: &str) -> Candidate {
        let mut c = addon_candidate(id, "Some Addon", 10);
        c.addon.category = "misc".into();
        c
    }

    #[test]
    fn publishing_writes_a_minified_catalog_the_app_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, raw, gz) = publish(
            vec![publishable("gh:a/b")],
            &Curated::default(),
            None,
            dir.path(),
        )
        .unwrap();
        assert_eq!(catalog.addons.len(), 1);
        let bytes = std::fs::read(dir.path().join("catalog.json")).unwrap();
        assert_eq!(bytes.len(), raw);
        assert!(gz > 0);
        assert!(!bytes.contains(&b'\n'));
        let back: Catalog = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.schema, CATALOG_SCHEMA);
        assert_eq!(back.addons[0].id, "gh:a/b");
    }

    #[test]
    fn a_refused_catalog_leaves_the_published_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let previous = dir.path().join("previous.json");
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
        std::fs::write(&previous, serde_json::to_vec(&old).unwrap()).unwrap();
        let published = dir.path().join("catalog.json");
        std::fs::write(&published, b"the old catalog").unwrap();

        let result = publish(
            vec![publishable("gh:a/b")],
            &Curated::default(),
            Some(&previous),
            dir.path(),
        );
        assert!(result.unwrap_err().contains("Not publishing"));
        assert_eq!(std::fs::read(&published).unwrap(), b"the old catalog");
    }

    #[test]
    fn the_previous_count_tolerates_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(previous_count(None), None);
        assert_eq!(previous_count(Some(&dir.path().join("missing.json"))), None);
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json").unwrap();
        assert_eq!(previous_count(Some(&bad)), None);
        let good = dir.path().join("good.json");
        std::fs::write(&good, br#"{"schema":1,"generated":1,"addons":[]}"#).unwrap();
        assert_eq!(previous_count(Some(&good)), Some(0));
    }

    #[test]
    fn gzip_size_is_plausible() {
        let text = vec![b'a'; 100_000];
        let small = gzipped_len(&text).unwrap();
        assert!(small > 0 && small < 1000);
    }
}
