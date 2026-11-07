//! On-demand README and changelog text for the parchment panel.
//!
//! The panel shows plain paragraphs, so everything that arrives as Markdown or BBCode
//! gets boiled down here by small pure functions. Exact rendering doesn't matter: the
//! text just has to read well, and nothing a README contains may ever reach an HTML
//! renderer or a shell, because there isn't one.

use serde::Deserialize;

use crate::catalog::{Addon, Source};
use crate::error::{Error, Result};
use crate::net::{self, Http};

const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024;
const MAX_TEXT_CHARS: usize = 6000;
const MAX_IMAGES: usize = 3;
const NOT_FOUND: u16 = 404;

/// URL parts that mark an image as a badge, a donation button or similar, not a screenshot.
const BADGE_MARKERS: [&str; 22] = [
    "shields.io",
    "badge",
    "travis-ci",
    "circleci",
    "appveyor",
    "codecov",
    "coveralls",
    "codeclimate",
    "sonarcloud",
    "snyk.io",
    "wakatime",
    "gitter.im",
    "paypal",
    "patreon",
    "ko-fi",
    "buymeacoffee",
    "liberapay",
    "discordapp.com/api",
    "discord.com/api",
    "/workflows/",
    "/actions/",
    "cf.way",
];

/// Tags that sit between blocks of text, so removing one must leave a gap.
const HTML_BLOCK_TAGS: [&str; 16] = [
    "br", "p", "div", "li", "ul", "ol", "tr", "td", "th", "table", "h1", "h2", "h3", "h4", "h5",
    "h6",
];
/// Tags authors really do wrap over several lines, mostly for their attributes.
const MULTILINE_HTML_TAGS: [&str; 10] = [
    "img", "a", "p", "div", "span", "table", "td", "picture", "source", "video",
];
/// How long a tag left open keeps swallowing lines, so one typo can't eat a whole README.
const MAX_OPEN_TAG_LINES: u8 = 16;
const MAX_LINK_NESTING: u8 = 3;
/// Long enough for a `[url=...]` with a signed link in it.
const MAX_BBCODE_TAG_BYTES: usize = 2048;
const MAX_IMAGE_MARKUP_BYTES: usize = 4096;
/// A README with this many badges before its first screenshot has no screenshots.
const MAX_IMAGE_CANDIDATES: usize = 100;

/// What the panel shows for one addon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub about: String,
    pub changelog: Option<String>,
    /// Up to three screenshots, as https URLs the app can fetch through [`Http`].
    /// Written by a stranger, so fetch these with [`Http::get_untrusted`] only.
    pub images: Vec<String>,
}

/// The addresses details come from, so tests can point them at a local server.
struct Endpoints<'a> {
    github_raw: &'a str,
    github_api: &'a str,
    wowi_api: &'a str,
}

/// Where WoWInterface describes one file: `{WOWI_DETAILS_API}/{id}.json`.
pub const WOWI_DETAILS_API: &str = "https://api.mmoui.com/v3/game/WOW/filedetails";

const LIVE: Endpoints<'static> = Endpoints {
    github_raw: "https://raw.githubusercontent.com",
    github_api: "https://api.github.com",
    wowi_api: WOWI_DETAILS_API,
};

/// Fetch an addon's details. Call it on demand, from a worker thread.
pub fn fetch(http: &Http, addon: &Addon) -> Result<Details> {
    fetch_from(http, addon, &LIVE)
}

fn fetch_from(http: &Http, addon: &Addon, endpoints: &Endpoints<'_>) -> Result<Details> {
    match &addon.source {
        Source::Github { repo } if is_plain_repo(repo) => {
            fetch_github(http, repo, addon, endpoints)
        }
        Source::Wowi { id } => fetch_wowi(http, *id, addon, endpoints),
        _ => Ok(Details {
            about: addon.summary.clone(),
            ..Details::default()
        }),
    }
}

/// `repo` comes from the catalog and lands inside URLs, so it has to look like `owner/name`.
fn is_plain_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].iter().all(|part| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    })
}

fn fetch_github(
    http: &Http,
    repo: &str,
    addon: &Addon,
    endpoints: &Endpoints<'_>,
) -> Result<Details> {
    let base = format!("{}/{repo}/HEAD", endpoints.github_raw);
    let readme = first_found(http, &base, &["README.md", "readme.md", "README"])?;
    let (about, images) = match readme {
        Some(markdown) => {
            let about = cap_text(&markdown_to_text(&markdown));
            let images = image_urls_from_markdown(&markdown, &base);
            (about, images)
        }
        None => (addon.summary.clone(), Vec::new()),
    };
    // Extras from here on. The panel is still useful without them.
    let changelog = first_found(http, &base, &["CHANGELOG.md"])
        .ok()
        .flatten()
        .or_else(|| latest_release_body(http, repo, endpoints))
        .map(|markdown| cap_text(&markdown_to_text(&markdown)))
        .filter(|text| !text.is_empty());
    Ok(Details {
        about,
        changelog,
        images,
    })
}

