//! Read and Seek over HTTP range requests, with a block cache, so the zip crate
//! can read a 90 MB archive's directory and a few TOCs with a handful of small
//! requests. Only explicit ranges are used, because GitHub answers suffix ranges
//! with 501, and the total size comes from the caller.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};

use crate::http::{Channel, FetchError, Http, ZIP_BODY};

const BLOCK: u64 = 32 * 1024;
/// The first read fetches this much from the end: the directory usually fits.
const TAIL: u64 = 64 * 1024;
/// Sequential reads double their request size up to this many blocks.
const MAX_RUN: u64 = 32;
/// A zip's directory and TOCs never need this much. Past it, something is wrong
/// and it's better to fail than to quietly download a whole archive.
const MAX_FETCHED: u64 = 64 * 1024 * 1024;

pub enum RangeReply {
    /// Exactly the requested bytes.
    Partial(Vec<u8>),
    /// The server ignored the range and sent the whole file.
    Whole(Vec<u8>),
}

pub trait RangeSource {
    /// Fetch `start..=end`.
    fn fetch(&mut self, start: u64, end: u64) -> io::Result<RangeReply>;
}

pub struct RangeReader<S> {
    source: S,
    size: u64,
    pos: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
    whole: Option<Vec<u8>>,
    fetched: u64,
    requests: u32,
    run: u64,
    next_in_run: Option<u64>,
}

impl<S: RangeSource> RangeReader<S> {
    pub fn new(source: S, size: u64) -> Self {
        Self {
            source,
            size,
            pos: 0,
            blocks: BTreeMap::new(),
            whole: None,
            fetched: 0,
            requests: 0,
            run: 1,
            next_in_run: None,
        }
    }

    #[cfg(test)]
    pub fn requests(&self) -> u32 {
        self.requests
    }

    #[cfg(test)]
    pub fn fetched(&self) -> u64 {
        self.fetched
    }

    fn load(&mut self, block: u64) -> io::Result<()> {
        let (first, count) = self.plan(block);
        let start = first * BLOCK;
        let end = ((first + count) * BLOCK).min(self.size) - 1;
        let wanted = end - start + 1;
        if self.fetched + wanted > MAX_FETCHED {
            return Err(io::Error::other(
                "the zip needs too many range requests to read",
            ));
        }
        self.requests += 1;
        match self.source.fetch(start, end)? {
            RangeReply::Whole(bytes) => {
                if bytes.len() as u64 != self.size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "expected {} bytes, the server sent {}",
                            self.size,
                            bytes.len()
                        ),
                    ));
                }
                self.fetched += self.size;
                self.whole = Some(bytes);
            }
            RangeReply::Partial(bytes) => {
                if bytes.len() as u64 != wanted {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected {wanted} bytes, the server sent {}", bytes.len()),
                    ));
                }
                self.fetched += wanted;
                let block_len = usize::try_from(BLOCK).unwrap_or(usize::MAX);
                for (i, chunk) in bytes.chunks(block_len).enumerate() {
                    self.blocks.insert(first + i as u64, chunk.to_vec());
                }
            }
        }
        Ok(())
    }

    /// Which blocks to fetch for a miss on `block`, as `(first, count)`.
    fn plan(&mut self, block: u64) -> (u64, u64) {
        let total = self.size.div_ceil(BLOCK);
        if self.requests == 0 && self.size.saturating_sub(block * BLOCK) <= TAIL {
            let first = self.size.saturating_sub(TAIL) / BLOCK;
            self.next_in_run = Some(total);
            return (first, total - first);
        }
        self.run = if self.next_in_run == Some(block) {
            (self.run * 2).min(MAX_RUN)
        } else {
            1
        };
        let mut count = 1;
        while count < self.run
            && block + count < total
            && !self.blocks.contains_key(&(block + count))
        {
            count += 1;
        }
        self.next_in_run = Some(block + count);
        (block, count)
    }
}

impl<S: RangeSource> Read for RangeReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let block = self.pos / BLOCK;
        if self.whole.is_none() && !self.blocks.contains_key(&block) {
            self.load(block)?;
        }
        let available: &[u8] = if let Some(whole) = &self.whole {
            let at = usize::try_from(self.pos).map_err(io::Error::other)?;
            &whole[at..]
        } else {
            let data = self
                .blocks
                .get(&block)
                .ok_or_else(|| io::Error::other("range cache lost a block"))?;
            let at = usize::try_from(self.pos % BLOCK).map_err(io::Error::other)?;
            data.get(at..).unwrap_or(&[])
        };
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<S: RangeSource> Seek for RangeReader<S> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => self.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the zip",
            )
        })?;
        self.pos = target;
        Ok(target)
    }
}

