//! HTTP for everything the app fetches: catalog with ETags, addon zips, update files.
//!
//! The app only ever talks to the public internet over https. Redirects are followed by
//! hand so every hop goes through the same URL check, and a URL that a stranger wrote
//! (a README's image, say) also goes through a resolver that refuses private addresses,
//! because a name can point anywhere.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ureq::config::Config;
use ureq::http::header::{ETAG, IF_NONE_MATCH, LOCATION};
use ureq::http::{HeaderValue, Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, Body, Timeout};

use crate::error::{Error, Result};
use crate::install::rename_with_retry;

/// Honest about who we are, because several of the sites we read ask for that.
pub const USER_AGENT: &str = concat!(
    "WoW-is-a-Feeling/",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    " (+https://github.com/robbiemueller/wow-is-a-feeling)"
);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Waits between the three attempts a download gets to connect.
const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(300), Duration::from_millis(900)];
const BUFFER_SIZE: usize = 64 * 1024;
/// Names that only mean something on the player's own network. Windows answers the last
/// two by itself, without asking DNS.
const INTERNAL_SUFFIXES: [&str; 11] = [
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".corp",
    ".home.arpa",
    ".ipv6-literal.net",
    ".mshome.net",
];

/// How far a long-running operation has got, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    /// `None` when the server didn't say how much is coming.
    pub total: Option<u64>,
}

/// The result of a conditional GET.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
}

/// Which addresses a client may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// https to public names only. What the app runs with.
    Public,
    /// The same, plus plain http to this machine, so a test can serve files locally.
    PublicAndLoopback,
}

/// A shared HTTP client. Cheap to clone; clones share their connection pools.
#[derive(Clone)]
pub struct Http {
    agent: Agent,
    /// For URLs a stranger wrote. Its own pool, so it never reuses a checked connection.
    stranger_agent: Agent,
    reach: Reach,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    /// The client the app uses: https to public names only, on every hop of a redirect.
    /// A public name can still point anywhere, and then the certificate check stops anything
    /// being sent. Links a stranger wrote get the stronger [`Http::get_untrusted`].
    pub fn new() -> Self {
        Self::with_reach(Reach::Public)
    }

    /// Also allows plain http to this machine. For tests that serve files locally; a
    /// normal launch never uses it.
    pub fn allowing_loopback() -> Self {
        Self::with_reach(Reach::PublicAndLoopback)
    }

    fn with_reach(reach: Reach) -> Self {
        Http {
            agent: client_config(RootCerts::PlatformVerifier).new_agent(),
            stranger_agent: Agent::with_parts(
                // The OS's certificate checker can fetch a URL named inside a certificate
                // during the handshake, outside every rule in this file. So a stranger's
                // server gets the built-in root list, and at worst a screenshot is lost.
                client_config(RootCerts::WebPki),
                DefaultConnector::default(),
                PublicOnlyResolver::default(),
            ),
            reach,
        }
    }

    /// Fetch a URL into memory. Anything over `max_bytes` is an error.
    pub fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let opened = self.open(url, None)?;
        read_limited(opened, max_bytes)
    }

    /// Fetch a URL a stranger wrote, such as an image in a README or on an addon's page.
    /// Beyond the usual rules, a name that resolves to this machine or the local network
    /// is refused at connect time, on every hop, and no client relaxes that.
    pub fn get_untrusted(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let start = Target::parse(url, Reach::Public)?;
        let opened = follow_redirects(&self.stranger_agent, start, None, Reach::Public)
            .map_err(|failure| failure.error)?;
        read_limited(opened, max_bytes)
    }

    /// Fetch a URL unless it still matches `etag`, so an unchanged file costs one 304.
    pub fn get_if_modified(
        &self,
        url: &str,
        etag: Option<&str>,
        max_bytes: u64,
    ) -> Result<Fetched> {
        let opened = self.open(url, etag)?;
        if opened.response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        let etag = opened
            .response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(Fetched::Body {
            bytes: read_limited(opened, max_bytes)?,
            etag,
        })
    }

    /// Fetch the first `max_bytes` of a URL and ignore the rest. For text that gets
    /// trimmed to a panel's size anyway, where a huge file isn't worth failing over.
    pub(crate) fn get_prefix(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let opened = self.open(url, None)?;
        let host = opened.host;
        let mut bytes = Vec::new();
        opened
            .response
            .into_body()
            .into_reader()
            .take(max_bytes)
            .read_to_end(&mut bytes)
            .map_err(|e| body_error(e, &host))?;
        Ok(bytes)
    }

    /// Stream a URL to `dest`. Bytes land in `dest` + `.part`, get synced to disk,
    /// and only then take `dest`'s name, so `dest` is either complete or untouched.
    /// Returns the number of bytes written.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
        max_bytes: u64,
    ) -> Result<u64> {
        let part = part_path(dest);
        let finished = self
            .stream_to_file(url, &part, progress, cancel, max_bytes)
            .and_then(|size| {
                rename_with_retry(&part, dest).map_err(|e| Error::io(dest, e))?;
                Ok(size)
            });
        if finished.is_err() {
            let _ = fs::remove_file(&part);
        }
        finished
    }

    fn stream_to_file(
        &self,
        url: &str,
        part: &Path,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
        max_bytes: u64,
    ) -> Result<u64> {
        let opened = self.open_with_retries(url, cancel)?;
        let host = opened.host;
        let total = opened.response.body().content_length();
        if total.is_some_and(|size| size > max_bytes) {
            return Err(too_big(&host, max_bytes));
        }
        let mut reader = opened.response.into_body().into_reader();
        let mut file = File::create(part).map_err(|e| Error::io(part, e))?;
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut done = 0u64;
        let mut last_report = Instant::now();
        progress(Progress { done, total });
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(body_error(e, &host)),
            };
            done += count as u64;
            if done > max_bytes {
                return Err(too_big(&host, max_bytes));
            }
            file.write_all(&buffer[..count])
                .map_err(|e| Error::io(part, e))?;
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                progress(Progress { done, total });
                last_report = Instant::now();
            }
        }
        file.sync_all().map_err(|e| Error::io(part, e))?;
        progress(Progress { done, total });
        Ok(done)
    }

    fn open(&self, url: &str, etag: Option<&str>) -> Result<Opened> {
        self.open_once(url, etag).map_err(|failure| failure.error)
    }

    fn open_with_retries(&self, url: &str, cancel: &AtomicBool) -> Result<Opened> {
        let mut delays = CONNECT_RETRY_DELAYS.iter();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let failure = match self.open_once(url, None) {
                Ok(opened) => return Ok(opened),
                Err(failure) => failure,
            };
            match delays.next() {
                Some(delay) if failure.can_retry => sleep_unless_cancelled(*delay, cancel)?,
                _ => return Err(failure.error),
            }
        }
    }

    fn open_once(&self, url: &str, etag: Option<&str>) -> std::result::Result<Opened, OpenFailure> {
        let start = Target::parse(url, self.reach).map_err(OpenFailure::fatal)?;
        follow_redirects(&self.agent, start, etag, self.reach)
    }
}

