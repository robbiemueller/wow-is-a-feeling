//! Everything that talks to GitHub: discovering repositories and fetching their
//! metadata in GraphQL batches.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::detect::{GhAsset, GhRelease};
use crate::http::{Channel, FetchError, Http};
use crate::text::{encode_query, valid_repo_name};

const STRONGBOX_URL: &str =
    "https://raw.githubusercontent.com/ogri-la/strongbox-catalogue/master/full-catalogue.json";
const GRAPHQL_URL: &str = "https://api.github.com/graphql";
/// Repositories per query. GitHub answers big queries with RESOURCE_LIMITS_EXCEEDED
/// and blanks out asset nodes, depending on how many assets they cover, so the
/// size adapts as the run goes.
const MAX_BATCH: usize = 50;
/// The quick first look asks so little per repository that bigger batches are fine.
const MAX_HEAD_BATCH: usize = 100;
/// Repositories last pushed before this can't have a Forever build.
const PUSHED_SINCE: &str = "2026-07-01";

const TOPICS: [&str; 4] = [
    "wow-forever",
    "wow-addon",
    "world-of-warcraft-addon",
    "wow-addons",
];

/// Code search returns one hit per file and stops at 1000, so the broad query is
/// split by file size to see past that.
const CODE_QUERIES: [&str; 7] = [
    "\"Interface: 16001\" extension:toc size:<400",
    "\"Interface: 16001\" extension:toc size:400..700",
    "\"Interface: 16001\" extension:toc size:700..1100",
    "\"Interface: 16001\" extension:toc size:1100..2000",
    "\"Interface: 16001\" extension:toc size:>2000",
    "filename:_Forever extension:toc",
    "filename:_Camelot extension:toc",
];

/// The GraphQL fields per repository. Batches of 50 cost about 9 points.
const REPO_FRAGMENT: &str = "fragment F on Repository {
  nameWithOwner description stargazerCount isArchived isFork isTemplate isEmpty isMirror isDisabled
  parent { nameWithOwner }
  licenseInfo { spdxId }
  repositoryTopics(first: 20) { nodes { topic { name } } }
  releases(first: 15, orderBy: {field: CREATED_AT, direction: DESC}) {
    nodes {
      tagName isPrerelease isDraft publishedAt
      releaseAssets(first: 40) { nodes { name size downloadCount downloadUrl digest } }
    }
  }
}";

/// The quick first look: enough to reject a repository or to see that it has no
/// new release, in which case nothing else needs fetching. About 2 points per 100.
const HEAD_FRAGMENT: &str = "fragment F on Repository {
  nameWithOwner stargazerCount isArchived isFork isTemplate isEmpty isMirror isDisabled
  licenseInfo { spdxId }
  releases(first: 1, orderBy: {field: CREATED_AT, direction: DESC}) { nodes { tagName isDraft } }
}";

/// A repository as the rest of the pipeline sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoInfo {
    pub name: String,
    pub description: Option<String>,
    pub stars: u32,
    pub archived: bool,
    pub fork_of: Option<String>,
    pub is_fork: bool,
    pub template: bool,
    pub empty: bool,
    pub mirror: bool,
    pub disabled: bool,
    pub license: Option<String>,
    pub topics: Vec<String>,
    pub releases: Vec<GhRelease>,
}