/// The first of `names` that exists. Anything but "not found" is a real failure.
fn first_found(http: &Http, base: &str, names: &[&str]) -> Result<Option<String>> {
    for name in names {
        match http.get_prefix(&format!("{base}/{name}"), MAX_DOWNLOAD_BYTES) {
            Ok(bytes) => return Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(Error::HttpStatus {
                status: NOT_FOUND, ..
            }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Often rate limited, since it's the only REST call the app makes. `None` is fine.
fn latest_release_body(http: &Http, repo: &str, endpoints: &Endpoints<'_>) -> Option<String> {
    #[derive(Deserialize)]
    struct Release {
        body: Option<String>,
    }
    let url = format!("{}/repos/{repo}/releases/latest", endpoints.github_api);
    let bytes = http.get_prefix(&url, MAX_DOWNLOAD_BYTES).ok()?;
    serde_json::from_slice::<Release>(&bytes).ok()?.body
}

fn fetch_wowi(http: &Http, id: u64, addon: &Addon, endpoints: &Endpoints<'_>) -> Result<Details> {
    let details = wowi_file(http, endpoints.wowi_api, id)?;
    let text = |bbcode: Option<String>| cap_text(&bbcode_to_text(&bbcode.unwrap_or_default()));
    let changelog = text(details.changelog);
    Ok(Details {
        about: text(details.description),
        changelog: (!changelog.is_empty()).then_some(changelog),
        // WoWInterface only lists screenshots in its big file list, so the catalog carries them.
        images: addon
            .images
            .iter()
            .filter_map(|url| absolute_https(url))
            .filter(|url| net::is_screenshot_url(url))
            .take(MAX_IMAGES)
            .collect(),
    })
}

/// A new upload WoWInterface is holding until its staff approve it. Meanwhile it answers
/// every download of that addon with a web page, the version it already lists included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldUpload {
    /// As WoWInterface gives it: a stranger's text, to be cleaned before it's shown.
    pub version: String,
    /// Unix seconds, or 0 when WoWInterface doesn't say.
    pub uploaded: i64,
}

/// Whether WoWInterface is holding a new upload of file `id` for approval. `api` is
/// [`WOWI_DETAILS_API`] except in tests.
pub fn wowi_held_upload(http: &Http, api: &str, id: u64) -> Result<Option<HeldUpload>> {
    let file = wowi_file(http, api, id)?;
    if !is_yes(&file.pending) {
        return Ok(None);
    }
    Ok(Some(HeldUpload {
        version: plain_text(&file.version),
        // WoWInterface counts in milliseconds.
        uploaded: whole_number(&file.date).max(0) / 1000,
    }))
}

/// One file's details. Numbers and flags arrive as strings about as often as not.
#[derive(Default, Deserialize)]
struct WowiFile {
    #[serde(rename = "UIDescription", default)]
    description: Option<String>,
    #[serde(rename = "UIChangeLog", default)]
    changelog: Option<String>,
    #[serde(rename = "UIVersion", default)]
    version: serde_json::Value,
    #[serde(rename = "UIDate", default)]
    date: serde_json::Value,
    #[serde(rename = "UIPending", default)]
    pending: serde_json::Value,
}

/// `{api}/{id}.json`, which holds a list of one, or now and then a bare object.
fn wowi_file(http: &Http, api: &str, id: u64) -> Result<WowiFile> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Response {
        Many(Vec<WowiFile>),
        One(WowiFile),
    }

    let url = format!("{api}/{id}.json");
    let bytes = http.get_prefix(&url, MAX_DOWNLOAD_BYTES)?;
    let parsed: Response = serde_json::from_slice(&bytes).map_err(|e| Error::Parse {
        what: "this addon's details".into(),
        detail: e.to_string(),
    })?;
    Ok(match parsed {
        Response::One(file) => file,
        Response::Many(list) => list.into_iter().next().unwrap_or_default(),
    })
}

fn is_yes(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(yes) => *yes,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        serde_json::Value::String(s) => {
            let s = s.trim();
            s == "1" || s.eq_ignore_ascii_case("true")
        }
        _ => false,
    }
}

fn plain_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.trim().to_owned(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn whole_number(value: &serde_json::Value) -> i64 {
    match value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Cut long text at a paragraph break if one's nearby, else a space, and say so.
fn cap_text(text: &str) -> String {
    let Some((cut, _)) = text.char_indices().nth(MAX_TEXT_CHARS) else {
        return text.to_string();
    };
    let head = &text[..cut];
    let nearby = head.len().saturating_sub(MAX_TEXT_CHARS / 4);
    let keep = head
        .rfind("\n\n")
        .filter(|at| *at >= nearby)
        .or_else(|| head.rfind(char::is_whitespace))
        .unwrap_or(cut);
    format!("{}…", head[..keep].trim_end())
}

/// Markdown to plain, readable paragraphs: no badges, HTML, table markup or images.
pub fn markdown_to_text(markdown: &str) -> String {
    let source = strip_html_comments(markdown);
    let mut blocks = TextBlocks::default();
    let mut fence: Option<&str> = None;
    let mut open_tag_lines = 0u8;
    let mut previous_was_item = false;
    for raw_line in source.lines() {
        if let Some(marker) = fence {
            if raw_line.trim().starts_with(marker) {
                fence = None;
                blocks.blank();
            } else {
                blocks.line(raw_line.trim_end());
            }
            continue;
        }
        let mut line = raw_line.trim();
        if open_tag_lines > 0 {
            match line.find('>') {
                Some(at) => {
                    line = line[at + 1..].trim();
                    open_tag_lines = 0;
                }
                None => {
                    open_tag_lines -= 1;
                    continue;
                }
            }
        }
        if let Some(marker) = fence_marker(line) {
            fence = Some(marker);
            blocks.blank();
            continue;
        }
        if line.is_empty() || is_rule(line) {
            blocks.blank();
            previous_was_item = false;
            continue;
        }
        if is_table_separator(line) || is_reference_definition(line) {
            continue;
        }
        // A tag still open at the end of the line continues on the next ones.
        if let Some(at) = open_tag_start(line) {
            line = line[..at].trim_end();
            open_tag_lines = MAX_OPEN_TAG_LINES;
            if line.is_empty() {
                continue;
            }
        }
        let quoted = strip_quote_markers(line);
        if is_callout_marker(quoted) {
            continue;
        }
        if let Some(heading) = heading_text(quoted) {
            let text = inline_text(heading);
            if !text.is_empty() {
                blocks.paragraph(&text);
            }
            previous_was_item = false;
        } else if let Some((bullet, item)) = list_item(quoted) {
            let text = inline_text(item);
            if !text.is_empty() {
                blocks.line(&format!("{bullet} {text}"));
                previous_was_item = true;
            }
        } else if quoted.starts_with('|') {
            let cells: Vec<String> = quoted
                .trim_matches('|')
                .split('|')
                .map(inline_text)
                .filter(|cell| !cell.is_empty())
                .collect();
            if !cells.is_empty() {
                blocks.line(&cells.join(" - "));
            }
            previous_was_item = false;
        } else {
            let text = inline_text(quoted);
            if !text.is_empty() {
                if previous_was_item && raw_line.starts_with([' ', '\t']) {
                    blocks.continue_line(&text);
                } else {
                    blocks.flow(&text);
                    previous_was_item = false;
                }
            }
        }
    }
    blocks.finish()
}

fn strip_html_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => &rest[start + end + 3..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `---`, `***`, `___`, and setext heading underlines like `===`.
fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && ['-', '*', '_', '=']
            .iter()
            .any(|marker| compact.chars().all(|c| c == *marker))
}

/// `|---|:--:|` and `---|---`.
fn is_table_separator(line: &str) -> bool {
    line.contains('-')
        && line.contains('|')
        && line.chars().all(|c| matches!(c, '-' | '|' | ':' | ' '))
}

/// `[label]: https://example.com`.
fn is_reference_definition(line: &str) -> bool {
    line.starts_with('[')
        && !line.starts_with("[!")
        && line.find("]:").is_some_and(|at| !line[1..at].contains(']'))
}

/// Where a tag begins that doesn't close on this line. Only names that really do wrap
/// onto several lines count, so `if x <y then` in someone's prose stays prose.
fn open_tag_start(line: &str) -> Option<usize> {
    let at = line.rfind('<')?;
    let after = &line[at + 1..];
    if after.contains('>') {
        return None;
    }
    let name = after.split(char::is_whitespace).next().unwrap_or(after);
    MULTILINE_HTML_TAGS
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case(name))
        .then_some(at)
}

/// `> [!NOTE]` is how GitHub marks a callout. The words on the next line are what matter.
fn is_callout_marker(line: &str) -> bool {
    line.strip_prefix("[!")
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic()))
}

fn strip_quote_markers(mut line: &str) -> &str {
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    line
}

/// `## Heading ##` to `Heading`. Seven hashes or no space isn't a heading.
fn heading_text(line: &str) -> Option<&str> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = line[level..].strip_prefix(' ')?;
    Some(rest.trim_end_matches(['#', ' ']).trim())
}