fn client_config(roots: RootCerts) -> Config {
    let tls = TlsConfig::builder().root_certs(roots).build();
    Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_per_read(Some(IO_TIMEOUT))
        .timeout_per_write(Some(IO_TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
}

/// Request `start` and follow its redirects, holding every hop to `reach`.
fn follow_redirects(
    agent: &Agent,
    start: Target,
    etag: Option<&str>,
    reach: Reach,
) -> std::result::Result<Opened, OpenFailure> {
    let mut target = start;
    for _ in 0..=MAX_REDIRECTS {
        let response = request(agent, &target, etag).map_err(|e| OpenFailure {
            can_retry: is_connect_failure(&e),
            error: offline(&target.host),
        })?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NOT_MODIFIED {
            return Ok(Opened {
                host: target.host,
                response,
            });
        }
        // Not `to_str`, which refuses the raw UTF-8 some servers send here. `escape_url`
        // handles it, and `Target::parse` still checks the result.
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok());
        match location {
            Some(location) if status.is_redirection() => {
                let next = join_location(&target.url, location);
                target = Target::parse(&next, reach).map_err(OpenFailure::fatal)?;
            }
            _ => {
                return Err(OpenFailure::fatal(Error::HttpStatus {
                    host: target.host,
                    status: status.as_u16(),
                }));
            }
        }
    }
    Err(OpenFailure::fatal(Error::Other(format!(
        "{} sent us through too many redirects. Try again in a little while.",
        target.host
    ))))
}

fn request(
    agent: &Agent,
    target: &Target,
    etag: Option<&str>,
) -> std::result::Result<Response<Body>, ureq::Error> {
    let mut request = agent.get(target.url.as_str());
    // A cached ETag that isn't a valid header value is just a cache miss.
    if let Some(etag) = etag.filter(|e| HeaderValue::from_str(e).is_ok()) {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if target.is_loopback {
        request.config().proxy(None).build().call()
    } else {
        request.call()
    }
}

/// Refuses a name that resolves to this machine or the local network. The check happens
/// at the moment of connecting, so DNS can't give one answer to a check and another to
/// the connection.
#[derive(Debug, Default)]
struct PublicOnlyResolver {
    inner: DefaultResolver,
}

impl Resolver for PublicOnlyResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let addresses = self.inner.resolve(uri, config, timeout)?;
        // The player's own proxy may well live on this machine. It's their choice, not a
        // stranger's, and the name a stranger wrote is left for the proxy to resolve.
        if config.proxy().is_some_and(|proxy| proxy.uri() == uri) {
            return Ok(addresses);
        }
        let public = keep_public(&addresses, self.inner.empty());
        if public.is_empty() {
            log::warn!("Refused a private address for a link from an addon's page.");
            return Err(ureq::Error::HostNotFound);
        }
        Ok(public)
    }
}

/// The connector only ever tries what's kept here, so a private address is never contacted,
/// and a host with a stray private record still works through its public ones.
fn keep_public(addresses: &[SocketAddr], mut kept: ResolvedSocketAddrs) -> ResolvedSocketAddrs {
    for address in addresses {
        if is_public_ip(address.ip()) {
            kept.push(*address);
        }
    }
    kept
}