#[derive(Debug, Default)]
pub struct BatchResult {
    /// Keyed by the name that was asked for. The info's own name is the current
    /// one, which differs after a rename.
    pub found: BTreeMap<String, RepoInfo>,
    /// GitHub says these don't exist any more.
    pub gone: Vec<String>,
    /// Couldn't get an answer this run. They keep their last-known results.
    pub failed: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepo {
    name_with_owner: String,
    description: Option<String>,
    #[serde(default)]
    stargazer_count: u32,
    #[serde(default)]
    is_archived: bool,
    #[serde(default)]
    is_fork: bool,
    #[serde(default)]
    is_template: bool,
    #[serde(default)]
    is_empty: bool,
    #[serde(default)]
    is_mirror: bool,
    #[serde(default)]
    is_disabled: bool,
    parent: Option<RawName>,
    license_info: Option<RawLicense>,
    repository_topics: Option<Nodes<RawTopicNode>>,
    releases: Option<Nodes<RawRelease>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawName {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLicense {
    spdx_id: Option<String>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
struct RawTopicNode {
    topic: RawTopic,
}

#[derive(Deserialize)]
struct RawTopic {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    is_prerelease: bool,
    #[serde(default)]
    is_draft: bool,
    published_at: Option<String>,
    release_assets: Option<Nodes<RawAsset>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAsset {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    download_count: u64,
    download_url: String,
    digest: Option<String>,
}

impl RawRepo {
    /// A null node means GitHub gave up on that part of the answer, not that it's empty.
    fn has_blanks(&self) -> bool {
        self.releases.as_ref().is_some_and(|releases| {
            releases.nodes.iter().any(|release| match release {
                Some(release) => release
                    .release_assets
                    .as_ref()
                    .is_some_and(|assets| assets.nodes.iter().any(Option::is_none)),
                None => true,
            })
        })
    }

    fn into_info(self) -> RepoInfo {
        let releases = self
            .releases
            .map(|r| r.nodes)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter(|r| !r.is_draft)
            .map(RawRelease::into_release)
            .collect();
        RepoInfo {
            name: self.name_with_owner,
            description: self.description.filter(|d| !d.trim().is_empty()),
            stars: self.stargazer_count,
            archived: self.is_archived,
            is_fork: self.is_fork,
            fork_of: self.parent.map(|p| p.name_with_owner),
            template: self.is_template,
            empty: self.is_empty,
            mirror: self.is_mirror,
            disabled: self.is_disabled,
            license: self
                .license_info
                .and_then(|l| l.spdx_id)
                .filter(|id| id != "NOASSERTION"),
            topics: self
                .repository_topics
                .map(|t| t.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|n| n.topic.name)
                .collect(),
            releases,
        }
    }
}

impl RawRelease {
    fn into_release(self) -> GhRelease {
        GhRelease {
            tag: self.tag_name,
            prerelease: self.is_prerelease,
            published: self
                .published_at
                .as_deref()
                .and_then(parse_iso_time)
                .unwrap_or(0),
            assets: self
                .release_assets
                .map(|a| a.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|a| GhAsset {
                    name: a.name,
                    size: a.size,
                    downloads: a.download_count,
                    url: a.download_url,
                    sha256: a.digest.as_deref().and_then(parse_digest),
                })
                .collect(),
        }
    }
}

/// `sha256:ABC...` gives lowercase hex. Other algorithms aren't usable.
pub fn parse_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// `2026-11-03T22:34:00Z` or `2026-11-04T22:37:15-03:00` to Unix seconds.
pub fn parse_iso_time(text: &str) -> Option<i64> {
    let digits = |s: &str| -> Option<i64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let (year, month, day) = (
        digits(text.get(0..4)?)?,
        digits(text.get(5..7)?)?,
        digits(text.get(8..10)?)?,
    );
    let (hour, minute, second) = (
        digits(text.get(11..13)?)?,
        digits(text.get(14..16)?)?,
        digits(text.get(17..19)?)?,
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut seconds = days * 86_400 + hour * 3600 + minute * 60 + second;

    let zone = text.get(19..)?;
    if zone != "Z" && !zone.is_empty() {
        // Skip fractional seconds, then apply the offset.
        let offset = zone.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        if let Some(sign) = offset.chars().next().filter(|c| *c == '+' || *c == '-') {
            let hours = digits(offset.get(1..3)?)?;
            let minutes = digits(offset.get(4..6)?)?;
            let shift = hours * 3600 + minutes * 60;
            seconds += if sign == '+' { -shift } else { shift };
        }
    }
    Some(seconds)
}

fn repo_query(names: &[String], fragment: &str) -> String {
    let mut query = String::from("query { rateLimit { cost remaining resetAt }\n");
    for (i, name) in names.iter().enumerate() {
        let (owner, repo) = name.split_once('/').unwrap_or((name, ""));
        query.push_str(&format!(
            "r{i}: repository(owner: \"{owner}\", name: \"{repo}\") {{ ...F }}\n"
        ));
    }
    query.push_str("}\n");
    query.push_str(fragment);
    query
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct GraphQlReply {
    data: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug)]
enum BatchReply {
    Answered(BatchResult),
    RateLimited,
    TooBig,
}

/// Interpret one batch's reply. `Err` means the whole batch should be retried or split.
fn read_batch(names: &[String], body: &[u8]) -> Result<BatchReply, String> {
    let reply: GraphQlReply =
        serde_json::from_slice(body).map_err(|e| format!("GraphQL reply doesn't parse: {e}"))?;
    if reply.errors.iter().any(|e| e.kind == "RATE_LIMITED") {
        return Ok(BatchReply::RateLimited);
    }
    if reply
        .errors
        .iter()
        .any(|e| e.kind == "RESOURCE_LIMITS_EXCEEDED")
    {
        return Ok(BatchReply::TooBig);
    }
    let Some(data) = reply.data else {
        return Err(format!(
            "GraphQL returned no data ({} errors)",
            reply.errors.len()
        ));
    };
    let mut result = BatchResult::default();
    for (i, name) in names.iter().enumerate() {
        let alias = format!("r{i}");
        let error_kind = reply
            .errors
            .iter()
            .find(|e| e.path.first().and_then(|p| p.as_str()) == Some(alias.as_str()))
            .map(|e| e.kind.as_str());
        match (
            data.get(&alias).filter(|value| !value.is_null()),
            error_kind,
        ) {
            (None, Some("NOT_FOUND")) => result.gone.push(name.clone()),
            (Some(value), None) => match serde_json::from_value::<RawRepo>(value.clone()) {
                Ok(raw) if raw.has_blanks() => {
                    eprintln!("{name}: GitHub's answer has gaps in it");
                    result.failed.push(name.clone());
                }
                Ok(raw) => {
                    result.found.insert(name.clone(), raw.into_info());
                }
                Err(problem) => {
                    eprintln!("{name}: unexpected GraphQL data: {problem}");
                    result.failed.push(name.clone());
                }
            },
            _ => result.failed.push(name.clone()),
        }
    }
    Ok(BatchReply::Answered(result))
}

/// Fetches one batch into `into`, splitting it when GitHub can't manage it.
/// Returns true when the batch was too big, so the caller can ask for less next time.
fn fetch_batch(
    http: &Http,
    url: &str,
    fragment: &str,
    names: &[String],
    into: &mut BatchResult,
) -> bool {
    let body = serde_json::json!({ "query": repo_query(names, fragment) }).to_string();
    let mut waits = 0;
    let mut too_big = false;
    let outcome = loop {
        let reply = match http.post_json(Channel::GraphQl, url, &body) {
            Ok(reply) => reply,
            Err(error) => break Err(error.to_string()),
        };
        match read_batch(names, &reply.body) {
            Ok(BatchReply::Answered(batch)) => break Ok(batch),
            Ok(BatchReply::RateLimited) if waits < 3 => {
                waits += 1;
                http.wait_for_reset(reply.rate_reset);
            }
            Ok(BatchReply::RateLimited) => break Err("still rate limited".to_string()),
            Ok(BatchReply::TooBig) => {
                too_big = true;
                break Err("GitHub says the query is too big".to_string());
            }
            Err(problem) => break Err(problem),
        }
    };
    match outcome {
        Ok(batch) => {
            into.found.extend(batch.found);
            into.gone.extend(batch.gone);
            into.failed.extend(batch.failed);
        }
        // A big query can also time out on GitHub's side, so try smaller ones before giving up.
        Err(problem) if names.len() > 1 => {
            eprintln!(
                "graphql batch of {} failed ({problem}), splitting it",
                names.len()
            );
            let (left, right) = names.split_at(names.len() / 2);
            too_big |= fetch_batch(http, url, fragment, left, into);
            too_big |= fetch_batch(http, url, fragment, right, into);
        }
        Err(problem) => {
            eprintln!("{}: {problem}", names[0]);
            into.failed.push(names[0].clone());
        }
    }
    too_big
}

/// The next batch size: halve after trouble, and creep back up after success,
/// because each query that comes back too big is a wasted one.
fn next_batch_size(current: usize, too_big: bool, max: usize) -> usize {
    if too_big {
        (current / 2).max(1)
    } else {
        (current + 2).min(max)
    }
}

fn fetch_all(http: &Http, url: &str, fragment: &str, max: usize, names: &[String]) -> BatchResult {
    let mut result = BatchResult::default();
    let mut size = max;
    let mut at = 0;
    let mut queries = 0;
    while at < names.len() {
        if queries % 20 == 0 {
            eprintln!("graphql: {at} of {} repositories", names.len());
        }
        queries += 1;
        let end = (at + size).min(names.len());
        let too_big = fetch_batch(http, url, fragment, &names[at..end], &mut result);
        size = next_batch_size(size, too_big, max);
        at = end;
    }
    result
}

/// A quick look at every repository. The answers have flags and the newest tag,
/// but no release assets.
pub fn fetch_heads(http: &Http, names: &[String]) -> BatchResult {
    fetch_all(http, GRAPHQL_URL, HEAD_FRAGMENT, MAX_HEAD_BATCH, names)
}

/// Everything about the repositories that need a proper look.
pub fn fetch_repos(http: &Http, names: &[String]) -> BatchResult {
    fetch_all(http, GRAPHQL_URL, REPO_FRAGMENT, MAX_BATCH, names)
}

/// A repository found in the strongbox catalogue.
#[derive(Debug, PartialEq, Eq)]
pub struct StrongboxRepo {
    pub name: String,
    pub downloads: u64,
}

#[derive(Deserialize)]
struct StrongboxFile {
    #[serde(default, rename = "addon-summary-list")]
    addons: Vec<StrongboxRow>,
}

#[derive(Deserialize)]
struct StrongboxRow {
    #[serde(default)]
    source: String,
    #[serde(default, rename = "source-id")]
    source_id: String,
    #[serde(default, rename = "game-track-list")]
    tracks: Vec<String>,
    #[serde(default, rename = "download-count")]
    downloads: u64,
}

pub fn parse_strongbox(body: &[u8]) -> Result<Vec<StrongboxRepo>, String> {
    let file: StrongboxFile = serde_json::from_slice(body)
        .map_err(|e| format!("the strongbox catalogue doesn't parse: {e}"))?;
    Ok(file
        .addons
        .into_iter()
        .filter(|row| row.source == "github" && row.tracks.iter().any(|t| t == "forever"))
        .filter(|row| valid_repo_name(&row.source_id))
        .map(|row| StrongboxRepo {
            name: row.source_id,
            downloads: row.downloads,
        })
        .collect())
}

/// A new copy of the strongbox catalogue.
pub struct StrongboxUpdate {
    pub repos: Vec<StrongboxRepo>,
    pub etag: Option<String>,
}

/// Fetch the strongbox catalogue unless it's unchanged. `Ok(None)` means unchanged.
pub fn fetch_strongbox(
    http: &Http,
    etag: Option<&str>,
) -> Result<Option<StrongboxUpdate>, FetchError> {
    let headers: Vec<(&str, &str)> = etag.map(|e| ("if-none-match", e)).into_iter().collect();
    let reply = http.get_with(
        Channel::Raw,
        STRONGBOX_URL,
        &headers,
        crate::http::SMALL_BODY,
    )?;
    if reply.status == 304 {
        return Ok(None);
    }
    let repos = parse_strongbox(&reply.body).map_err(FetchError::Transport)?;
    Ok(Some(StrongboxUpdate {
        repos,
        etag: reply.etag,
    }))
}

#[derive(Deserialize)]
struct SearchPage<T> {
    #[serde(default)]
    total_count: u64,
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

#[derive(Deserialize)]
struct RepoHit {
    full_name: String,
}

#[derive(Deserialize)]
struct CodeHit {
    repository: RepoHit,
}

/// Reads up to GitHub's limit of 10 pages. A short page doesn't mean the last page:
/// code search drops hits it won't show after counting them, so only an empty
/// page or having seen everything ends it.
fn search_pages<T: serde::de::DeserializeOwned>(
    http: &Http,
    channel: Channel,
    url: &str,
    query: &str,
    extra: &str,
    mut take: impl FnMut(T),
) -> Result<u64, String> {
    let mut total = 0;
    let mut seen = 0u64;
    for page in 1..=10 {
        let url = format!(
            "{url}?q={}&per_page=100&page={page}{extra}",
            encode_query(query)
        );
        let reply = http.get(channel, &url).map_err(|e| e.to_string())?;
        let parsed: SearchPage<T> = serde_json::from_slice(&reply.body)
            .map_err(|e| format!("search reply doesn't parse: {e}"))?;
        total = parsed.total_count;
        let count = parsed.items.len() as u64;
        seen += count;
        parsed.items.into_iter().for_each(&mut take);
        if count == 0 || seen >= total {
            break;
        }
    }
    Ok(total)
}

/// Repositories with the addon topics, pushed recently enough to matter.
pub fn search_topics(http: &Http) -> Vec<String> {
    let mut found = Vec::new();
    for topic in TOPICS {
        let query = format!("topic:{topic} pushed:>={PUSHED_SINCE}");
        // One order only reaches the first 1000. A second order reaches past that for big topics.
        for order in ["&sort=updated&order=desc", "&sort=stars&order=desc"] {
            let result = search_pages::<RepoHit>(
                http,
                Channel::Search,
                "https://api.github.com/search/repositories",
                &query,
                order,
                |hit| {
                    found.push(hit.full_name);
                },
            );
            match result {
                Ok(total) if total <= 1000 => break,
                Ok(_) => {}
                Err(problem) => {
                    eprintln!("topic search {topic}: {problem}");
                    break;
                }
            }
        }
    }
    found.retain(|name| valid_repo_name(name));
    found
}

/// Repositories whose TOC files declare Forever.
pub fn search_code(http: &Http) -> Vec<String> {
    let mut found = Vec::new();
    for query in CODE_QUERIES {
        let url = "https://api.github.com/search/code";
        let result = search_pages::<CodeHit>(http, Channel::CodeSearch, url, query, "", |hit| {
            found.push(hit.repository.full_name);
        });
        if let Err(problem) = result {
            eprintln!("code search `{query}`: {problem}");
        }
    }
    found.retain(|name| valid_repo_name(name));
    found.sort();
    found.dedup();
    found
}

/// A tag, newest first, with the date of the commit it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInfo {
    pub name: String,
    pub date: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRef {
    name: String,
    target: Option<RawTarget>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTarget {
    committed_date: Option<String>,
    tagger: Option<RawTagger>,
    target: Option<Box<RawTarget>>,
}

#[derive(Deserialize)]
struct RawTagger {
    date: Option<String>,
}

impl RawTarget {
    fn date(&self) -> Option<i64> {
        self.committed_date
            .as_deref()
            .or(self.tagger.as_ref().and_then(|t| t.date.as_deref()))
            .and_then(parse_iso_time)
            .or_else(|| self.target.as_ref().and_then(|t| t.date()))
    }
}

/// The newest tags of a curated repository, and its metadata.
pub fn fetch_tags(http: &Http, repo: &str, count: u32) -> Result<(RepoInfo, Vec<TagInfo>), String> {
    let Some((owner, name)) = repo.split_once('/').filter(|_| valid_repo_name(repo)) else {
        return Err(format!("{repo} isn't an owner/repo name"));
    };
    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ ...F
          refs(refPrefix: \"refs/tags/\", first: {count}, orderBy: {{field: TAG_COMMIT_DATE, direction: DESC}}) {{
            nodes {{ name target {{ ... on Commit {{ committedDate }}
              ... on Tag {{ tagger {{ date }} target {{ ... on Commit {{ committedDate }} }} }} }} }}
          }}
        }} }}\n{REPO_FRAGMENT}"
    );
    let body = serde_json::json!({ "query": query }).to_string();
    let reply = http
        .post_json(Channel::GraphQl, GRAPHQL_URL, &body)
        .map_err(|e| e.to_string())?;
    read_tags(&reply.body)
}

fn read_tags(body: &[u8]) -> Result<(RepoInfo, Vec<TagInfo>), String> {
    let reply: GraphQlReply =
        serde_json::from_slice(body).map_err(|e| format!("GraphQL reply doesn't parse: {e}"))?;
    if let Some(error) = reply.errors.first() {
        return Err(format!("GitHub answered with an error ({})", error.kind));
    }
    let value = reply
        .data
        .and_then(|mut data| data.remove("repository"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| "GitHub doesn't know this repository".to_string())?;
    #[derive(Deserialize)]
    struct WithRefs {
        refs: Nodes<RawRef>,
    }
    let refs: WithRefs = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let tags = refs
        .refs
        .nodes
        .into_iter()
        .flatten()
        .map(|r| TagInfo {
            date: r.target.as_ref().and_then(RawTarget::date).unwrap_or(0),
            name: r.name,
        })
        .collect();
    let raw: RawRepo = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok((raw.into_info(), tags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::{client, reply, serve};

    #[test]
    fn digests_are_validated() {
        let hex = "49FB2833ed6717f6b4d8931ba43207c7ad648695c909143e24264f72541fb042";
        assert_eq!(
            parse_digest(&format!("sha256:{hex}")),
            Some(hex.to_ascii_lowercase())
        );
        assert_eq!(parse_digest("sha1:abc"), None);
        assert_eq!(parse_digest("sha256:short"), None);
        assert_eq!(parse_digest(""), None);
    }

    #[test]
    fn timestamps_parse_with_offsets() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_time("2026-07-01T00:00:00Z"), Some(1_782_864_000));
        assert_eq!(
            parse_iso_time("2026-11-04T22:37:15-03:00"),
            parse_iso_time("2026-11-05T01:37:15Z")
        );
        assert_eq!(
            parse_iso_time("2026-11-05T01:37:15+01:00"),
            parse_iso_time("2026-11-05T00:37:15Z")
        );
        assert_eq!(
            parse_iso_time("2026-11-05T01:37:15.123Z"),
            parse_iso_time("2026-11-05T01:37:15Z")
        );
        assert_eq!(parse_iso_time("yesterday"), None);
        assert_eq!(parse_iso_time(""), None);
        assert_eq!(parse_iso_time("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn strongbox_keeps_only_forever_github_rows() {
        let body = br#"{"addon-summary-list":[
            {"source":"github","source-id":"A/One","game-track-list":["forever","retail"],"download-count":5},
            {"source":"github","source-id":"B/Two","game-track-list":["retail"]},
            {"source":"wowinterface","source-id":"123","game-track-list":["forever"]},
            {"source":"github","source-id":"bad name","game-track-list":["forever"]}
        ]}"#;
        let repos = parse_strongbox(body).unwrap();
        assert_eq!(
            repos,
            [StrongboxRepo {
                name: "A/One".into(),
                downloads: 5
            }]
        );
        assert!(parse_strongbox(b"not json").is_err());
    }

    fn repo_json(name: &str) -> serde_json::Value {
        serde_json::json!({
            "nameWithOwner": name,
            "description": " ",
            "stargazerCount": 12,
            "isArchived": false, "isFork": true, "isTemplate": false, "isEmpty": false, "isMirror": false, "isDisabled": false,
            "parent": {"nameWithOwner": "Up/Stream"},
            "licenseInfo": {"spdxId": "NOASSERTION"},
            "repositoryTopics": {"nodes": [{"topic": {"name": "wow-addon"}}]},
            "releases": {"nodes": [
                {"tagName": "v1", "isPrerelease": false, "isDraft": false, "publishedAt": "2026-10-01T00:00:00Z",
                 "releaseAssets": {"nodes": [{"name": "a.zip", "size": 9, "downloadCount": 3,
                    "downloadUrl": "https://github.com/x/y/a.zip", "digest": null}]}},
                {"tagName": "draft", "isPrerelease": false, "isDraft": true, "publishedAt": null,
                 "releaseAssets": {"nodes": []}}
            ]}
        })
    }

    #[test]
    fn batches_separate_found_gone_and_failed() {
        let names: Vec<String> = ["A/a", "B/b", "C/c", "D/d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let body = serde_json::json!({
            "data": {
                "rateLimit": {"cost": 1, "remaining": 100, "resetAt": "x"},
                "r0": repo_json("A/renamed"),
                "r1": null,
                "r2": null,
                "r3": {"nameWithOwner": 12}
            },
            "errors": [
                {"type": "NOT_FOUND", "path": ["r1"], "message": "gone"},
                {"type": "FORBIDDEN", "path": ["r2"], "message": "no"}
            ]
        })
        .to_string();
        let BatchReply::Answered(batch) = read_batch(&names, body.as_bytes()).unwrap() else {
            panic!("expected an answer");
        };
        let info = &batch.found["A/a"];
        assert_eq!(
            info.name, "A/renamed",
            "a rename is reported through the name"
        );
        assert_eq!(info.stars, 12);
        assert_eq!(
            info.description, None,
            "a blank description is no description"
        );
        assert_eq!(info.license, None, "NOASSERTION isn't a license");
        assert_eq!(info.fork_of.as_deref(), Some("Up/Stream"));
        assert_eq!(info.topics, ["wow-addon"]);
        assert_eq!(info.releases.len(), 1, "drafts are dropped");
        assert_eq!(info.releases[0].assets[0].downloads, 3);
        assert_eq!(batch.gone, ["B/b"]);
        assert_eq!(batch.failed, ["C/c", "D/d"]);
    }

    #[test]
    fn a_rate_limit_error_and_a_null_data_reply_are_recognized() {
        let names = vec!["A/a".to_string()];
        let limited = br#"{"errors":[{"type":"RATE_LIMITED","message":"slow"}]}"#;
        assert!(matches!(
            read_batch(&names, limited),
            Ok(BatchReply::RateLimited)
        ));
        assert!(read_batch(&names, br#"{"data":null,"errors":[{"message":"boom"}]}"#).is_err());
        assert!(read_batch(&names, b"<html>502</html>").is_err());
    }

    /// GitHub really does this: a 200 with most of the data, every asset node
    /// nulled out, and the reason only in the error list.
    #[test]
    fn resource_limits_mean_too_big_never_no_assets() {
        let names = vec!["A/a".to_string()];
        let blanked = serde_json::json!({
            "data": {"r0": {
                "nameWithOwner": "A/a",
                "releases": {"nodes": [{"tagName": "v1", "publishedAt": "2026-10-01T00:00:00Z",
                    "releaseAssets": {"nodes": [null, null]}}]}
            }},
            "errors": [{"type": "RESOURCE_LIMITS_EXCEEDED",
                "path": ["r0", "releases", "nodes", 0, "releaseAssets", "nodes", 0, "name"],
                "message": "Resource limits for this query exceeded."}]
        })
        .to_string();
        assert!(matches!(
            read_batch(&names, blanked.as_bytes()),
            Ok(BatchReply::TooBig)
        ));
    }

    #[test]
    fn blank_nodes_without_an_explanation_still_count_as_failure() {
        let names = vec!["A/a".to_string()];
        let body = serde_json::json!({"data": {"r0": {
            "nameWithOwner": "A/a",
            "releases": {"nodes": [{"tagName": "v1", "releaseAssets": {"nodes": [null]}}]}
        }}})
        .to_string();
        let BatchReply::Answered(batch) = read_batch(&names, body.as_bytes()).unwrap() else {
            panic!("expected an answer");
        };
        assert_eq!(batch.failed, ["A/a"]);
        assert!(batch.found.is_empty());
    }

    #[test]
    fn an_error_about_part_of_a_repo_fails_that_repo_only() {
        let names = vec!["A/a".to_string(), "B/b".to_string()];
        let body = serde_json::json!({
            "data": {"r0": repo_json("A/a"), "r1": repo_json("B/b")},
            "errors": [{"type": "SERVICE_UNAVAILABLE", "path": ["r1", "releases"]}]
        })
        .to_string();
        let BatchReply::Answered(batch) = read_batch(&names, body.as_bytes()).unwrap() else {
            panic!("expected an answer");
        };
        assert_eq!(batch.found.len(), 1);
        assert_eq!(batch.failed, ["B/b"]);
    }

    #[test]
    fn search_keeps_going_past_short_pages_and_stops_when_done() {
        let page = |names: &[&str]| {
            let items: Vec<serde_json::Value> = names
                .iter()
                .map(|n| serde_json::json!({"repository": {"full_name": n}}))
                .collect();
            serde_json::json!({"total_count": 4, "items": items})
                .to_string()
                .into_bytes()
        };
        let (base, seen) = serve(vec![
            reply(200, &[], &page(&["a/1"])),
            reply(200, &[], &page(&["a/2", "a/3"])),
            reply(200, &[], &page(&["a/4"])),
        ]);
        let http = client(Some("t"));
        let mut found = Vec::new();
        let total = search_pages::<CodeHit>(
            &http,
            Channel::CodeSearch,
            &format!("{base}/search/code"),
            "q",
            "",
            |hit| found.push(hit.repository.full_name),
        )
        .unwrap();
        assert_eq!(total, 4);
        assert_eq!(found, ["a/1", "a/2", "a/3", "a/4"]);
        assert_eq!(
            seen.lock().unwrap().len(),
            3,
            "no fourth page after seeing everything"
        );
    }

    #[test]
    fn search_stops_at_an_empty_page_even_if_the_total_promises_more() {
        let (base, seen) = serve(vec![
            reply(
                200,
                &[],
                br#"{"total_count": 500, "items": [{"full_name": "a/1"}]}"#,
            ),
            reply(200, &[], br#"{"total_count": 500, "items": []}"#),
        ]);
        let http = client(Some("t"));
        let mut found = Vec::new();
        search_pages::<RepoHit>(
            &http,
            Channel::Search,
            &format!("{base}/search/repositories"),
            "q",
            "",
            |hit| found.push(hit.full_name),
        )
        .unwrap();
        assert_eq!(found, ["a/1"]);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_head_answer_has_the_flags_and_the_newest_tag() {
        let names = vec!["A/a".to_string()];
        let body = serde_json::json!({"data": {"r0": {
            "nameWithOwner": "A/a", "stargazerCount": 5, "isArchived": true, "isFork": false,
            "isTemplate": false, "isEmpty": false, "isMirror": false, "isDisabled": false,
            "licenseInfo": {"spdxId": "MIT"},
            "releases": {"nodes": [{"tagName": "v9", "isDraft": false}]}
        }}})
        .to_string();
        let BatchReply::Answered(batch) = read_batch(&names, body.as_bytes()).unwrap() else {
            panic!("expected an answer");
        };
        let head = &batch.found["A/a"];
        assert!(head.archived);
        assert_eq!(head.stars, 5);
        assert_eq!(head.license.as_deref(), Some("MIT"));
        assert_eq!(head.releases[0].tag, "v9");
        assert!(head.releases[0].assets.is_empty());
    }

    #[test]
    fn batch_sizes_adapt() {
        assert_eq!(next_batch_size(50, true, 50), 25);
        assert_eq!(next_batch_size(1, true, 50), 1);
        assert_eq!(next_batch_size(25, false, 50), 27);
        assert_eq!(next_batch_size(1, false, 50), 3);
        assert_eq!(next_batch_size(50, false, 50), 50);
        assert_eq!(next_batch_size(99, false, 100), 100);
    }

    #[test]
    fn a_too_big_query_is_split_and_later_batches_start_smaller() {
        let names: Vec<String> = (0..60).map(|i| format!("o/r{i}")).collect();
        let too_big =
            serde_json::json!({"data": {}, "errors": [{"type": "RESOURCE_LIMITS_EXCEEDED"}]})
                .to_string()
                .into_bytes();
        let answer = |offset: usize, count: usize| {
            let data: serde_json::Map<String, serde_json::Value> = (0..count)
                .map(|i| (format!("r{i}"), repo_json(&format!("o/r{}", i + offset))))
                .collect();
            serde_json::json!({ "data": data }).to_string().into_bytes()
        };
        let (base, seen) = serve(vec![
            reply(200, &[], &too_big), // 50
            reply(200, &[], &answer(0, 25)),
            reply(200, &[], &answer(25, 25)),
            reply(200, &[], &answer(50, 10)), // what's left, in one go
        ]);
        let http = client(Some("t"));
        let url = format!("{base}/graphql");
        let result = fetch_all(&http, &url, REPO_FRAGMENT, MAX_BATCH, &names);
        assert_eq!(result.found.len(), 60);
        assert!(result.failed.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn query_uses_aliases_and_stays_small() {
        let names: Vec<String> = ["A/a", "B/b"].iter().map(|s| s.to_string()).collect();
        let query = repo_query(&names, REPO_FRAGMENT);
        assert!(query.contains("r0: repository(owner: \"A\", name: \"a\")"));
        assert!(query.contains("r1: repository(owner: \"B\", name: \"b\")"));
        assert!(query.contains("fragment F on Repository"));
    }

    #[test]
    fn tag_dates_handle_lightweight_and_annotated_tags() {
        let body = serde_json::json!({"data": {"repository": {
            "nameWithOwner": "T/D",
            "refs": {"nodes": [
                {"name": "Details.1", "target": {"tagger": {"date": "2026-11-04T22:37:15-03:00"},
                                                 "target": {"committedDate": "2026-11-05T01:36:29Z"}}},
                {"name": "v2", "target": {"committedDate": "2026-10-13T22:41:44Z"}},
                {"name": "odd", "target": null}
            ]}
        }}})
        .to_string();
        let (info, tags) = read_tags(body.as_bytes()).unwrap();
        assert_eq!(info.name, "T/D");
        assert_eq!(
            tags[0].date,
            parse_iso_time("2026-11-05T01:37:15Z").unwrap()
        );
        assert_eq!(
            tags[1].date,
            parse_iso_time("2026-10-13T22:41:44Z").unwrap()
        );
        assert_eq!(tags[2].date, 0);
        assert!(read_tags(br#"{"data":{"repository":null}}"#).is_err());
        let partial = br#"{"data":{"repository":{"nameWithOwner":"T/D","refs":{"nodes":[]}}},"errors":[{"type":"RESOURCE_LIMITS_EXCEEDED"}]}"#;
        assert!(read_tags(partial).is_err());
    }

    #[test]
    fn a_failed_batch_is_split_until_only_the_bad_repo_fails() {
        // Whole batch 502s until it runs out of attempts, then each half answers.
        let good = |alias: &str, name: &str| {
            serde_json::json!({"data": {alias: repo_json(name)}})
                .to_string()
                .into_bytes()
        };
        let mut script: Vec<_> = (0..6).map(|_| reply(502, &[], b"")).collect();
        script.push(reply(200, &[], &good("r0", "A/a")));
        script.extend((0..6).map(|_| reply(500, &[], b"")));
        let (base, _) = serve(script);
        let http = client(Some("t"));
        let names: Vec<String> = ["A/a", "B/b"].iter().map(|s| s.to_string()).collect();
        let mut result = BatchResult::default();
        fetch_batch(
            &http,
            &format!("{base}/graphql"),
            REPO_FRAGMENT,
            &names,
            &mut result,
        );
        assert_eq!(result.found.len(), 1);
        assert_eq!(result.found["A/a"].name, "A/a");
        assert_eq!(result.failed, ["B/b"]);
    }

    #[test]
    fn a_rate_limited_batch_waits_and_retries() {
        let good = serde_json::json!({"data": {"r0": repo_json("A/a")}})
            .to_string()
            .into_bytes();
        let (base, seen) = serve(vec![
            reply(
                200,
                &[("x-ratelimit-reset", "1")],
                br#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
            ),
            reply(200, &[], &good),
        ]);
        let http = client(Some("t"));
        let names = vec!["A/a".to_string()];
        let mut result = BatchResult::default();
        fetch_batch(
            &http,
            &format!("{base}/graphql"),
            REPO_FRAGMENT,
            &names,
            &mut result,
        );
        assert_eq!(result.found.len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
