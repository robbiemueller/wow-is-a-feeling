//! Blocking HTTP with retries, per-host pacing and request counters. The token
//! only ever goes to api.github.com and is never logged.

use std::cell::RefCell;
use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ureq::Agent;

pub const USER_AGENT: &str =
    "WoW-is-a-Feeling/0.1 (+https://github.com/robbiemueller/wow-is-a-feeling)";

/// Bodies of API answers and small files.
pub const SMALL_BODY: u64 = 32 * 1024 * 1024;
/// A whole zip from a host that can't do range requests.
pub const ZIP_BODY: u64 = 400 * 1024 * 1024;

const MAX_ATTEMPTS: u32 = 6;
/// A host that can't be reached rarely comes back within a minute, and there may
/// be thousands of requests to get through.
const MAX_CONNECTION_ATTEMPTS: u32 = 4;
/// GitHub's primary limit resets hourly, so a longer wait means something is off.
const MAX_RATE_WAIT_SECS: u64 = 65 * 60;

/// Where a request goes. Decides pacing, whether the token is sent, and how it is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    GraphQl,
    Search,
    CodeSearch,
    Raw,
    Asset,
    Wowi,
    Direct,
}

impl Channel {
    const COUNT: usize = 7;

    fn index(self) -> usize {
        self as usize
    }

    /// Search allows 30 requests a minute and code search 10. WoWInterface gets
    /// a small gap between requests because we're a guest there.
    fn spacing(self) -> Duration {
        match self {
            Channel::Search => Duration::from_millis(2200),
            Channel::CodeSearch => Duration::from_millis(6500),
            Channel::Wowi => Duration::from_millis(750),
            _ => Duration::ZERO,
        }
    }

    fn uses_token(self) -> bool {
        matches!(
            self,
            Channel::GraphQl | Channel::Search | Channel::CodeSearch
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Channel::GraphQl => "github graphql",
            Channel::Search => "github search",
            Channel::CodeSearch => "github code search",
            Channel::Raw => "raw.githubusercontent",
            Channel::Asset => "release assets",
            Channel::Wowi => "wowinterface",
            Channel::Direct => "direct hosts",
        }
    }

    pub const ALL: [Channel; Channel::COUNT] = [
        Channel::GraphQl,
        Channel::Search,
        Channel::CodeSearch,
        Channel::Raw,
        Channel::Asset,
        Channel::Wowi,
        Channel::Direct,
    ];
}

#[derive(Debug, Default)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub content_range: Option<String>,
    pub rate_reset: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    Status(u16),
    Transport(String),
    TooLarge,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Status(code) => write!(f, "HTTP {code}"),
            FetchError::Transport(detail) => write!(f, "connection failed: {detail}"),
            FetchError::TooLarge => write!(f, "the response was larger than allowed"),
        }
    }
}

impl std::error::Error for FetchError {}

impl FetchError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, FetchError::Status(404 | 410))
    }
}

/// What the response headers say about when to try again.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateHints {
    pub retry_after: Option<u64>,
    pub remaining: Option<i64>,
    pub reset: Option<i64>,
}