/// The bullet to show and the item's text.
fn list_item(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line
        .strip_prefix(['-', '*', '+'])
        .and_then(|rest| rest.strip_prefix(' '))
    {
        let rest = rest.trim_start();
        // A task list's checkbox is markup too.
        let rest = ["[ ] ", "[x] ", "[X] "]
            .iter()
            .find_map(|box_mark| rest.strip_prefix(box_mark))
            .unwrap_or(rest);
        return Some(("•".to_string(), rest));
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=4).contains(&digits) {
        let rest = line[digits..].strip_prefix(['.', ')'])?;
        let rest = rest.strip_prefix(' ')?;
        return Some((format!("{}.", &line[..digits]), rest.trim_start()));
    }
    None
}

/// One line of inline Markdown down to its words.
fn inline_text(text: &str) -> String {
    let stripped = strip_inline(text, 0);
    let decoded = decode_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_inline(text: &str, depth: u8) -> String {
    let chars: Vec<char> = text.chars().collect();
    let pairs = bracket_pairs(&chars);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let ticks = backtick_run(&chars, i);
                let body_start = i + ticks;
                match closing_backticks(&chars, body_start, ticks) {
                    Some(end) => {
                        out.extend(&chars[body_start..end]);
                        i = end + ticks;
                    }
                    None => {
                        out.extend(&chars[i..body_start]);
                        i = body_start;
                    }
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => match link_end(&chars, &pairs, i + 1) {
                Some((_, end)) => i = end,
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            '[' => match link_end(&chars, &pairs, i) {
                Some((label_end, end)) => {
                    let label: String = chars[i + 1..label_end].iter().collect();
                    // Real links nest once or twice. Hostile ones don't get to pick our
                    // recursion depth.
                    if depth < MAX_LINK_NESTING {
                        out.push_str(&strip_inline(&label, depth + 1));
                    }
                    i = end;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            '<' => match html_tag(&chars, i) {
                Some(tag) => {
                    out.push_str(&tag.replacement);
                    i = tag.end;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            '~' if chars.get(i + 1) == Some(&'~') => i += 2,
            '*' => {
                let before = i.checked_sub(1).map(|at| chars[at]);
                let after = chars.get(i + 1).copied();
                let spaced =
                    before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace);
                if spaced {
                    out.push(c);
                }
                i += 1;
            }
            '_' => {
                let before = i.checked_sub(1).map(|at| chars[at]);
                let after = chars.get(i + 1).copied();
                let word = |x: Option<char>| x.is_some_and(char::is_alphanumeric);
                // `foo_bar` is a name, not emphasis.
                if word(before) && word(after) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn backtick_run(chars: &[char], start: usize) -> usize {
    chars[start..].iter().take_while(|c| **c == '`').count()
}

/// The next run of exactly `ticks` backticks.
fn closing_backticks(chars: &[char], start: usize, ticks: usize) -> Option<usize> {
    let mut at = start;
    while at < chars.len() {
        if chars[at] == '`' {
            let run = backtick_run(chars, at);
            if run == ticks {
                return Some(at);
            }
            at += run;
        } else {
            at += 1;
        }
    }
    None
}

/// For each `[` and `(`, where its partner is. Worked out once per line, so a line of
/// nothing but brackets can't turn every bracket into a scan of the whole line.
fn bracket_pairs(chars: &[char]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; chars.len()];
    let (mut square, mut round) = (Vec::new(), Vec::new());
    for (at, c) in chars.iter().enumerate() {
        match c {
            '[' => square.push(at),
            '(' => round.push(at),
            ']' => {
                if let Some(open) = square.pop() {
                    pairs[open] = Some(at);
                }
            }
            ')' => {
                if let Some(open) = round.pop() {
                    pairs[open] = Some(at);
                }
            }
            _ => {}
        }
    }
    pairs
}

/// For `[` at `start`: the matching `]`, and where the `(url)` or `[ref]` that makes it a
/// link ends. Brackets nest, because a linked badge is an image inside a link.
fn link_end(chars: &[char], pairs: &[Option<usize>], start: usize) -> Option<(usize, usize)> {
    let label_end = pairs[start]?;
    let target = label_end + 1;
    match chars.get(target) {
        Some('(' | '[') => Some((label_end, pairs[target]? + 1)),
        _ => None,
    }
}

struct HtmlTag {
    end: usize,
    replacement: String,
}

/// An HTML tag or autolink at `start`. `a < b` is neither, and stays.
fn html_tag(chars: &[char], start: usize) -> Option<HtmlTag> {
    // A tag can't contain another `<`, which also keeps `<<<<<<` from being quadratic.
    let close = chars[start + 1..]
        .iter()
        .position(|c| matches!(c, '>' | '<'))?
        + start
        + 1;
    if chars[close] == '<' {
        return None;
    }
    let inner: String = chars[start + 1..close].iter().collect();
    if inner.starts_with("http://") || inner.starts_with("https://") {
        return Some(HtmlTag {
            end: close + 1,
            replacement: inner,
        });
    }
    let name = inner.trim_start_matches('/');
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name: String = name
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    let is_block = HTML_BLOCK_TAGS
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case(&name));
    Some(HtmlTag {
        end: close + 1,
        replacement: if is_block { " ".into() } else { String::new() },
    })
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Builds the final text: paragraphs apart by a blank line, list and code lines together.
#[derive(Default)]
struct TextBlocks {
    out: String,
    mode: Mode,
}

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Start,
    Flowing,
    Lines,
    Break,
}

impl TextBlocks {
    fn flow(&mut self, text: &str) {
        match self.mode {
            Mode::Start => {}
            Mode::Flowing => self.out.push(' '),
            Mode::Lines | Mode::Break => self.out.push_str("\n\n"),
        }
        self.out.push_str(text);
        self.mode = Mode::Flowing;
    }

    fn line(&mut self, text: &str) {
        match self.mode {
            Mode::Start => {}
            Mode::Lines => self.out.push('\n'),
            Mode::Flowing | Mode::Break => self.out.push_str("\n\n"),
        }
        self.out.push_str(text);
        self.mode = Mode::Lines;
    }

    /// A wrapped list item's next line.
    fn continue_line(&mut self, text: &str) {
        self.out.push(' ');
        self.out.push_str(text);
    }

    fn paragraph(&mut self, text: &str) {
        self.blank();
        self.flow(text);
        self.blank();
    }

    fn blank(&mut self) {
        if self.mode != Mode::Start {
            self.mode = Mode::Break;
        }
    }

    fn finish(self) -> String {
        self.out.trim_end().to_string()
    }
}

/// WoWInterface's BBCode to plain, readable paragraphs: tags and images gone, `[*]` to `•`.
pub fn bbcode_to_text(bbcode: &str) -> String {
    let mut out = String::with_capacity(bbcode.len());
    let mut rest = bbcode;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        // The window has to fit a long `[url=...]`, and it bounds the work a text full
        // of unclosed brackets can cause.
        let window = &rest.as_bytes()[..rest.len().min(MAX_BBCODE_TAG_BYTES)];
        let Some(close) = window.iter().position(|byte| *byte == b']') else {
            out.push('[');
            rest = &rest[1..];
            continue;
        };
        let tag = &rest[1..close];
        let after = &rest[close + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(['=', ' '])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "*" | "li" => {
                start_line(&mut out);
                out.push_str("• ");
                rest = after;
            }
            "img" | "video" | "youtube" | "media" | "attach" if !tag.starts_with('/') => {
                let closing = format!("[/{name}]");
                rest =
                    find_ignore_case(after, &closing).map_or("", |at| &after[at + closing.len()..]);
            }
            "list" | "quote" | "code" | "php" | "html" | "center" | "left" | "right" | "indent"
            | "hr" | "table" | "tr" | "td" | "th" | "pre" | "spoiler" => {
                start_line(&mut out);
                rest = after;
            }
            "b" | "i" | "u" | "s" | "strike" | "size" | "color" | "colour" | "font" | "url"
            | "email" | "highlight" | "sup" | "sub" | "noparse" | "align" | "img" | "video"
            | "youtube" | "media" | "attach" => rest = after,
            // Not a tag at all, like `[Interface]` in the middle of a sentence.
            _ => {
                out.push('[');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    tidy_lines(&out)
}

/// Tags break lines, but one that follows a line break shouldn't make a paragraph of it.
fn start_line(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// `needle` must be ASCII, which also makes the result a valid place to slice.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// HTML and entities out, spaces in lines squeezed, at most one blank line in a row.
fn tidy_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = inline_html_only(line);
        let line = decode_entities(&line);
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        // Authors type bullets by hand as often as they use `[*]`.
        let line = match line.strip_prefix("* ") {
            Some(item) => format!("• {item}"),
            None => line,
        };
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        out.push_str(&line);
        blank_run = 0;
    }
    out
}

/// BBCode text often carries stray HTML, but isn't Markdown, so `*` and `_` stay.
fn inline_html_only(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        match (chars[i] == '<').then(|| html_tag(&chars, i)).flatten() {
            Some(tag) => {
                out.push_str(&tag.replacement);
                i = tag.end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Real screenshots from a README, in order, at most three. Badges, donation buttons and
/// SVGs are skipped. Relative paths resolve against `raw_base`, the repo's raw URL.
pub fn image_urls_from_markdown(markdown: &str, raw_base: &str) -> Vec<String> {
    let lowered = markdown.to_ascii_lowercase();
    let next_after = |needle: &str, from: usize| lowered[from..].find(needle).map(|at| at + from);
    let mut next_markdown = next_after("![", 0);
    let mut next_html = next_after("<img", 0);
    let mut images: Vec<String> = Vec::new();
    for _ in 0..MAX_IMAGE_CANDIDATES {
        if images.len() == MAX_IMAGES {
            break;
        }
        let html_is_next = match (next_markdown, next_html) {
            (None, None) => break,
            (Some(md), Some(html)) => html < md,
            (Some(_), None) => false,
            (None, Some(_)) => true,
        };
        let candidate = if html_is_next {
            let at = next_html.unwrap_or_default();
            next_html = next_after("<img", at + 4);
            html_img_src(image_snippet(markdown, at))
        } else {
            let at = next_markdown.unwrap_or_default();
            next_markdown = next_after("![", at + 2);
            markdown_image_url(image_snippet(markdown, at))
        };
        let Some(url) = candidate.filter(|url| !is_badge(url)) else {
            continue;
        };
        let Some(url) = resolve_image(url, raw_base).filter(|url| net::is_screenshot_url(url))
        else {
            continue;
        };
        if !images.contains(&url) {
            images.push(url);
        }
    }
    images
}

/// Enough text after `at` to hold one image's markup, cut on a character boundary.
fn image_snippet(text: &str, at: usize) -> &str {
    let mut end = text.len().min(at + MAX_IMAGE_MARKUP_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[at..end]
}

/// From `![alt](url "title")`, the url.
fn markdown_image_url(text: &str) -> Option<&str> {
    let chars: Vec<char> = text.chars().collect();
    let pairs = bracket_pairs(&chars);
    let (label_end, _) = link_end(&chars, &pairs, 1)?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let url_start: usize = chars[..label_end + 2].iter().map(|c| c.len_utf8()).sum();
    let rest = text[url_start..].trim_start();
    let url = if let Some(bracketed) = rest.strip_prefix('<') {
        bracketed.split('>').next()?
    } else {
        rest.split([' ', '\t', ')']).next()?
    };
    Some(url)
}

/// From `<img ... src="url" ...>`, the url, quoted either way.
fn html_img_src(text: &str) -> Option<&str> {
    let tag = &text[..text.find('>').unwrap_or(text.len())];
    let lowered = tag.to_ascii_lowercase();
    let at = lowered.find("src=")? + 4;
    let value = &tag[at..];
    let quote = value.chars().next()?;
    if quote == '"' || quote == '\'' {
        value[1..].split(quote).next()
    } else {
        value.split([' ', '\t']).next()
    }
}

fn is_badge(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    let path = lowered.split(['?', '#']).next().unwrap_or(&lowered);
    path.ends_with(".svg") || BADGE_MARKERS.iter().any(|marker| lowered.contains(marker))
}

fn resolve_image(url: &str, raw_base: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('#') {
        return None;
    }
    if let Some(absolute) = absolute_https(url) {
        return Some(absolute);
    }
    // A scheme we don't fetch: `data:`, `mailto:` and friends.
    if url
        .split_once(':')
        .is_some_and(|(scheme, _)| !scheme.contains('/'))
    {
        return None;
    }
    let mut segments = Vec::new();
    for segment in url.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    (!segments.is_empty())
        .then(|| format!("{}/{}", raw_base.trim_end_matches('/'), segments.join("/")))
}

/// An absolute image URL in the form [`Http`] can fetch: `http` upgraded to `https`, and
/// a GitHub page link turned into the file itself. `None` if it isn't absolute.
fn absolute_https(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))?;
    if let Some(path) = rest.strip_prefix("github.com/") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if let [owner, repo, "blob" | "raw", tail] = parts.as_slice() {
            let file = tail.split('?').next().unwrap_or(tail);
            return Some(format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{file}"
            ));
        }
    }
    Some(format!("https://{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::test_server::{Server, respond, serve};

    fn endpoints(server: &Server) -> Endpoints<'_> {
        Endpoints {
            github_raw: &server.base,
            github_api: &server.base,
            wowi_api: &server.base,
        }
    }

    fn addon(source: Source) -> Addon {
        Addon {
            summary: "The catalog's summary.".into(),
            source,
            ..Addon::default()
        }
    }

    fn github() -> Addon {
        addon(Source::Github {
            repo: "owner/repo".into(),
        })
    }

    #[test]
    fn github_details_come_from_readme_and_changelog() {
        let server = serve(|request, stream| {
            match request.path.as_str() {
            "/owner/repo/HEAD/README.md" => respond(
                stream,
                "200 OK",
                &[],
                b"# Title\n\n[![Build](https://img.shields.io/x.svg)](https://ci)\n\nHello **world**.\n\n![shot](https://i.imgur.com/shot.png)\n",
            ),
            "/owner/repo/HEAD/CHANGELOG.md" => respond(stream, "200 OK", &[], b"## 1.0\n- First\n"),
            other => panic!("unexpected request for {other}"),
        }
        });
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert_eq!(details.about, "Title\n\nHello world.");
        assert_eq!(details.changelog.as_deref(), Some("1.0\n\n• First"));
        assert_eq!(details.images, ["https://i.imgur.com/shot.png"]);
    }

    #[test]
    fn readme_fallbacks_and_release_body_are_tried_in_order() {
        let server = serve(|request, stream| match request.path.as_str() {
            "/owner/repo/HEAD/README" => respond(stream, "200 OK", &[], b"Plain readme."),
            "/repos/owner/repo/releases/latest" => {
                respond(
                    stream,
                    "200 OK",
                    &[],
                    br#"{"tag_name":"v1","body":"Fixed **things**."}"#,
                );
            }
            _ => respond(stream, "404 Not Found", &[], b""),
        });
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert_eq!(details.about, "Plain readme.");
        assert_eq!(details.changelog.as_deref(), Some("Fixed things."));
    }

    #[test]
    fn a_rate_limited_api_does_not_fail_the_whole_fetch() {
        let server = serve(|request, stream| match request.path.as_str() {
            "/owner/repo/HEAD/README.md" => respond(stream, "200 OK", &[], b"About."),
            "/repos/owner/repo/releases/latest" => {
                respond(stream, "403 rate limit exceeded", &[], b"{}")
            }
            _ => respond(stream, "404 Not Found", &[], b""),
        });
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert_eq!(details.about, "About.");
        assert_eq!(details.changelog, None);
    }

    #[test]
    fn a_broken_changelog_does_not_fail_the_whole_fetch() {
        let server = serve(|request, stream| match request.path.as_str() {
            "/owner/repo/HEAD/README.md" => respond(stream, "200 OK", &[], b"About."),
            "/repos/owner/repo/releases/latest" => respond(stream, "200 OK", &[], b"not json"),
            "/owner/repo/HEAD/CHANGELOG.md" => respond(stream, "500 Oops", &[], b""),
            _ => respond(stream, "404 Not Found", &[], b""),
        });
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert_eq!(details.about, "About.");
        assert_eq!(details.changelog, None);
    }

    #[test]
    fn no_readme_falls_back_to_the_catalog_summary() {
        let server = serve(|_, stream| respond(stream, "404 Not Found", &[], b""));
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert_eq!(details.about, "The catalog's summary.");
        assert!(details.images.is_empty());
        assert_eq!(details.changelog, None);
    }

    #[test]
    fn a_server_error_on_the_readme_is_reported() {
        let server = serve(|_, stream| respond(stream, "500 Oops", &[], b""));
        let result = fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server));
        assert!(matches!(result, Err(Error::HttpStatus { status: 500, .. })));
    }

    #[test]
    fn huge_files_are_capped_at_download_and_text_size() {
        let body = "word ".repeat(400_000);
        assert!(body.len() as u64 > MAX_DOWNLOAD_BYTES);
        let server = serve(move |request, stream| {
            if request.path.ends_with("README.md") {
                respond(stream, "200 OK", &[], body.as_bytes());
            } else {
                respond(stream, "404 Not Found", &[], b"");
            }
        });
        let details =
            fetch_from(&Http::allowing_loopback(), &github(), &endpoints(&server)).unwrap();
        assert!(details.about.chars().count() <= MAX_TEXT_CHARS + 1);
        assert!(details.about.ends_with('…'));
    }

    #[test]
    fn an_odd_repo_name_never_reaches_a_url() {
        let server = serve(|_, _| panic!("no request should be made"));
        for repo in [
            "../../etc",
            "owner",
            "a/b/c",
            "owner/../x",
            "own er/x",
            "",
            "/x",
            "a/..",
        ] {
            let odd = addon(Source::Github { repo: repo.into() });
            let details =
                fetch_from(&Http::allowing_loopback(), &odd, &endpoints(&server)).unwrap();
            assert_eq!(details.about, "The catalog's summary.", "{repo}");
        }
        assert_eq!(server.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn direct_sources_use_the_summary_without_touching_the_network() {
        let server = serve(|_, _| panic!("no request should be made"));
        let direct = addon(Source::Direct {
            url: "https://example.test/a.zip".into(),
        });
        let details = fetch_from(&Http::allowing_loopback(), &direct, &endpoints(&server)).unwrap();
        assert_eq!(details.about, "The catalog's summary.");
    }

    #[test]
    fn wowinterface_details_come_from_filedetails() {
        let server = serve(|request, stream| {
            assert_eq!(request.path, "/5086.json");
            respond(
                stream,
                "200 OK",
                &[],
                br#"[{"UID":"5086","UIName":"Big","UIDescription":"[b]Bold[/b] intro\r\n[list][*]One[*]Two[/list][img]https://x/y.png[/img]",
                     "UIChangeLog":"[size=4]v1[/size]\r\nFixed."}]"#,
            );
        });
        let big = Addon {
            images: vec![
                "https://i.imgur.com/1.png".into(),
                "http://i.imgur.com/2.png".into(),
                "https://i.imgur.com/3.png".into(),
                "https://i.imgur.com/4.png".into(),
            ],
            ..addon(Source::Wowi { id: 5086 })
        };
        let details = fetch_from(&Http::allowing_loopback(), &big, &endpoints(&server)).unwrap();
        assert_eq!(details.about, "Bold intro\n• One\n• Two");
        assert_eq!(details.changelog.as_deref(), Some("v1\nFixed."));
        assert_eq!(
            details.images,
            [
                "https://i.imgur.com/1.png",
                "https://i.imgur.com/2.png",
                "https://i.imgur.com/3.png"
            ]
        );
    }

    #[test]
    fn wowinterface_copes_with_missing_fields_and_a_bare_object() {
        let server = serve(|_, stream| {
            respond(
                stream,
                "200 OK",
                &[],
                br#"{"UIDescription":"Just this.","UIIMGs":null}"#,
            )
        });
        let details = fetch_from(
            &Http::allowing_loopback(),
            &addon(Source::Wowi { id: 1 }),
            &endpoints(&server),
        )
        .unwrap();
        assert_eq!(details.about, "Just this.");
        assert_eq!(details.changelog, None);
        assert!(details.images.is_empty());
    }

    #[test]
    fn wowinterface_says_when_it_holds_an_upload_for_approval() {
        // TomTom's details while v4.4.1 waited, trimmed. The flag is a string, the date a
        // number of milliseconds.
        let server = serve(|request, stream| {
            let body: &[u8] = match request.path.as_str() {
                "/7032.json" => {
                    br#"[{"UID":"7032","UIVersion":"v4.4.1-release","UIDate":1794062518000,"UIPending":"1","UIName":"TomTom"}]"#
                }
                "/5108.json" => {
                    br#"[{"UID":"5108","UIVersion":"v5.1.0-release","UIDate":1793564625000,"UIPending":"0"}]"#
                }
                "/1.json" => br#"{"UIVersion":12,"UIDate":"1794062518000","UIPending":1}"#,
                "/2.json" => br#"{"UIPending":true}"#,
                "/3.json" => br#"[{"UIVersion":"1.0"}]"#,
                _ => br"[]",
            };
            respond(stream, "200 OK", &[], body);
        });
        let held = |id| wowi_held_upload(&Http::allowing_loopback(), &server.base, id).unwrap();
        assert_eq!(
            held(7032),
            Some(HeldUpload {
                version: "v4.4.1-release".into(),
                uploaded: 1_794_062_518,
            })
        );
        assert_eq!(held(5108), None);
        assert_eq!(
            held(1),
            Some(HeldUpload {
                version: "12".into(),
                uploaded: 1_794_062_518,
            })
        );
        assert_eq!(
            held(2),
            Some(HeldUpload {
                version: String::new(),
                uploaded: 0,
            })
        );
        assert_eq!(held(3), None, "no flag means nothing is held");
        assert_eq!(held(4), None, "an empty answer means nothing is held");
    }

    #[test]
    fn wowinterface_held_upload_nonsense_is_an_error() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"<html>maintenance</html>"));
        let result = wowi_held_upload(&Http::allowing_loopback(), &server.base, 7032);
        assert!(matches!(result, Err(Error::Parse { .. })));
    }

    #[test]
    fn wowinterface_nonsense_is_a_parse_error() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"<html>maintenance</html>"));
        let result = fetch_from(
            &Http::allowing_loopback(),
            &addon(Source::Wowi { id: 1 }),
            &endpoints(&server),
        );
        assert!(matches!(result, Err(Error::Parse { .. })));
    }

    #[test]
    fn markdown_headings_and_paragraphs() {
        assert_eq!(
            markdown_to_text("# Top\nText\n## Sub ##\nMore"),
            "Top\n\nText\n\nSub\n\nMore"
        );
        assert_eq!(
            markdown_to_text("Setext\n===\nBody\n---\nTail"),
            "Setext\n\nBody\n\nTail"
        );
        assert_eq!(markdown_to_text("one\ntwo\n\nthree"), "one two\n\nthree");
        assert_eq!(
            markdown_to_text("#hashtag not heading"),
            "#hashtag not heading"
        );
        assert_eq!(markdown_to_text("  \n\n# A\n\n\n\nB\n\n"), "A\n\nB");
    }

    #[test]
    fn markdown_badges_and_images_vanish() {
        let md = "[![Build](https://img.shields.io/b.svg)](https://ci.example) ![alt](x.png) Words ![a][ref]\n\n[ref]: x.png\n";
        assert_eq!(markdown_to_text(md), "Words");
        assert_eq!(markdown_to_text("[![a](b)](c)\n[![d](e)](f)\n"), "");
    }

    #[test]
    fn markdown_links_keep_their_text() {
        assert_eq!(
            markdown_to_text("See [the docs](https://x.example/a(b)) now."),
            "See the docs now."
        );
        assert_eq!(
            markdown_to_text("See [the docs][1].\n\n[1]: https://x.example"),
            "See the docs."
        );
        assert_eq!(
            markdown_to_text("Auto <https://x.example/y> link"),
            "Auto https://x.example/y link"
        );
        assert_eq!(markdown_to_text("Not [a link] here"), "Not [a link] here");
    }

    #[test]
    fn markdown_emphasis_goes_but_names_and_math_stay() {
        assert_eq!(
            markdown_to_text("**bold** *it* __bold__ _it_ ~~gone~~"),
            "bold it bold it gone"
        );
        assert_eq!(
            markdown_to_text("call some_function_name and 2 * 3"),
            "call some_function_name and 2 * 3"
        );
        assert_eq!(markdown_to_text("***both***"), "both");
        assert_eq!(
            markdown_to_text(r"escaped \*stars\* and \_lines\_"),
            "escaped *stars* and _lines_"
        );
    }

    #[test]
    fn markdown_inline_code_is_kept_verbatim() {
        assert_eq!(
            markdown_to_text("Use `/reload` or ``a`b``, and `**raw**`."),
            "Use /reload or a`b, and **raw**."
        );
        assert_eq!(markdown_to_text("Stray ` tick"), "Stray ` tick");
    }

    #[test]
    fn markdown_code_blocks_stay_as_lines() {
        assert_eq!(
            markdown_to_text("Run:\n\n```lua\nlocal x = **1**\n  indented()\n```\nDone"),
            "Run:\n\nlocal x = **1**\n  indented()\n\nDone"
        );
        assert_eq!(
            markdown_to_text("~~~\n# not a heading\n~~~"),
            "# not a heading"
        );
    }

    #[test]
    fn markdown_html_is_removed() {
        assert_eq!(
            markdown_to_text(
                "<p align=\"center\">\n<img src=\"x.png\" width=\"1\">\n</p>\n\nHello <b>you</b>.<br>Next"
            ),
            "Hello you. Next"
        );
        assert_eq!(
            markdown_to_text("<!-- hidden\nmore -->\nVisible<!-- also hidden -->."),
            "Visible."
        );
        assert_eq!(
            markdown_to_text("<img\n  src=\"x.png\"\n  alt=\"y\">\nAfter"),
            "After"
        );
        assert_eq!(markdown_to_text("if a < b and c > d"), "if a < b and c > d");
        assert_eq!(
            markdown_to_text("<details><summary>More</summary>\nInside\n</details>"),
            "More Inside"
        );
        assert_eq!(
            markdown_to_text("x &lt;b&gt; &amp; y&nbsp;z"),
            "x <b> & y z"
        );
    }

    #[test]
    fn markdown_table_markup_is_removed() {
        assert_eq!(
            markdown_to_text("| Name | Use |\n|---|:--:|\n| *Foo* | Bars |\n\nAfter"),
            "Name - Use\nFoo - Bars\n\nAfter"
        );
        assert_eq!(markdown_to_text("a | b\n---|---\nc | d"), "a | b c | d");
    }

    #[test]
    fn markdown_lists_quotes_and_rules() {
        assert_eq!(
            markdown_to_text(
                "Intro\n- one\n* two\n+ three\n  wrapped\n1. first\n2) second\n- [x] done\n- [ ] todo\n\nEnd"
            ),
            "Intro\n\n• one\n• two\n• three wrapped\n1. first\n2. second\n• done\n• todo\n\nEnd"
        );
        assert_eq!(
            markdown_to_text("> quoted\n> more\n\nplain"),
            "quoted more\n\nplain"
        );
        assert_eq!(
            markdown_to_text("a\n\n---\n\nb\n\n***\n\nc\n\n___\n\nd"),
            "a\n\nb\n\nc\n\nd"
        );
        assert_eq!(
            markdown_to_text("-not a list\n*nor this"),
            "-not a list nor this"
        );
    }

    #[test]
    fn markdown_handles_multibyte_text_and_oddities_without_panicking() {
        assert_eq!(
            markdown_to_text("# Ünïcode — ✓\n\n日本語 **太字**"),
            "Ünïcode — ✓\n\n日本語 太字"
        );
        for nasty in [
            "[", "![", "[]", "[](", "![](", "<", "<<<", "`", "``", "*", "_", "\\", "#", "|",
            "<!--", "[a](", "[a][", "]", "][",
        ] {
            let _ = markdown_to_text(nasty);
            let _ = bbcode_to_text(nasty);
            let _ = image_urls_from_markdown(nasty, "https://raw.githubusercontent.com/a/b/HEAD");
        }
    }

    #[test]
    fn bbcode_tags_go_and_text_stays() {
        assert_eq!(
            bbcode_to_text(
                "[b]Bold[/b] [i]it[/i] [u]un[/u] [size=5]big[/size] [color=#ff0000]red[/color] [FONT=Arial]font[/FONT]"
            ),
            "Bold it un big red font"
        );
        assert_eq!(
            bbcode_to_text("[url=https://x.example]the site[/url]"),
            "the site"
        );
        assert_eq!(
            bbcode_to_text("[url]https://x.example[/url]"),
            "https://x.example"
        );
        assert_eq!(bbcode_to_text("[email]a@b.example[/email]"), "a@b.example");
    }

    #[test]
    fn bbcode_images_and_videos_disappear_with_their_content() {
        assert_eq!(bbcode_to_text("A[img]https://x/y.png[/img]B"), "AB");
        assert_eq!(bbcode_to_text("A[IMG=100x100]https://x/y.png[/IMG]B"), "AB");
        assert_eq!(bbcode_to_text("A[youtube]abc123[/youtube]B"), "AB");
        assert_eq!(bbcode_to_text("A[video=youtube]abc[/video]B"), "AB");
        assert_eq!(bbcode_to_text("A[img]never closed"), "A");
    }

    #[test]
    fn bbcode_lists_and_blocks_break_lines() {
        assert_eq!(
            bbcode_to_text("Intro[list][*]One[*]Two[/list]Outro"),
            "Intro\n• One\n• Two\nOutro"
        );
        assert_eq!(
            bbcode_to_text("[quote]Said[/quote]Then[hr]More"),
            "Said\nThen\nMore"
        );
        assert_eq!(bbcode_to_text("[center]Mid[/center]"), "Mid");
        assert_eq!(bbcode_to_text("[code]x = 1[/code]"), "x = 1");
    }

    #[test]
    fn bbcode_paragraphs_survive_and_extra_blank_lines_do_not() {
        assert_eq!(
            bbcode_to_text("One\r\nTwo\r\n\r\n\r\n\r\nThree"),
            "One\nTwo\n\nThree"
        );
        assert_eq!(bbcode_to_text("  spaced    out  "), "spaced out");
    }

    #[test]
    fn bbcode_keeps_things_that_are_not_tags() {
        assert_eq!(
            bbcode_to_text("Type [Interface] to open"),
            "Type [Interface] to open"
        );
        assert_eq!(
            bbcode_to_text("Odd ] and [ brackets"),
            "Odd ] and [ brackets"
        );
        assert_eq!(bbcode_to_text("1 * 2 _ 3"), "1 * 2 _ 3");
    }

    #[test]
    fn bbcode_stray_html_and_entities_are_cleaned() {
        assert_eq!(
            bbcode_to_text("Line<br />Next &amp; last &quot;x&quot;"),
            "Line Next & last \"x\""
        );
    }

    #[test]
    fn bbcode_copes_with_what_real_descriptions_contain() {
        // From a real WoWInterface description: long links, and bullets typed by hand.
        let link = format!(
            "https://www.wowinterface.com/downloads/info9577-{}.html",
            "x".repeat(100)
        );
        assert_eq!(
            bbcode_to_text(&format!("* [url={link}]LittleWigs[/url] is a plugin")),
            "• LittleWigs is a plugin"
        );
        assert_eq!(
            bbcode_to_text("[note [b]bold[/b] inside]"),
            "[note bold inside]"
        );
        assert_eq!(bbcode_to_text("* * *"), "• * *");
    }

    #[test]
    fn hostile_text_cannot_make_the_parsers_crawl_or_overflow_the_stack() {
        let size = MAX_DOWNLOAD_BYTES as usize;
        let cases = [
            "[".repeat(size),
            "]".repeat(size),
            "[a](".repeat(size / 4),
            "![".repeat(size / 2),
            "<".repeat(size),
            "<a ".repeat(size / 3),
            "<img ".repeat(size / 5),
            "`".repeat(size),
            "` ``".repeat(size / 4),
            "[img]x[/img]".repeat(size / 12),
            "[url=".repeat(size / 5),
            "*_".repeat(size / 2),
            format!("{}x{}", "[".repeat(5000), "](y)".repeat(5000)),
            "> ".repeat(size / 2),
            "# ".repeat(size / 2),
            "\n".repeat(size),
            "<!--".repeat(size / 4),
        ];
        for case in &cases {
            let started = std::time::Instant::now();
            let _ = markdown_to_text(case);
            let markdown = started.elapsed();
            let _ = bbcode_to_text(case);
            let bbcode = started.elapsed() - markdown;
            let _ = image_urls_from_markdown(case, "https://raw.githubusercontent.com/a/b/HEAD");
            let images = started.elapsed() - markdown - bbcode;
            let limit = std::time::Duration::from_secs(10);
            let shown: String = case.chars().take(12).collect();
            println!("{shown:?}: markdown {markdown:?}, bbcode {bbcode:?}, images {images:?}");
            assert!(
                markdown < limit && bbcode < limit && images < limit,
                "{shown:?}"
            );
        }
    }

    #[test]
    fn markdown_keeps_prose_that_looks_a_bit_like_a_tag() {
        assert_eq!(
            markdown_to_text("if x <y then\nthis stays"),
            "if x <y then this stays"
        );
        assert_eq!(
            markdown_to_text("<img src=\"x\"\nnever closed\n\nAfter"),
            ""
        );
        let long_open = format!("<img src=\"x\"\n{}\nStill here", "line\n".repeat(20));
        assert!(markdown_to_text(&long_open).ends_with("Still here"));
    }

    #[test]
    fn github_callouts_lose_their_marker_and_keep_their_words() {
        assert_eq!(markdown_to_text("> [!NOTE]\n> Read this."), "Read this.");
    }

    #[test]
    fn inline_code_matches_whole_runs_of_backticks() {
        assert_eq!(markdown_to_text("`a``b` c"), "a``b c");
    }

    #[test]
    fn image_urls_are_real_screenshots_only() {
        let md = r#"
[![Build](https://img.shields.io/github/actions/workflow/status/a/b/ci.yml)](https://ci)
![badge](https://github.com/a/b/actions/workflows/ci.yml/badge.svg)
![Donate](https://www.paypal.com/en_US/i/btn/btn_donate.gif)
![Logo](logo.svg)
![Ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)
![Shot one](docs/images/one.png "A title")
<img src="./docs/two.jpg" width="400">
<IMG SRC='https://i.imgur.com/three.png'>
![Too many](https://i.imgur.com/four.png)
"#;
        let raw = "https://raw.githubusercontent.com/a/b/HEAD";
        assert_eq!(
            image_urls_from_markdown(md, raw),
            [
                "https://raw.githubusercontent.com/a/b/HEAD/docs/images/one.png",
                "https://raw.githubusercontent.com/a/b/HEAD/docs/two.jpg",
                "https://i.imgur.com/three.png",
            ]
        );
    }

    #[test]
    fn image_paths_resolve_sensibly() {
        let raw = "https://raw.githubusercontent.com/a/b/HEAD/";
        let one = |url: &str| image_urls_from_markdown(&format!("![x]({url})"), raw);
        assert_eq!(
            one("x.png"),
            ["https://raw.githubusercontent.com/a/b/HEAD/x.png"]
        );
        assert_eq!(
            one("./a/x.png"),
            ["https://raw.githubusercontent.com/a/b/HEAD/a/x.png"]
        );
        assert_eq!(
            one("/a/x.png"),
            ["https://raw.githubusercontent.com/a/b/HEAD/a/x.png"]
        );
        assert_eq!(
            one("a/../b/x.png"),
            ["https://raw.githubusercontent.com/a/b/HEAD/b/x.png"]
        );
        assert_eq!(
            one("<a b/x y.png>"),
            ["https://raw.githubusercontent.com/a/b/HEAD/a b/x y.png"]
        );
        assert_eq!(
            one("http://i.imgur.com/x.png"),
            ["https://i.imgur.com/x.png"]
        );
        assert_eq!(one("//i.imgur.com/x.png"), ["https://i.imgur.com/x.png"]);
        assert_eq!(
            one("https://github.com/o/r/blob/main/img/x.png?raw=true"),
            ["https://raw.githubusercontent.com/o/r/main/img/x.png"]
        );
        assert_eq!(
            one("https://github.com/o/r/raw/main/x.png"),
            ["https://raw.githubusercontent.com/o/r/main/x.png"]
        );
        assert_eq!(
            one("https://github.com/user-attachments/assets/1234"),
            ["https://github.com/user-attachments/assets/1234"]
        );
        for skipped in [
            "data:image/png;base64,AAAA",
            "mailto:x@y.z",
            "javascript:alert(1)",
            "#anchor",
            "",
            ".",
        ] {
            assert!(one(skipped).is_empty(), "{skipped}");
        }
    }

    #[test]
    fn a_readme_cannot_point_the_app_at_the_players_own_network() {
        for url in [
            "http://192.168.1.1/admin.png",
            "https://127.0.0.1/x.png",
            "https://localhost/x.png",
            "http://localhost:8080/x.png",
            "https://[::1]/x.png",
            "https://printer.local/x.png",
            "https://me@10.0.0.1:444/x.png",
            "https://router/x.png",
            "https://2130706433/x.png",
            "https://tracker.example.com/pixel.png",
        ] {
            assert!(
                image_urls_from_markdown(
                    &format!("![x]({url})"),
                    "https://raw.githubusercontent.com/a/b/HEAD"
                )
                .is_empty(),
                "{url}"
            );
        }
        let fine = image_urls_from_markdown(
            "![x](https://i.imgur.com/a.png)",
            "https://raw.githubusercontent.com/a/b/HEAD",
        );
        assert_eq!(fine, ["https://i.imgur.com/a.png"]);
    }

    #[test]
    fn a_wowi_page_cannot_point_the_app_at_the_players_own_network() {
        let server = serve(|_, stream| {
            respond(stream, "200 OK", &[], br#"[{"UIDescription":"x"}]"#);
        });
        let page = Addon {
            images: vec![
                "https://127.0.0.1/a.png".into(),
                "http://192.168.1.1/b.png".into(),
                "//localhost/c.png".into(),
                "https://router/d.png".into(),
                "https://i.imgur.com/fine.png".into(),
            ],
            ..addon(Source::Wowi { id: 1 })
        };
        let details = fetch_from(&Http::allowing_loopback(), &page, &endpoints(&server)).unwrap();
        assert_eq!(details.images, ["https://i.imgur.com/fine.png"]);
    }

    #[test]
    fn a_repo_with_badge_in_its_name_keeps_its_screenshots() {
        let raw = "https://raw.githubusercontent.com/a/badge-addon/HEAD";
        assert_eq!(
            image_urls_from_markdown("![x](docs/shot.png)", raw),
            ["https://raw.githubusercontent.com/a/badge-addon/HEAD/docs/shot.png"]
        );
    }

    #[test]
    fn repeated_images_count_once() {
        let md = "![a](x.png) ![b](x.png) ![c](y.png)";
        assert_eq!(
            image_urls_from_markdown(md, "https://raw.githubusercontent.com/a/b/HEAD").len(),
            2
        );
    }

    #[test]
    fn long_text_is_cut_at_a_sensible_place() {
        let short = "short text";
        assert_eq!(cap_text(short), short);

        let exact = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(cap_text(&exact), exact);

        let paragraphs = format!(
            "{}\n\n{}",
            "a".repeat(MAX_TEXT_CHARS - 100),
            "b".repeat(500)
        );
        assert_eq!(
            cap_text(&paragraphs),
            format!("{}…", "a".repeat(MAX_TEXT_CHARS - 100))
        );

        let words = "word ".repeat(2000);
        let capped = cap_text(&words);
        assert!(
            capped.ends_with("word…"),
            "{}",
            &capped[capped.len() - 20..]
        );
        assert!(capped.chars().count() <= MAX_TEXT_CHARS + 1);

        let no_spaces = "é".repeat(MAX_TEXT_CHARS + 50);
        assert_eq!(cap_text(&no_spaces).chars().count(), MAX_TEXT_CHARS + 1);
    }

    #[test]
    #[ignore = "reaches the real internet; run by hand with --ignored"]
    fn live_details_for_real_addons() {
        let http = Http::new();
        let github = addon(Source::Github {
            repo: "BigWigsMods/BigWigs".into(),
        });
        let details = fetch(&http, &github).unwrap();
        println!(
            "GITHUB about:\n{}\n\nchangelog: {:?}\nimages: {:?}",
            details.about, details.changelog, details.images
        );
        assert!(!details.about.is_empty());

        let wowi = addon(Source::Wowi { id: 5086 });
        let details = fetch(&http, &wowi).unwrap();
        println!(
            "WOWI about:\n{}\n\nchangelog: {:?}\nimages: {:?}",
            details.about, details.changelog, details.images
        );
        assert!(!details.about.is_empty());
    }
}
