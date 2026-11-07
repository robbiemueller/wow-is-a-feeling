//! HTTP for everything the app fetches: catalog with ETags, addon zips, update files.
//!
//! Redirects are followed by hand so every hop goes through the same URL check.
//! ureq only exposes a single https-only switch, which can't express "https
//! everywhere, plain http on loopback only" across a redirect chain.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ureq::http::header::{ETAG, IF_NONE_MATCH, LOCATION};
use ureq::http::{HeaderValue, Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig};
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

/// A shared HTTP client. Cheap to clone; clones share one connection pool.
#[derive(Clone)]
pub struct Http {
    agent: Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let agent = Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_per_read(Some(IO_TIMEOUT))
            .timeout_per_write(Some(IO_TIMEOUT))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        Http { agent }
    }

    /// Fetch a URL into memory. Anything over `max_bytes` is an error.
    pub fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let opened = self.open(url, None)?;
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
        let mut target = Target::parse(url).map_err(OpenFailure::fatal)?;
        for _ in 0..=MAX_REDIRECTS {
            let response = self.request(&target, etag).map_err(|e| OpenFailure {
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
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok());
            match location {
                Some(location) if status.is_redirection() => {
                    let next = join_location(&target.url, location);
                    target = Target::parse(&next).map_err(OpenFailure::fatal)?;
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
        &self,
        target: &Target,
        etag: Option<&str>,
    ) -> std::result::Result<Response<Body>, ureq::Error> {
        let mut request = self.agent.get(target.url.as_str());
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

/// A URL that passed the scheme check, ready to request.
struct Target {
    url: String,
    host: String,
    is_loopback: bool,
}

impl Target {
    fn parse(url: &str) -> Result<Target> {
        let url = escape_url(url.trim()).into_owned();
        let invalid = || Error::Other("That link isn't a web address this app can open.".into());
        let uri: Uri = url.parse().map_err(|_| invalid())?;
        let authority = uri.authority().ok_or_else(invalid)?;
        // `https://good.example@evil.example/` shows one host and connects to another.
        if authority.as_str().contains('@') {
            return Err(invalid());
        }
        let host = authority.host().to_string();
        let is_loopback = is_loopback_host(&host);
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") if is_loopback => {}
            _ => {
                return Err(Error::Other(format!(
                    "Refused to connect to {host} because the link isn't secure (https)."
                )));
            }
        }
        Ok(Target {
            url,
            host,
            is_loopback,
        })
    }
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
        Http::new()
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
        let http = Http::new();
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
        let fetched = Http::new()
            .get_if_modified(&format!("{}/x", server.base), Some("bad\r\nvalue"), 100)
            .unwrap();
        assert!(matches!(fetched, Fetched::Body { .. }));
    }

    #[test]
    fn oversized_body_is_rejected_by_declared_length() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], &[7u8; 1000]));
        let http = Http::new();
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
        let http = Http::new();
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
        let http = Http::new();
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
        let body = Http::new()
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
        let result = Http::new().get_bytes(&format!("{}/x", server.base), 100_000);
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }

    #[test]
    fn download_writes_the_file_replaces_old_and_leaves_no_part() {
        let server = serve(|_, stream| respond(stream, "200 OK", &[], b"new contents"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("addon.zip");
        fs::write(&dest, b"old").unwrap();
        let mut reports = Vec::new();
        let size = Http::new()
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
        let result = Http::new().download(
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
        let result = Http::new().download(
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
        let http = Http::new();
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
        let result = Http::new().download(
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
        let http = Http::new();
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
        let result = Http::new().get_bytes(&format!("http://127.0.0.1:{port}/x"), 100);
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
        Http::new()
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
        let result = Http::new().download(
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
        let result = Http::new().download(
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
        Http::new()
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
        let http = Http::new();
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
            assert!(Target::parse(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn userinfo_in_a_url_is_refused() {
        assert!(Target::parse("https://github.com@evil.example/x").is_err());
        assert!(Target::parse("http://user:pw@127.0.0.1/x").is_err());
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
        let body = Http::new()
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
        let result = Http::new().get_bytes(&format!("{}/x", server.base), 100);
        assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
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
            Http::new()
                .get_bytes(&format!("{}/0", server.base), 100)
                .unwrap(),
            b"done"
        );
        let server = hops(6);
        let result = Http::new().get_bytes(&format!("{}/0", server.base), 100);
        assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
        assert_eq!(server.hits.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_redirect_without_location_is_an_error() {
        let server = serve(|_, stream| respond(stream, "302 Found", &[], b""));
        let result = Http::new().get_bytes(&format!("{}/x", server.base), 100);
        assert!(matches!(result, Err(Error::HttpStatus { status: 302, .. })));
    }

    #[test]
    fn spaces_in_links_are_escaped() {
        let server = serve(|request, stream| {
            assert_eq!(request.path, "/My%20Addon%20%C3%A9.zip");
            respond(stream, "200 OK", &[], b"ok");
        });
        let body = Http::new()
            .get_bytes(&format!("{}/My Addon é.zip", server.base), 100)
            .unwrap();
        assert_eq!(body, b"ok");
    }

    #[test]
    fn control_characters_in_links_are_refused() {
        assert!(Target::parse("https://example.com/a\r\nX-Evil: 1").is_err());
        assert!(Target::parse("https://example.com/a\0b").is_err());
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
        assert!(Target::parse(&join_location(base, "file:///etc/passwd")).is_err());
    }

    #[test]
    fn error_text_never_contains_the_full_url() {
        let server = serve(|_, stream| respond(stream, "500 Oops", &[], b""));
        let url = format!("{}/asset?token=SECRET", server.base);
        let text = Http::new().get_bytes(&url, 10).unwrap_err().to_string();
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
    }
}