/// A [`RangeSource`] backed by real HTTP.
pub struct HttpRange<'a> {
    http: &'a Http,
    channel: Channel,
    url: &'a str,
    size: Option<u64>,
}

impl<'a> HttpRange<'a> {
    /// `size`, when known, is checked against what the server reports.
    pub fn new(http: &'a Http, channel: Channel, url: &'a str, size: Option<u64>) -> Self {
        Self {
            http,
            channel,
            url,
            size,
        }
    }

    /// Learn a file's size with a one-byte range. `Err` carries the failure, and a
    /// server that ignores ranges simply reports the whole body's length.
    pub fn probe_size(http: &Http, channel: Channel, url: &str) -> Result<u64, FetchError> {
        let reply = http.get_with(
            channel,
            url,
            &[("range", "bytes=0-0"), ("accept-encoding", "identity")],
            ZIP_BODY,
        )?;
        if reply.status == 206 {
            return reply
                .content_range
                .as_deref()
                .and_then(parse_content_range)
                .map(|(_, _, total)| total)
                .ok_or_else(|| FetchError::Transport("no usable Content-Range".to_string()));
        }
        Ok(reply.body.len() as u64)
    }
}

/// How much of a reply to read. A server that ignores the range sends the whole file, which
/// is only any use when the file is small enough to keep. For a bigger one, everything past
/// the range is waste, and reading it would cost up to `ZIP_BODY` before failing anyway. One
/// byte over the range, because the end of a reply is only seen by trying to read past it.
fn reply_limit(start: u64, end: u64, size: Option<u64>) -> u64 {
    match size {
        Some(size) if size > ZIP_BODY => (end - start + 1).saturating_add(1),
        _ => ZIP_BODY,
    }
}

impl RangeSource for HttpRange<'_> {
    fn fetch(&mut self, start: u64, end: u64) -> io::Result<RangeReply> {
        let range = format!("bytes={start}-{end}");
        // Compression would make the byte offsets meaningless.
        let headers = [("range", range.as_str()), ("accept-encoding", "identity")];
        let limit = reply_limit(start, end, self.size);
        let reply = self
            .http
            .get_with(self.channel, self.url, &headers, limit)
            .map_err(io::Error::other)?;
        match reply.status {
            200 => Ok(RangeReply::Whole(reply.body)),
            206 => {
                let parsed = reply.content_range.as_deref().and_then(parse_content_range);
                let Some((got_start, got_end, total)) = parsed else {
                    return Err(io::Error::other(
                        "the server sent a range without a Content-Range",
                    ));
                };
                if got_start != start || got_end != end {
                    return Err(io::Error::other(format!(
                        "asked for bytes {start}-{end}, got {got_start}-{got_end}"
                    )));
                }
                if self.size.is_some_and(|size| size != total) {
                    return Err(io::Error::other("the file changed size while being read"));
                }
                Ok(RangeReply::Partial(reply.body))
            }
            other => Err(io::Error::other(format!(
                "HTTP {other} for a range request"
            ))),
        }
    }
}