/// How long to wait before retrying a failed status, or `None` when retrying is
/// pointless. Pure, so the rules can be tested without a clock or a server.
pub fn retry_delay(status: u16, hints: RateHints, attempt: u32, now: i64) -> Option<Duration> {
    match status {
        403 | 429 => {
            if let Some(secs) = hints.retry_after {
                return Some(Duration::from_secs(secs.min(MAX_RATE_WAIT_SECS)));
            }
            if hints.remaining == Some(0) {
                let reset = hints.reset.unwrap_or(now + 60);
                let wait = u64::try_from(reset.saturating_sub(now)).unwrap_or(0) + 2;
                return Some(Duration::from_secs(wait.min(MAX_RATE_WAIT_SECS)));
            }
            (status == 429).then(|| backoff(attempt))
        }
        408 | 500 | 502 | 503 | 504 => Some(backoff(attempt)),
        _ => None,
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2u64.pow(attempt.min(6)))
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub struct Http {
    agent: Agent,
    token: Option<String>,
    counts: RefCell<[u64; Channel::COUNT]>,
    last_request: RefCell<[Option<Instant>; Channel::COUNT]>,
    bytes: RefCell<u64>,
    /// Shortens every wait in tests.
    wait_scale: f64,
}

impl Http {
    pub fn new(token: Option<String>) -> Self {
        let agent = Agent::config_builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .https_only(true)
            .max_redirects(5)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .build()
            .into();
        Self::with_agent(agent, token, 1.0)
    }

    fn with_agent(agent: Agent, token: Option<String>, wait_scale: f64) -> Self {
        Self {
            agent,
            token,
            counts: RefCell::new([0; Channel::COUNT]),
            last_request: RefCell::new([None; Channel::COUNT]),
            bytes: RefCell::new(0),
            wait_scale,
        }
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Requests made so far per channel, including retries.
    pub fn counts(&self) -> Vec<(Channel, u64)> {
        let counts = self.counts.borrow();
        Channel::ALL
            .iter()
            .map(|&c| (c, counts[c.index()]))
            .collect()
    }

    pub fn bytes(&self) -> u64 {
        *self.bytes.borrow()
    }

    pub fn get(&self, channel: Channel, url: &str) -> Result<Reply, FetchError> {
        self.send(channel, url, &[], None, SMALL_BODY)
    }

    pub fn get_with(
        &self,
        channel: Channel,
        url: &str,
        headers: &[(&str, &str)],
        limit: u64,
    ) -> Result<Reply, FetchError> {
        self.send(channel, url, headers, None, limit)
    }

    pub fn post_json(&self, channel: Channel, url: &str, body: &str) -> Result<Reply, FetchError> {
        self.send(
            channel,
            url,
            &[("content-type", "application/json")],
            Some(body),
            SMALL_BODY,
        )
    }

    /// Sleeps until `reset`, for callers that learn about a limit from a response body.
    pub fn wait_for_reset(&self, reset: Option<i64>) {
        let hints = RateHints {
            remaining: Some(0),
            reset,
            ..RateHints::default()
        };
        if let Some(wait) = retry_delay(429, hints, 0, unix_now()) {
            eprintln!("rate limited, waiting {}s", wait.as_secs());
            self.wait(wait);
        }
    }

    fn wait(&self, duration: Duration) {
        sleep(duration.mul_f64(self.wait_scale));
    }

    fn pace(&self, channel: Channel) {
        let spacing = channel.spacing().mul_f64(self.wait_scale);
        let mut last = self.last_request.borrow_mut();
        if let Some(previous) = last[channel.index()] {
            let elapsed = previous.elapsed();
            if elapsed < spacing {
                sleep(spacing - elapsed);
            }
        }
        last[channel.index()] = Some(Instant::now());
        self.counts.borrow_mut()[channel.index()] += 1;
    }

    fn send(
        &self,
        channel: Channel,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
        limit: u64,
    ) -> Result<Reply, FetchError> {
        let mut attempt = 0;
        loop {
            self.pace(channel);
            let failure = match self.once(channel, url, headers, body, limit) {
                Ok((reply, _)) if matches!(reply.status, 200..=299 | 304) => return Ok(reply),
                Ok((reply, hints)) => {
                    let delay = retry_delay(reply.status, hints, attempt, unix_now());
                    match delay {
                        Some(delay) if attempt + 1 < MAX_ATTEMPTS => {
                            eprintln!(
                                "{}: HTTP {} on {}, retrying in {}s",
                                channel.label(),
                                reply.status,
                                loggable(url),
                                delay.as_secs()
                            );
                            self.wait(delay);
                            attempt += 1;
                            continue;
                        }
                        _ => FetchError::Status(reply.status),
                    }
                }
                Err(error) => error,
            };
            if !matches!(failure, FetchError::Transport(_))
                || attempt + 1 >= MAX_CONNECTION_ATTEMPTS
            {
                return Err(failure);
            }
            eprintln!(
                "{}: {failure} on {}, retrying",
                channel.label(),
                loggable(url)
            );
            self.wait(backoff(attempt));
            attempt += 1;
        }
    }

    fn once(
        &self,
        channel: Channel,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
        limit: u64,
    ) -> Result<(Reply, RateHints), FetchError> {
        let transport = |e: ureq::Error| match e {
            ureq::Error::BodyExceedsLimit(_) => FetchError::TooLarge,
            other => FetchError::Transport(other.to_string()),
        };
        let token = self.token.as_deref().filter(|_| channel.uses_token());
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut response = match body {
            Some(text) => {
                let mut request = self.agent.post(url);
                if let Some(value) = &auth {
                    request = request.header("authorization", value);
                }
                for (key, value) in headers {
                    request = request.header(*key, *value);
                }
                request.send(text)
            }
            None => {
                let mut request = self.agent.get(url);
                if let Some(value) = &auth {
                    request = request.header("authorization", value);
                }
                for (key, value) in headers {
                    request = request.header(*key, *value);
                }
                request.call()
            }
        }
        .map_err(transport)?;

        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let hints = RateHints {
            retry_after: header("retry-after").and_then(|v| v.trim().parse().ok()),
            remaining: header("x-ratelimit-remaining").and_then(|v| v.trim().parse().ok()),
            reset: header("x-ratelimit-reset").and_then(|v| v.trim().parse().ok()),
        };
        let mut reply = Reply {
            status,
            etag: header("etag"),
            content_range: header("content-range"),
            rate_reset: hints.reset,
            body: Vec::new(),
        };
        if matches!(status, 200..=299) {
            reply.body = response
                .body_mut()
                .with_config()
                .limit(limit)
                .read_to_vec()
                .map_err(transport)?;
            *self.bytes.borrow_mut() += reply.body.len() as u64;
        }
        Ok((reply, hints))
    }
}

/// Drops the query string, which is where signed URLs keep their secrets.
fn loggable(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
pub mod testing {
    //! A tiny scripted HTTP server for tests. Plain HTTP on localhost only.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    pub struct Scripted {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    pub fn reply(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Scripted {
        Scripted {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    /// Serves `script` in order, then closes. Returns the base URL and every request head seen.
    pub fn serve(script: Vec<Scripted>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for step in script {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut sink = vec![0; length];
                let _ = reader.read_exact(&mut sink);
                log.lock().unwrap().push(head);
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    step.status,
                    step.body.len()
                );
                for (key, value) in &step.headers {
                    out.push_str(&format!("{key}: {value}\r\n"));
                }
                out.push_str("\r\n");
                let _ = stream.write_all(out.as_bytes());
                let _ = stream.write_all(&step.body);
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    /// A client that may talk plain HTTP and barely waits between retries.
    pub fn client(token: Option<&str>) -> Http {
        let agent = Agent::config_builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Http::with_agent(agent, token.map(str::to_string), 0.001)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{client, reply, serve};
    use super::*;

    #[test]
    fn retry_rules_follow_the_status_and_headers() {
        let none = RateHints::default();
        assert_eq!(retry_delay(404, none, 0, 0), None);
        assert_eq!(
            retry_delay(501, none, 0, 0),
            None,
            "501 means unsupported, so don't retry"
        );
        assert_eq!(
            retry_delay(403, none, 0, 0),
            None,
            "a plain 403 is a real refusal"
        );
        assert_eq!(retry_delay(502, none, 0, 0), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(502, none, 3, 0), Some(Duration::from_secs(8)));
        assert_eq!(retry_delay(429, none, 1, 0), Some(Duration::from_secs(2)));
        let after = RateHints {
            retry_after: Some(30),
            ..none
        };
        assert_eq!(retry_delay(403, after, 0, 0), Some(Duration::from_secs(30)));
        let reset = RateHints {
            remaining: Some(0),
            reset: Some(1100),
            ..none
        };
        assert_eq!(
            retry_delay(403, reset, 0, 1000),
            Some(Duration::from_secs(102))
        );
        let far = RateHints {
            remaining: Some(0),
            reset: Some(10_000_000),
            ..none
        };
        assert_eq!(
            retry_delay(403, far, 0, 0),
            Some(Duration::from_secs(MAX_RATE_WAIT_SECS))
        );
        let past = RateHints {
            remaining: Some(0),
            reset: Some(10),
            ..none
        };
        assert_eq!(
            retry_delay(403, past, 0, 1000),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn retries_bad_gateway_then_succeeds() {
        let (base, seen) = serve(vec![
            reply(502, &[], b"bad"),
            reply(503, &[], b"bad"),
            reply(200, &[("etag", "\"abc\"")], b"ok"),
        ]);
        let http = client(None);
        let got = http.get(Channel::Raw, &format!("{base}/file")).unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"ok");
        assert_eq!(got.etag.as_deref(), Some("\"abc\""));
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(http.counts()[Channel::Raw.index()].1, 3);
    }

    #[test]
    fn waits_out_a_rate_limit_then_succeeds() {
        let (base, seen) = serve(vec![
            reply(
                403,
                &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1")],
                b"limit",
            ),
            reply(429, &[("retry-after", "1")], b"slow down"),
            reply(200, &[], b"ok"),
        ]);
        let got = client(None)
            .get(Channel::Search, &format!("{base}/x"))
            .unwrap();
        assert_eq!(got.body, b"ok");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn gives_up_after_too_many_failures() {
        let script = (0..MAX_ATTEMPTS).map(|_| reply(502, &[], b"")).collect();
        let (base, seen) = serve(script);
        let error = client(None)
            .get(Channel::Raw, &format!("{base}/x"))
            .unwrap_err();
        assert_eq!(error, FetchError::Status(502));
        assert_eq!(seen.lock().unwrap().len(), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn not_found_is_not_retried() {
        let (base, seen) = serve(vec![reply(404, &[], b"nope")]);
        let error = client(None)
            .get(Channel::Raw, &format!("{base}/x"))
            .unwrap_err();
        assert!(error.is_not_found());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn token_only_goes_to_the_github_api() {
        let (base, seen) = serve(vec![
            reply(200, &[], b"a"),
            reply(200, &[], b"b"),
            reply(200, &[], b"c"),
            reply(200, &[], b"d"),
        ]);
        let http = client(Some("secret"));
        for channel in [Channel::Raw, Channel::Asset, Channel::Wowi] {
            http.get(channel, &format!("{base}/x")).unwrap();
        }
        http.post_json(Channel::GraphQl, &format!("{base}/graphql"), "{}")
            .unwrap();
        let heads = seen.lock().unwrap();
        for head in &heads[..3] {
            assert!(
                !head.to_ascii_lowercase().contains("authorization"),
                "{head}"
            );
        }
        assert!(heads[3].contains("Bearer secret"), "{}", heads[3]);
        for head in heads.iter() {
            assert!(head.contains(USER_AGENT), "{head}");
        }
    }

    #[test]
    fn body_limit_is_enforced_without_retrying() {
        let (base, seen) = serve(vec![reply(200, &[], &[0u8; 2048])]);
        let error = client(None).get_with(Channel::Direct, &format!("{base}/x"), &[], 1024);
        assert_eq!(error.unwrap_err(), FetchError::TooLarge);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn connection_failures_are_retried_a_few_times_then_reported() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let http = client(None);
        let error = http
            .get(Channel::Raw, &format!("http://127.0.0.1:{port}/x"))
            .unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)), "{error:?}");
        assert_eq!(
            http.counts()[Channel::Raw.index()].1,
            u64::from(MAX_CONNECTION_ATTEMPTS)
        );
    }

    #[test]
    fn logs_never_show_query_strings() {
        assert_eq!(
            loggable("https://host/file?sig=secret"),
            "https://host/file"
        );
    }
}