/// Whether an address is out on the public internet, not on this machine or a local network.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    let this_network = first == 0;
    let protocol_assignments = first == 192 && second == 0 && third == 0;
    let carrier_nat = first == 100 && (64..128).contains(&second);
    let reserved = first >= 240;
    !(this_network
        || protocol_assignments
        || carrier_nat
        || reserved
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast())
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // These forms carry an IPv4 address and reach whatever that address reaches.
    if let Some(v4) = ip.to_ipv4() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    let embedded = |high: u16, low: u16| {
        let [a, b] = high.to_be_bytes();
        let [c, d] = low.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    let local_nat64 = segments[..3] == [0x0064, 0xff9b, 1];
    if local_nat64 {
        return false;
    }
    let nat64 = segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0];
    if nat64 {
        return is_public_v4(embedded(segments[6], segments[7]));
    }
    let six_to_four = segments[0] == 0x2002;
    if six_to_four {
        return is_public_v4(embedded(segments[1], segments[2]));
    }
    let unique_local = segments[0] & 0xfe00 == 0xfc00;
    let link_local = segments[0] & 0xffc0 == 0xfe80;
    let site_local = segments[0] & 0xffc0 == 0xfec0;
    let multicast = segments[0] & 0xff00 == 0xff00;
    !(unique_local || link_local || site_local || multicast)
}

struct Opened {
    host: String,
    response: Response<Body>,
}

struct OpenFailure {
    error: Error,
    can_retry: bool,
}

impl OpenFailure {
    fn fatal(error: Error) -> Self {
        OpenFailure {
            error,
            can_retry: false,
        }
    }
}

/// A URL that passed the address checks, ready to request.
struct Target {
    url: String,
    host: String,
    is_loopback: bool,
}

impl Target {
    fn parse(url: &str, reach: Reach) -> Result<Target> {
        let url = escape_url(url.trim()).into_owned();
        let invalid = || Error::Other("That link isn't a web address this app can open.".into());
        let uri: Uri = url.parse().map_err(|_| invalid())?;
        let authority = uri.authority().ok_or_else(invalid)?;
        // `https://good.example@evil.example/` shows one host and connects to another.
        if authority.as_str().contains('@') {
            return Err(invalid());
        }
        let host = authority.host().to_string();
        let is_loopback = reach == Reach::PublicAndLoopback && is_loopback_host(&host);
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") if is_loopback => {}
            _ => {
                return Err(Error::Other(format!(
                    "Refused to connect to {host} because the link isn't secure (https)."
                )));
            }
        }
        if !is_loopback && !is_public_name(&host) {
            return Err(Error::Other(format!(
                "Refused to connect to {host} because it isn't a public web address."
            )));
        }
        Ok(Target {
            url,
            host,
            is_loopback,
        })
    }
}

/// Whether a URL passes the checks every request has to pass, without fetching it. A cheap
/// first look for links that won't be requested until later; the checks still run then.
pub fn is_public_url(url: &str) -> bool {
    Target::parse(url, Reach::Public).is_ok()
}