/// `bytes 0-99/1234` gives `(0, 99, 1234)`.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;
    use crate::http::testing::{client, reply, serve};
    use crate::zipscan::scan_zip;

    struct Memory {
        data: Vec<u8>,
        calls: Vec<(u64, u64)>,
    }

    impl RangeSource for Memory {
        fn fetch(&mut self, start: u64, end: u64) -> io::Result<RangeReply> {
            self.calls.push((start, end));
            let (start, end) = (start as usize, end as usize);
            Ok(RangeReply::Partial(self.data[start..=end].to_vec()))
        }
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn big_zip(payload: usize) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("Big/Big.toc", stored).unwrap();
        writer
            .write_all(b"## Interface: 16001\n## Title: Big\n")
            .unwrap();
        writer.start_file("Big/data.bin", stored).unwrap();
        writer.write_all(&vec![7u8; payload]).unwrap();
        writer.start_file("Other/Other.toc", stored).unwrap();
        writer
            .write_all(b"## Interface: 120105\n## Title: Other\n")
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_and_seeks_like_a_file() {
        let data = sample(300_000);
        let mut reader = RangeReader::new(
            Memory {
                data: data.clone(),
                calls: vec![],
            },
            300_000,
        );
        let mut buf = [0u8; 100];
        reader.seek(SeekFrom::Start(70_000)).unwrap();
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[70_000..70_100]);
        reader.seek(SeekFrom::End(-10)).unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[299_990..]);
        assert_eq!(reader.seek(SeekFrom::Current(-20)).unwrap(), 299_980);
        assert!(reader.seek(SeekFrom::Current(-400_000)).is_err());
        reader.seek(SeekFrom::Start(500_000)).unwrap();
        assert_eq!(
            reader.read(&mut buf).unwrap(),
            0,
            "reading past the end is EOF"
        );
    }

    #[test]
    fn repeated_reads_come_from_the_cache() {
        let data = sample(200_000);
        let mut reader = RangeReader::new(
            Memory {
                data,
                calls: vec![],
            },
            200_000,
        );
        let mut buf = [0u8; 16];
        for _ in 0..5 {
            reader.seek(SeekFrom::Start(1000)).unwrap();
            reader.read_exact(&mut buf).unwrap();
        }
        assert_eq!(reader.requests(), 1);
    }

    #[test]
    fn a_large_zip_takes_only_a_few_small_requests() {
        let data = big_zip(40 * 1024 * 1024);
        let size = data.len() as u64;
        let mut reader = RangeReader::new(
            Memory {
                data,
                calls: vec![],
            },
            size,
        );
        let scan = scan_zip(&mut reader, &["Big"]).unwrap();
        assert_eq!(scan.folders, ["Big", "Other"]);
        assert!(scan.forever);
        assert_eq!(scan.title.as_deref(), Some("Big"));
        assert!(
            reader.requests() <= 4,
            "took {} requests",
            reader.requests()
        );
        assert!(
            reader.fetched() < 512 * 1024,
            "fetched {} bytes",
            reader.fetched()
        );
    }

    #[test]
    fn never_asks_for_a_suffix_range_or_past_the_end() {
        let data = big_zip(1024 * 1024);
        let size = data.len() as u64;
        let mut reader = RangeReader::new(
            Memory {
                data,
                calls: vec![],
            },
            size,
        );
        scan_zip(&mut reader, &["Big"]).unwrap();
        for &(start, end) in &reader.source.calls {
            assert!(start <= end, "{start}-{end}");
            assert!(end < size, "{end} is past the end of {size}");
        }
    }

    #[test]
    fn a_server_that_ignores_ranges_still_works() {
        struct Ignores(Vec<u8>);
        impl RangeSource for Ignores {
            fn fetch(&mut self, _: u64, _: u64) -> io::Result<RangeReply> {
                Ok(RangeReply::Whole(self.0.clone()))
            }
        }
        let data = big_zip(1000);
        let size = data.len() as u64;
        let mut reader = RangeReader::new(Ignores(data), size);
        let scan = scan_zip(&mut reader, &["Big"]).unwrap();
        assert_eq!(scan.folders, ["Big", "Other"]);
        assert_eq!(reader.requests(), 1);
    }

    #[test]
    fn short_replies_and_size_mismatches_are_errors() {
        struct Short;
        impl RangeSource for Short {
            fn fetch(&mut self, _: u64, _: u64) -> io::Result<RangeReply> {
                Ok(RangeReply::Partial(vec![1, 2, 3]))
            }
        }
        let mut reader = RangeReader::new(Short, 100_000);
        let mut buf = [0u8; 16];
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        struct WrongWhole;
        impl RangeSource for WrongWhole {
            fn fetch(&mut self, _: u64, _: u64) -> io::Result<RangeReply> {
                Ok(RangeReply::Whole(vec![1, 2, 3]))
            }
        }
        let mut reader = RangeReader::new(WrongWhole, 100_000);
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn source_errors_reach_the_zip_reader_as_scan_failures() {
        struct Down;
        impl RangeSource for Down {
            fn fetch(&mut self, _: u64, _: u64) -> io::Result<RangeReply> {
                Err(io::Error::other("HTTP 503"))
            }
        }
        let reader = RangeReader::new(Down, 1_000_000);
        assert!(scan_zip(reader, &["x"]).is_err());
    }

    #[test]
    fn stops_before_downloading_a_whole_archive() {
        struct Endless;
        impl RangeSource for Endless {
            fn fetch(&mut self, start: u64, end: u64) -> io::Result<RangeReply> {
                Ok(RangeReply::Partial(vec![0; (end - start + 1) as usize]))
            }
        }
        let mut reader = RangeReader::new(Endless, 1 << 40);
        let mut sink = Vec::new();
        let error = reader.read_to_end(&mut sink).unwrap_err();
        assert!(
            error.to_string().contains("too many range requests"),
            "{error}"
        );
        assert!(reader.fetched() <= MAX_FETCHED);
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 99, 1234)));
        assert_eq!(parse_content_range("bytes 5-5/*"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn http_range_checks_what_the_server_sent() {
        let (base, seen) = serve(vec![
            reply(206, &[("content-range", "bytes 0-3/10")], b"abcd"),
            reply(206, &[("content-range", "bytes 1-4/10")], b"abcd"),
            reply(206, &[], b"abcd"),
            reply(206, &[("content-range", "bytes 0-3/99")], b"abcd"),
            reply(200, &[], b"everything"),
            reply(501, &[], b"Unsupported client range"),
        ]);
        let http = client(None);
        let url = format!("{base}/a.zip");
        let mut source = HttpRange::new(&http, Channel::Asset, &url, Some(10));
        assert!(matches!(source.fetch(0, 3), Ok(RangeReply::Partial(b)) if b == b"abcd"));
        assert!(source.fetch(0, 3).is_err(), "wrong range");
        assert!(source.fetch(0, 3).is_err(), "no Content-Range");
        assert!(source.fetch(0, 3).is_err(), "file changed size");
        assert!(matches!(source.fetch(0, 3), Ok(RangeReply::Whole(b)) if b == b"everything"));
        assert!(source.fetch(0, 3).is_err(), "501 surfaces as an error");
        let heads = seen.lock().unwrap();
        assert!(
            heads[0].to_ascii_lowercase().contains("range: bytes=0-3"),
            "{}",
            heads[0]
        );
        assert!(
            heads[0]
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"),
            "{}",
            heads[0]
        );
    }

    #[test]
    fn a_server_ignoring_ranges_on_a_huge_file_costs_about_the_range_not_the_file() {
        let huge = ZIP_BODY + 1;
        assert_eq!(reply_limit(0, 1023, Some(huge)), 1025);
        assert_eq!(
            reply_limit(0, 1023, Some(10)),
            ZIP_BODY,
            "a small file may still come whole"
        );
        assert_eq!(reply_limit(0, 1023, None), ZIP_BODY);

        let total = format!("bytes 100-103/{huge}");
        let (base, _) = serve(vec![
            reply(206, &[("content-range", total.as_str())], b"abcd"),
            reply(200, &[], &vec![7u8; 2 * 1024 * 1024]),
        ]);
        let http = client(None);
        let url = format!("{base}/pack.zip");
        let mut source = HttpRange::new(&http, Channel::Asset, &url, Some(huge));
        assert!(
            matches!(source.fetch(100, 103), Ok(RangeReply::Partial(b)) if b == b"abcd"),
            "exactly the range fits the limit"
        );
        let error = source
            .fetch(0, 1023)
            .err()
            .expect("the whole file is refused");
        assert!(error.to_string().contains("larger than allowed"), "{error}");
        assert_eq!(http.bytes(), 4, "nothing past the range was kept");
    }

    #[test]
    fn probing_size_handles_both_kinds_of_server() {
        let (base, _) = serve(vec![
            reply(206, &[("content-range", "bytes 0-0/4242")], b"x"),
            reply(200, &[], b"whole file"),
            reply(404, &[], b""),
        ]);
        let http = client(None);
        let url = format!("{base}/a.zip");
        assert_eq!(
            HttpRange::probe_size(&http, Channel::Direct, &url),
            Ok(4242)
        );
        assert_eq!(HttpRange::probe_size(&http, Channel::Direct, &url), Ok(10));
        assert_eq!(
            HttpRange::probe_size(&http, Channel::Direct, &url),
            Err(FetchError::Status(404))
        );
    }
}