/// Whether `host` is a name on the public internet. An address written as numbers is
/// refused wherever it points: nothing this app fetches lives at one, and resolvers accept
/// many spellings of this machine, such as `2130706433`, `0x7f.1` and `127.1`. Labels are
/// held to what the TLS layer accepts, which also rules out special names Windows answers
/// itself, such as `..localmachine`. A name in non-ASCII letters is refused too: it's rare,
/// its punycode spelling works, and it keeps look-alikes of local names out.
fn is_public_name(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let is_valid_label = |label: &str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    if name.len() > 253 || !name.split('.').all(is_valid_label) {
        return false;
    }
    // A single word is a name on the local network: `localhost`, `router`, `nas`.
    let Some((_, last_label)) = name.rsplit_once('.') else {
        return false;
    };
    let is_number = last_label.bytes().all(|b| b.is_ascii_digit()) || last_label.starts_with("0x");
    !is_number
        && !INTERNAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Percent-encode the characters real download links contain but `http::Uri`
/// refuses, such as spaces in WoWInterface file names. Control characters stay,
/// so the URI parser rejects them instead of us smuggling them through.
fn escape_url(url: &str) -> Cow<'_, str> {
    let needs_escape = |b: u8| {
        matches!(
            b,
            b' ' | b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
        ) || b >= 0x7f
    };
    if !url.bytes().any(needs_escape) {
        return Cow::Borrowed(url);
    }
    let mut out = String::with_capacity(url.len() + 16);
    for ch in url.chars() {
        if ch.is_ascii() && !needs_escape(ch as u8) {
            out.push(ch);
        } else {
            let mut utf8 = [0u8; 4];
            for byte in ch.encode_utf8(&mut utf8).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    Cow::Owned(out)
}

/// Resolve a `Location` header against the URL that sent it.
fn join_location(base: &str, location: &str) -> String {
    let location = location.trim();
    if has_scheme(location) {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, base_path) = rest.split_at(path_start);
    if location.starts_with("//") {
        return format!("{scheme}:{location}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    let base_path = base_path.split(['?', '#']).next().unwrap_or("");
    if location.is_empty() || location.starts_with('?') {
        return format!("{scheme}://{authority}{base_path}{location}");
    }
    let dir = &base_path[..base_path.rfind('/').map_or(0, |i| i + 1)];
    let tail_start = location.find(['?', '#']).unwrap_or(location.len());
    let (relative, tail) = location.split_at(tail_start);
    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let trailing_slash = if relative.ends_with('/') { "/" } else { "" };
    format!(
        "{scheme}://{authority}/{}{trailing_slash}{tail}",
        segments.join("/")
    )
}

fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

fn read_limited(opened: Opened, max_bytes: u64) -> Result<Vec<u8>> {
    let host = opened.host;
    let declared = opened.response.body().content_length();
    if declared.is_some_and(|size| size > max_bytes) {
        return Err(too_big(&host, max_bytes));
    }
    let mut bytes = Vec::new();
    // The limit sits on the decoded bytes. ureq's own `limit()` counts the compressed
    // stream, which a gzip bomb would pass with ease.
    opened
        .response
        .into_body()
        .into_reader()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| body_error(e, &host))?;
    if bytes.len() as u64 > max_bytes {
        return Err(too_big(&host, max_bytes));
    }
    Ok(bytes)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    PathBuf::from(part)
}

fn sleep_unless_cancelled(delay: Duration, cancel: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn is_connect_failure(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Timeout(Timeout::Connect | Timeout::Resolve)
    )
}

fn offline(host: &str) -> Error {
    Error::Offline {
        host: host.to_string(),
    }
}

/// A read that fails partway. ureq has already compared the bytes against
/// `Content-Length` by this point, so a short body lands here too.
fn body_error(error: io::Error, host: &str) -> Error {
    match ureq::Error::from(error) {
        ureq::Error::Decompress(..) => {
            Error::Corrupt(format!("{host} sent a damaged compressed file"))
        }
        _ => offline(host),
    }
}

fn too_big(host: &str, max_bytes: u64) -> Error {
    Error::Corrupt(format!(
        "{host} sent more than the {} this app accepts",
        format_size(max_bytes)
    ))
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    match bytes {
        b if b >= GB => format!("{} GB", b / GB),
        b if b >= MB => format!("{} MB", b / MB),
        b if b >= KB => format!("{} KB", b / KB),
        b => format!("{b} bytes"),
    }
}

/// A tiny local HTTP server for tests. Handles one connection at a time and always
/// closes it, so ureq never reuses a socket across a test's requests.
#[cfg(test)]
pub(crate) mod test_server {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    pub struct Request {
        pub path: String,
        /// Header lines, lowercased names and values, one per line.
        pub headers: Vec<String>,
    }

    impl Request {
        pub fn header(&self, name: &str) -> Option<&str> {
            let prefix = format!("{name}: ");
            self.headers
                .iter()
                .find_map(|line| line.strip_prefix(prefix.as_str()))
        }
    }

    pub struct Server {
        pub base: String,
        pub hits: Arc<AtomicUsize>,
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Start a server. The handler writes the raw response; returning closes the socket.
    pub fn serve(handler: impl Fn(&Request, &mut TcpStream) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (stop, hits) = (stop.clone(), hits.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    hits.fetch_add(1, Ordering::SeqCst);
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    if let Some(request) = read_request(&stream) {
                        handler(&request, &mut stream);
                    }
                }
            })
        };
        Server {
            base: format!("http://{addr}"),
            hits,
            addr,
            stop,
            thread: Some(thread),
        }
    }

    fn read_request(stream: &TcpStream) -> Option<Request> {
        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        reader.read_line(&mut first).ok()?;
        let path = first.split_whitespace().nth(1)?.to_string();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            let line = line.trim_end();
            if line.is_empty() {
                return Some(Request { path, headers });
            }
            headers.push(line.to_ascii_lowercase());
        }
    }

    /// Write a complete response with a correct `Content-Length`.
    pub fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{Server, respond, serve};
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    fn no_progress(_: Progress) {}

    fn parse_for_test(url: &str) -> Result<Target> {
        Target::parse(url, Reach::PublicAndLoopback)
    }

    /// Lets the local test server stand in for a public site: the first hop may be the
    /// server, and every later hop is held to the rules the app runs with.
    fn open_from_stand_in(agent: &Agent, url: &str) -> std::result::Result<Opened, OpenFailure> {
        let start = Target::parse(url, Reach::PublicAndLoopback).map_err(OpenFailure::fatal)?;
        follow_redirects(agent, start, None, Reach::Public)
    }

    fn is_empty_dir(dir: &Path) -> bool {
        fs::read_dir(dir).unwrap().next().is_none()
    }

    #[test]
    fn user_agent_names_the_project_and_is_sent() {
        assert!(USER_AGENT.starts_with("WoW-is-a-Feeling/0."));
        assert!(USER_AGENT.ends_with("(+https://github.com/robbiemueller/wow-is-a-feeling)"));
        let seen = Arc::new(Mutex::new(String::new()));
        let captured = seen.clone();
        let server = serve(move |request, stream| {
            *captured.lock().unwrap() = request.header("user-agent").unwrap_or("").to_string();
            respond(stream, "200 OK", &[], b"hi");
        });
        Http::allowing_loopback()
            .get_bytes(&format!("{}/x", server.base), 100)
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), USER_AGENT.to_ascii_lowercase());
    }

    #[test]
    fn etag_flow_returns_body_then_not_modified() {
        let server = serve(|request, stream| {
            if request.header("if-none-match") == Some("\"v1\"") {
                let _ = stream.write_all(
                    b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\nETag: \"v1\"\r\n\r\n",
                );
            } else {
                respond(stream, "200 OK", &[("ETag", "\"v1\"")], b"catalog");
            }
        });
        let http = Http::allowing_loopback();
        let url = format!("{}/catalog.json", server.base);
        let first = http.get_if_modified(&url, None, 100).unwrap();
        assert_eq!(
            first,
            Fetched::Body {
                bytes: b"catalog".to_vec(),
                etag: Some("\"v1\"".into())
            }
        );
        assert_eq!(
            http.get_if_modified(&url, Some("\"v1\""), 100).unwrap(),
            Fetched::NotModified
        );
        let changed = http.get_if_modified(&url, Some("\"old\""), 100).unwrap();
        assert!(matches!(changed, Fetched::Body { .. }));
    }

    #[test]
    fn a_corrupt_cached_etag_is_ignored() {
        let server = serve(|request, stream| {
            assert!(request.header("if-none-match").is_none());
            respond(stream, "200 OK", &[], b"fresh");
        });
        let fetched = Http::allowing_loopback()
            .get_if_modified(&format!("{}/x", server.base), Some("bad\r\nvalue"), 100)
            .unwrap();
        assert!(matches!(fetched, Fetched::Body { .. }));
    }

    #[test]
    fn oversized_body_is_rejected_by_declared_length() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], &[7u8; 1000]));
        let http = Http::allowing_loopback();
        let url = format!("{}/big", server.base);
        assert!(matches!(http.get_bytes(&url, 999), Err(Error::Corrupt(_))));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("big.zip");
        let result = http.download(&url, &dest, &mut no_progress, &AtomicBool::new(false), 999);
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn oversized_body_is_rejected_when_length_is_undeclared() {
        let server = serve(|_, stream| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&[7u8; 300_000]);
        });
        let http = Http::allowing_loopback();
        let url = format!("{}/stream", server.base);
        assert!(matches!(http.get_bytes(&url, 1000), Err(Error::Corrupt(_))));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("stream.zip");
        let result = http.download(&url, &dest, &mut no_progress, &AtomicBool::new(false), 1000);
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn a_gzip_bomb_is_measured_after_decoding() {
        use flate2::{Compression, write::GzEncoder};
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&vec![0u8; 5_000_000]).unwrap();
        let bomb = encoder.finish().unwrap();
        assert!(bomb.len() < 50_000);
        let server = serve(move |_, stream| {
            respond(stream, "200 OK", &[("Content-Encoding", "gzip")], &bomb);
        });
        let http = Http::allowing_loopback();
        let url = format!("{}/bomb", server.base);
        assert!(matches!(
            http.get_bytes(&url, 100_000),
            Err(Error::Corrupt(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bomb");
        let result = http.download(
            &url,
            &dest,
            &mut no_progress,
            &AtomicBool::new(false),
            100_000,
        );
        assert!(matches!(result, Err(Error::Corrupt(_))));
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn gzip_bodies_are_decoded() {
        use flate2::{Compression, write::GzEncoder};
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let gz = encoder.finish().unwrap();
        let server = serve(move |request, stream| {
            assert!(request.header("accept-encoding").unwrap().contains("gzip"));
            respond(stream, "200 OK", &[("Content-Encoding", "gzip")], &gz);
        });
        let body = Http::allowing_loopback()
            .get_bytes(&format!("{}/x", server.base), 100)
            .unwrap();
        assert_eq!(body, b"hello gzip");
    }

    #[test]
    fn truncated_gzip_is_an_error() {
        use flate2::{Compression, write::GzEncoder};
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[9u8; 4096]).unwrap();
        let mut gz = encoder.finish().unwrap();
        gz.truncate(gz.len() - 6);
        let server = serve(move |_, stream| {
            respond(stream, "200 OK", &[("Content-Encoding", "gzip")], &gz);
        });
        let result = Http::allowing_loopback().get_bytes(&format!("{}/x", server.base), 100_000);
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }

    #[test]
    fn download_writes_the_file_replaces_old_and_leaves_no_part() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"new contents"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("addon.zip");
        fs::write(&dest, b"old").unwrap();
        let mut reports = Vec::new();
        let size = Http::allowing_loopback()
            .download(
                &format!("{}/addon.zip", server.base),
                &dest,
                &mut |p| reports.push(p),
                &AtomicBool::new(false),
                100,
            )
            .unwrap();
        assert_eq!(size, 12);
        assert_eq!(fs::read(&dest).unwrap(), b"new contents");
        assert!(!part_path(&dest).exists());
        assert_eq!(reports.first().unwrap().done, 0);
        assert_eq!(
            reports.last().unwrap(),
            &Progress {
                done: 12,
                total: Some(12)
            }
        );
    }

    #[test]
    fn cancel_mid_stream_leaves_no_part_and_keeps_the_old_file() {
        let server = serve(|_, stream| {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 10000000\r\n\r\n",
            );
            for _ in 0..1000 {
                if stream.write_all(&[1u8; 10_000]).is_err() {
                    return;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(5));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("addon.zip");
        fs::write(&dest, b"old").unwrap();
        let cancel = AtomicBool::new(false);
        let result = Http::allowing_loopback().download(
            &format!("{}/addon.zip", server.base),
            &dest,
            &mut |p| {
                if p.done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
            u64::MAX,
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(!part_path(&dest).exists());
        assert_eq!(fs::read(&dest).unwrap(), b"old");
    }

    #[test]
    fn already_cancelled_never_connects() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"x"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        let result = Http::allowing_loopback().download(
            &format!("{}/x", server.base),
            &dest,
            &mut no_progress,
            &AtomicBool::new(true),
            100,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn truncated_body_is_an_error_not_a_short_success() {
        let server = serve(|_, stream| {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nonly ten b",
            );
        });
        let http = Http::allowing_loopback();
        let url = format!("{}/short", server.base);
        assert!(matches!(
            http.get_bytes(&url, 1000),
            Err(Error::Offline { .. })
        ));

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("short.zip");
        let result = http.download(&url, &dest, &mut no_progress, &AtomicBool::new(false), 1000);
        assert!(matches!(result, Err(Error::Offline { .. })), "{result:?}");
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn truncated_chunked_body_is_an_error() {
        let server = serve(|_, stream| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n");
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("chunked.zip");
        let result = Http::allowing_loopback().download(
            &format!("{}/x", server.base),
            &dest,
            &mut no_progress,
            &AtomicBool::new(false),
            1000,
        );
        assert!(result.is_err(), "{result:?}");
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn http_errors_map_to_http_status_with_the_host() {
        let server = serve(|_, stream| respond(stream, "503 Service Unavailable", &[], b"nope"));
        let http = Http::allowing_loopback();
        let url = format!("{}/x", server.base);
        match http.get_bytes(&url, 100) {
            Err(Error::HttpStatus { host, status }) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(status, 503);
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        let result = http.download(&url, &dest, &mut no_progress, &AtomicBool::new(false), 100);
        assert!(matches!(result, Err(Error::HttpStatus { status: 503, .. })));
        assert!(is_empty_dir(dir.path()));
    }

    #[test]
    fn nothing_listening_is_offline() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let result =
            Http::allowing_loopback().get_bytes(&format!("http://127.0.0.1:{port}/x"), 100);
        assert!(matches!(result, Err(Error::Offline { .. })), "{result:?}");
    }

    fn server_that_drops_first(drops: usize) -> (Server, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let server = serve(move |_, stream| {
            if seen.fetch_add(1, Ordering::SeqCst) >= drops {
                respond(stream, "200 OK", &[], b"made it");
            }
        });
        (server, count)
    }

    #[test]
    fn download_retries_failed_connections_twice() {
        let (server, count) = server_that_drops_first(2);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        Http::allowing_loopback()
            .download(
                &format!("{}/x", server.base),
                &dest,
                &mut no_progress,
                &AtomicBool::new(false),
                100,
            )
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert_eq!(fs::read(&dest).unwrap(), b"made it");
    }

    #[test]
    fn download_gives_up_after_three_attempts() {
        let (server, count) = server_that_drops_first(usize::MAX);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        let result = Http::allowing_loopback().download(
            &format!("{}/x", server.base),
            &dest,
            &mut no_progress,
            &AtomicBool::new(false),
            100,
        );
        assert!(matches!(result, Err(Error::Offline { .. })), "{result:?}");
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn http_errors_are_not_retried() {
        let server = serve(|_, stream| respond(stream, "404 Not Found", &[], b""));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        let result = Http::allowing_loopback().download(
            &format!("{}/x", server.base),
            &dest,
            &mut no_progress,
            &AtomicBool::new(false),
            100,
        );
        assert!(matches!(result, Err(Error::HttpStatus { status: 404, .. })));
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn progress_is_reported_about_ten_times_a_second() {
        let server = serve(|_, stream| {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 12000\r\n\r\n",
            );
            for _ in 0..120 {
                let _ = stream.write_all(&[1u8; 100]);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(5));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        let started = Instant::now();
        let mut reports = 0u64;
        Http::allowing_loopback()
            .download(
                &format!("{}/x", server.base),
                &dest,
                &mut |_| reports += 1,
                &AtomicBool::new(false),
                u64::MAX,
            )
            .unwrap();
        let allowed = started.elapsed().as_millis() as u64 / 100 + 2;
        assert!(reports <= allowed, "{reports} reports, allowed {allowed}");
    }

    #[test]
    fn plain_http_is_refused_except_on_loopback() {
        let http = Http::allowing_loopback();
        for url in [
            "http://example.com/x",
            "HTTP://example.com/x",
            "http://127.0.0.1.example.com/x",
            "http://2130706433/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "//example.com/x",
            "not a url",
        ] {
            let result = http.get_bytes(url, 10);
            assert!(matches!(result, Err(Error::Other(_))), "{url}: {result:?}");
        }
        for url in [
            "http://127.0.0.1/x",
            "http://127.1.2.3:8080/x",
            "http://localhost:1/x",
            "http://[::1]:1/x",
            "https://example.com/x",
        ] {
            assert!(parse_for_test(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn userinfo_in_a_url_is_refused() {
        assert!(parse_for_test("https://github.com@evil.example/x").is_err());
        assert!(parse_for_test("http://user:pw@127.0.0.1/x").is_err());
    }

    #[test]
    fn redirects_are_followed_with_every_hop_checked() {
        let server = serve(|request, stream| match request.path.as_str() {
            "/start" => respond(stream, "302 Found", &[("Location", "/next/step")], b""),
            "/next/step" => respond(
                stream,
                "301 Moved",
                &[("Location", "final?x=1")],
                b"ignored",
            ),
            "/next/final?x=1" => respond(stream, "200 OK", &[], b"arrived"),
            other => panic!("unexpected path {other}"),
        });
        let body = Http::allowing_loopback()
            .get_bytes(&format!("{}/start", server.base), 100)
            .unwrap();
        assert_eq!(body, b"arrived");
    }

    #[test]
    fn a_redirect_to_plain_http_is_refused_without_connecting() {
        let server = serve(|_, stream| {
            respond(
                stream,
                "302 Found",
                &[("Location", "http://example.com/stolen")],
                b"",
            );
        });
        let result = Http::allowing_loopback().get_bytes(&format!("{}/x", server.base), 100);
        assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
    }

    #[test]
    fn the_apps_client_refuses_this_machine_and_the_local_network() {
        let http = Http::new();
        for url in [
            "http://127.0.0.1:1/x",
            "http://localhost:1/x",
            "http://[::1]:1/x",
            "https://127.0.0.1/x",
            "https://127.0.0.1./x",
            "https://127.1/x",
            "https://2130706433/x",
            "https://0x7f.0.0.1/x",
            "https://0x7F000001/x",
            "https://0177.0.0.1/x",
            "https://localhost/x",
            "https://LOCALHOST./x",
            "https://[::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[fe80::1]/x",
            "https://[fc00::1]/x",
            "https://10.0.0.1/x",
            "https://172.16.0.1/x",
            "https://192.168.1.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://0.0.0.0/x",
            "https://8.8.8.8/x",
            "https://router/x",
            "https://nas.local/x",
            "https://printer.lan/x",
            "https://foo.localhost/x",
            "https://vault.internal/x",
            "https://box.home.arpa/x",
            "https://%6c%6f%63%61%6c%68%6f%73%74/x",
            "https://0--1.ipv6-literal.net/x",
            "https://fe80--1s1.ipv6-literal.net/x",
            "https://SNOW.mshome.net/x",
            "https://..localmachine/x",
            "https://.router/x",
            "https://a..b/x",
            "https://x-.127.0.0.1.nip.io/x",
            "https://x.local[::1]/x",
            "https://münchen.de/x",
            "https://ｌｏｃａｌｈｏｓｔ/x",
            "https://127。0。0。1/x",
        ] {
            let result = http.get_bytes(url, 10);
            assert!(matches!(result, Err(Error::Other(_))), "{url}: {result:?}");
            let result = http.get_untrusted(url, 10);
            assert!(matches!(result, Err(Error::Other(_))), "{url}: {result:?}");
            assert!(!is_public_url(url), "{url}");
        }
    }

    #[test]
    fn public_names_pass_the_address_check() {
        for url in [
            "https://raw.githubusercontent.com/a/b/HEAD/c.png",
            "https://github.com/a/b/releases/download/v1/c.zip",
            "https://api.mmoui.com:443/v3/game/WOW/filelist.json",
            "https://cdn.wowinterface.com/downloads/My Addon.zip",
            "https://GitHub.com./x",
            "https://localhost.example.com/x",
            "https://example.co.uk/x",
            "https://xn--mnchen-3ya.de/x",
            "https://release-assets.githubusercontent.com/x",
            "https://cdn-wow.mmoui.com/preview/pvw29.jpg",
            "https://under_score.example.com/x",
        ] {
            assert!(Target::parse(url, Reach::Public).is_ok(), "{url}");
        }
    }

    #[test]
    fn a_public_page_cannot_redirect_the_app_to_this_machine() {
        let secret = serve(|_, stream| respond(stream, "200 OK", &[], b"secret"));
        let location = format!("{}/secret", secret.base);
        let page = serve(move |_, stream| {
            respond(stream, "302 Found", &[("Location", &location)], b"");
        });
        let Err(failure) = open_from_stand_in(&Http::new().agent, &format!("{}/x", page.base))
        else {
            panic!("followed a redirect to this machine");
        };
        assert!(
            matches!(failure.error, Error::Other(_)),
            "{:?}",
            failure.error
        );
        assert_eq!(secret.hits.load(Ordering::SeqCst), 0);
        assert_eq!(page.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_strangers_link_never_connects_to_a_private_address() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"secret"));
        let url = format!("{}/x", server.base);
        let http = Http::new();
        let Err(failure) = open_from_stand_in(&http.stranger_agent, &url) else {
            panic!("connected to this machine");
        };
        assert!(
            matches!(failure.error, Error::Offline { .. }),
            "{:?}",
            failure.error
        );
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
        // The server was there all along, so it was the resolver that refused.
        assert!(open_from_stand_in(&http.agent, &url).is_ok());
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_players_own_proxy_may_live_on_this_machine() {
        let proxy = serve(|_, stream| respond(stream, "502 Bad Gateway", &[], b""));
        let config = Agent::config_builder()
            .proxy(Some(ureq::Proxy::new(&proxy.base).unwrap()))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        let agent = Agent::with_parts(
            config,
            DefaultConnector::default(),
            PublicOnlyResolver::default(),
        );
        // The proxy answers the CONNECT with an error, which is all this needs: the
        // resolver let the connection to the proxy through.
        assert!(agent.get("https://example.com/x").call().is_err());
        assert_eq!(proxy.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn private_answers_are_dropped_and_public_ones_kept() {
        let empty = || DefaultResolver::default().empty();
        let address = |text: &str| -> SocketAddr { text.parse().unwrap() };
        let (public, private) = (address("140.82.112.3:443"), address("10.0.0.1:443"));
        let local = address("[::1]:443");
        let kept = keep_public(&[private, public, local], empty());
        assert_eq!(&kept[..], &[public]);
        // A DNS-based proxy tool answers with these two for every public name.
        let (fake_v4, fake_v6) = (
            address("198.18.0.1:443"),
            address("[fdfe:dcba:9876::1]:443"),
        );
        assert_eq!(&keep_public(&[fake_v6, fake_v4], empty())[..], &[fake_v4]);
        assert!(keep_public(&[private, local], empty()).is_empty());
    }

    #[test]
    fn a_strangers_server_never_gets_the_os_certificate_checker() {
        let http = Http::new();
        assert!(matches!(
            http.stranger_agent.config().tls_config().root_certs(),
            RootCerts::WebPki
        ));
        assert!(matches!(
            http.agent.config().tls_config().root_certs(),
            RootCerts::PlatformVerifier
        ));
    }

    #[test]
    fn a_redirect_with_raw_utf8_in_it_is_followed() {
        let server = serve(|request, stream| match request.path.as_str() {
            "/start" => respond(stream, "302 Found", &[("Location", "/files/café.zip")], b""),
            "/files/caf%C3%A9.zip" => respond(stream, "200 OK", &[], b"arrived"),
            other => panic!("unexpected path {other}"),
        });
        let body = Http::allowing_loopback()
            .get_bytes(&format!("{}/start", server.base), 100)
            .unwrap();
        assert_eq!(body, b"arrived");
    }

    #[test]
    fn only_addresses_on_the_public_internet_count_as_public() {
        for ip in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::c0a8:101",
            "2002:7f00:1::",
            "2002:c0a8:101::",
            "192.0.0.1",
            "192.0.0.255",
            "64:ff9b:1::c0a8:101",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "140.82.112.3",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.1.1",
            "192.0.1.1",
            // DNS-based proxy tools hand out this range for every public name.
            "198.18.0.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn five_redirects_are_allowed_and_six_are_not() {
        let hops = |limit: usize| {
            serve(move |request, stream| {
                let step: usize = request.path.trim_start_matches('/').parse().unwrap();
                if step < limit {
                    let next = format!("/{}", step + 1);
                    respond(stream, "302 Found", &[("Location", next.as_str())], b"");
                } else {
                    respond(stream, "200 OK", &[], b"done");
                }
            })
        };
        let server = hops(5);
        assert_eq!(
            Http::allowing_loopback()
                .get_bytes(&format!("{}/0", server.base), 100)
                .unwrap(),
            b"done"
        );
        let server = hops(6);
        let result = Http::allowing_loopback().get_bytes(&format!("{}/0", server.base), 100);
        assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
        assert_eq!(server.hits.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_redirect_without_location_is_an_error() {
        let server = serve(|_, stream| respond(stream, "302 Found", &[], b""));
        let result = Http::allowing_loopback().get_bytes(&format!("{}/x", server.base), 100);
        assert!(matches!(result, Err(Error::HttpStatus { status: 302, .. })));
    }

    #[test]
    fn spaces_in_links_are_escaped() {
        let server = serve(|request, stream| {
            assert_eq!(request.path, "/My%20Addon%20%C3%A9.zip");
            respond(stream, "200 OK", &[], b"ok");
        });
        let body = Http::allowing_loopback()
            .get_bytes(&format!("{}/My Addon é.zip", server.base), 100)
            .unwrap();
        assert_eq!(body, b"ok");
    }

    #[test]
    fn control_characters_in_links_are_refused() {
        assert!(parse_for_test("https://example.com/a\r\nX-Evil: 1").is_err());
        assert!(parse_for_test("https://example.com/a\0b").is_err());
    }

    #[test]
    fn location_joining() {
        let base = "https://a.example/x/y/z.zip?sig=1";
        assert_eq!(
            join_location(base, "https://b.example/q"),
            "https://b.example/q"
        );
        assert_eq!(join_location(base, "//c.example/q"), "https://c.example/q");
        assert_eq!(join_location(base, "/root"), "https://a.example/root");
        assert_eq!(
            join_location(base, "sib.zip"),
            "https://a.example/x/y/sib.zip"
        );
        assert_eq!(
            join_location(base, "../up.zip?a=b"),
            "https://a.example/x/up.zip?a=b"
        );
        assert_eq!(join_location(base, "./dir/"), "https://a.example/x/y/dir/");
        assert_eq!(
            join_location(base, "?other=2"),
            "https://a.example/x/y/z.zip?other=2"
        );
        assert_eq!(
            join_location("https://a.example", "b"),
            "https://a.example/b"
        );
        assert_eq!(
            join_location(base, "file:///etc/passwd"),
            "file:///etc/passwd"
        );
        assert!(parse_for_test(&join_location(base, "file:///etc/passwd")).is_err());
    }

    #[test]
    fn error_text_never_contains_the_full_url() {
        let server = serve(|_, stream| respond(stream, "500 Oops", &[], b""));
        let url = format!("{}/asset?token=SECRET", server.base);
        let text = Http::allowing_loopback()
            .get_bytes(&url, 10)
            .unwrap_err()
            .to_string();
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(512 * 1024), "512 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2 GB");
    }

    #[test]
    #[ignore = "reaches the real internet; run by hand with --ignored"]
    fn live_https_uses_the_platform_verifier() {
        let http = Http::new();
        let bytes = http
            .get_bytes(
                "https://raw.githubusercontent.com/robbiemueller/wow-is-a-feeling/HEAD/README.md",
                1024 * 1024,
            )
            .unwrap();
        assert!(!bytes.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("license.txt");
        let size = http
            .download(
                "https://github.com/robbiemueller/wow-is-a-feeling/raw/HEAD/LICENSE",
                &dest,
                &mut no_progress,
                &AtomicBool::new(false),
                1024 * 1024,
            )
            .unwrap();
        assert_eq!(size, fs::metadata(&dest).unwrap().len());
        // A stranger's link to a public file works, redirects and all.
        let license = http
            .get_untrusted(
                "https://github.com/robbiemueller/wow-is-a-feeling/raw/HEAD/LICENSE",
                1024 * 1024,
            )
            .unwrap();
        assert_eq!(license.len() as u64, size);
    }
}
