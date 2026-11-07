//! A tiny rotating file logger: one line per record in `app.log`, rotated to
//! `app.old.log` at 1 MB. Logging must never be able to take the app down, so
//! every failure in here is swallowed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record};

use crate::paths;

const MAX_LOG_BYTES: u64 = 1024 * 1024;
const LOG_FILE: &str = "app.log";
const OLD_LOG_FILE: &str = "app.old.log";
/// Crates of this workspace. Everything else is capped at `Warn`, which keeps
/// the log small and keeps HTTP libraries from writing request headers into it.
const OUR_TARGETS: [&str; 2] = ["feeling", "wow_is_a_feeling"];

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

/// Install the logger. Safe to call more than once, and still fine if the log
/// file can't be opened: the app just runs without a log.
pub fn init(level: LevelFilter) {
    install(paths::log_dir(), level);
}

/// The last `n` log lines, oldest first, reaching back into the rotated file
/// when the current one is shorter. For the "copy diagnostics" button.
pub fn recent_lines(n: usize) -> Vec<String> {
    recent_lines_in(&paths::log_dir(), n)
}

fn install(dir: PathBuf, level: LevelFilter) {
    let logger = LOGGER.get_or_init(|| FileLogger::open(dir, level));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

fn recent_lines_in(dir: &Path, n: usize) -> Vec<String> {
    let mut lines = tail_of(&dir.join(LOG_FILE), n);
    if lines.len() < n {
        let mut older = tail_of(&dir.join(OLD_LOG_FILE), n - lines.len());
        older.append(&mut lines);
        lines = older;
    }
    lines
}

fn tail_of(path: &Path, n: usize) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut tail: Vec<String> = text.lines().rev().take(n).map(str::to_string).collect();
    tail.reverse();
    tail
}

struct FileLogger {
    level: LevelFilter,
    sink: Mutex<Sink>,
}

impl FileLogger {
    fn open(dir: PathBuf, level: LevelFilter) -> Self {
        Self {
            level,
            sink: Mutex::new(Sink::open(dir, MAX_LOG_BYTES)),
        }
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let ours = OUR_TARGETS.iter().any(|t| metadata.target().starts_with(t));
        let limit = if ours {
            self.level
        } else {
            self.level.min(LevelFilter::Warn)
        };
        metadata.level() <= limit
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Formatted before taking the lock, so a Display impl that logs can't deadlock.
        let line = format_line(unix_now(), record);
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_line(&line);
    }

    /// Nothing to do: `File` has no buffer, so each line reaches the OS as it's written.
    fn flush(&self) {}
}

struct Sink {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
}

impl Sink {
    fn open(dir: PathBuf, limit: u64) -> Self {
        let file = open_append(&dir);
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        Self {
            dir,
            file,
            size,
            limit,
        }
    }

    fn write_line(&mut self, line: &str) {
        let len = line.len() as u64;
        if self.size > 0 && self.size + len > self.limit {
            self.rotate();
        }
        if let Some(file) = &mut self.file
            && file.write_all(line.as_bytes()).is_ok()
        {
            self.size += len;
        }
    }

    fn rotate(&mut self) {
        // Windows won't rename a file that's still open.
        self.file = None;
        let current = self.dir.join(LOG_FILE);
        if fs::rename(&current, self.dir.join(OLD_LOG_FILE)).is_err() {
            // Something is holding the old log. Start over rather than grow without bound.
            let _ = fs::write(&current, b"");
        }
        self.file = open_append(&self.dir);
        self.size = 0;
    }
}

fn open_append(dir: &Path) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
        .ok()
}

/// One record as one line. Messages can quote untrusted text such as addon names,
/// so control characters become spaces and can't forge extra lines.
fn format_line(unix_seconds: u64, record: &Record) -> String {
    let message = record
        .args()
        .to_string()
        .replace(|c: char| c.is_control(), " ");
    format!(
        "{} {:<5} {}: {message}\n",
        format_utc(unix_seconds),
        record.level(),
        record.target()
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `2026-11-07 12:34:56Z`.
fn format_utc(unix_seconds: u64) -> String {
    let (year, month, day) = civil_from_days(unix_seconds / 86_400);
    let time = unix_seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// Days since 1970-01-01 to a calendar date, by Howard Hinnant's `civil_from_days`,
/// which counts in 400-year eras of March-based years so leap days fall last.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    fn record_line(message: &str) -> String {
        format_line(
            0,
            &Record::builder()
                .args(format_args!("{message}"))
                .level(Level::Info)
                .target("feeling_core::test")
                .build(),
        )
    }

    fn line_of(bytes: usize) -> String {
        let mut line = "x".repeat(bytes - 1);
        line.push('\n');
        line
    }

    #[test]
    fn utc_formatting_is_correct_across_leap_years_and_centuries() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(format_utc(1_709_251_199), "2024-02-29 23:59:59Z");
        assert_eq!(format_utc(1_794_000_000), "2026-11-06 21:20:00Z");
        assert_eq!(format_utc(4_102_444_799), "2099-12-31 23:59:59Z");
    }

    #[test]
    fn a_record_is_one_timestamped_line() {
        assert_eq!(
            record_line("hello"),
            "1970-01-01 00:00:00Z INFO  feeling_core::test: hello\n"
        );
    }

    #[test]
    fn newlines_in_a_message_cannot_forge_extra_lines() {
        let line = record_line("evil\r\n2026-01-01 00:00:00Z ERROR fake: boom");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("evil  2026-01-01"));
    }

    #[test]
    fn sink_appends_and_recent_lines_reads_them_back() {
        let temp = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(temp.path().join("logs"), 1024);
        sink.write_line("one\n");
        sink.write_line("two\n");
        sink.write_line("three\n");
        assert_eq!(
            recent_lines_in(&temp.path().join("logs"), 2),
            ["two", "three"]
        );
        assert_eq!(
            recent_lines_in(&temp.path().join("logs"), 10),
            ["one", "two", "three"]
        );
        assert!(recent_lines_in(&temp.path().join("logs"), 0).is_empty());
    }

    #[test]
    fn reopening_counts_the_existing_size_toward_rotation() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(LOG_FILE), line_of(90)).unwrap();
        let mut sink = Sink::open(temp.path().to_path_buf(), 100);
        sink.write_line(&line_of(20));
        assert!(temp.path().join(OLD_LOG_FILE).exists());
    }

    #[test]
    fn rotates_at_the_limit_and_keeps_only_one_old_file() {
        let temp = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(temp.path().to_path_buf(), 100);
        sink.write_line(&line_of(60));
        assert!(!temp.path().join(OLD_LOG_FILE).exists());

        sink.write_line(&line_of(60));
        assert_eq!(
            fs::metadata(temp.path().join(OLD_LOG_FILE)).unwrap().len(),
            60
        );
        assert_eq!(fs::metadata(temp.path().join(LOG_FILE)).unwrap().len(), 60);

        sink.write_line("third\n");
        sink.write_line(&line_of(60));
        // The first rotated file has been replaced, and the tail still spans both files.
        let old = fs::read_to_string(temp.path().join(OLD_LOG_FILE)).unwrap();
        assert!(old.ends_with("third\n"));
        assert_eq!(recent_lines_in(temp.path(), 2)[0], "third");
    }

    #[test]
    fn the_real_logger_rotates_at_one_megabyte() {
        let temp = tempfile::tempdir().unwrap();
        let logger = FileLogger::open(temp.path().to_path_buf(), LevelFilter::Info);
        let message = "x".repeat(1000);
        for _ in 0..1100 {
            logger.log(
                &Record::builder()
                    .args(format_args!("{message}"))
                    .level(Level::Info)
                    .target("feeling_core::test")
                    .build(),
            );
        }
        let old = fs::metadata(temp.path().join(OLD_LOG_FILE)).unwrap().len();
        let current = fs::metadata(temp.path().join(LOG_FILE)).unwrap().len();
        assert!(
            old <= MAX_LOG_BYTES && old > MAX_LOG_BYTES - 2000,
            "old log: {old}"
        );
        assert!(
            current > 0 && current < MAX_LOG_BYTES,
            "current log: {current}"
        );
    }

    #[test]
    fn a_single_oversized_line_is_still_written() {
        let temp = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(temp.path().to_path_buf(), 10);
        sink.write_line(&line_of(50));
        assert!(!temp.path().join(OLD_LOG_FILE).exists());
        assert_eq!(fs::metadata(temp.path().join(LOG_FILE)).unwrap().len(), 50);
    }

    #[test]
    fn an_unusable_log_folder_is_silently_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("not-a-folder");
        fs::write(&file, b"x").unwrap();

        let logger = FileLogger::open(file.join("logs"), LevelFilter::Trace);
        logger.log(
            &Record::builder()
                .args(format_args!("nowhere to go"))
                .level(Level::Error)
                .target("feeling_core")
                .build(),
        );
        assert!(recent_lines_in(&file.join("logs"), 5).is_empty());
    }

    #[test]
    fn missing_log_files_mean_no_lines() {
        let temp = tempfile::tempdir().unwrap();
        assert!(recent_lines_in(temp.path(), 5).is_empty());
    }

    #[test]
    fn invalid_utf8_in_a_log_file_does_not_lose_the_tail() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(LOG_FILE), b"\xff\xfe broken\nfine\n").unwrap();
        assert_eq!(recent_lines_in(temp.path(), 1), ["fine"]);
    }

    #[test]
    fn third_party_crates_are_capped_at_warn() {
        let temp = tempfile::tempdir().unwrap();
        let logger = FileLogger::open(temp.path().to_path_buf(), LevelFilter::Debug);
        let meta = |target, level| Metadata::builder().target(target).level(level).build();

        assert!(logger.enabled(&meta("feeling_core::net", Level::Debug)));
        assert!(logger.enabled(&meta("wow_is_a_feeling::ui", Level::Debug)));
        assert!(!logger.enabled(&meta("feeling_core::net", Level::Trace)));
        assert!(!logger.enabled(&meta("ureq::run", Level::Info)));
        assert!(logger.enabled(&meta("ureq::run", Level::Warn)));
    }

    #[test]
    fn the_global_logger_installs_once_and_writes_through_the_log_macros() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path().to_path_buf(), LevelFilter::Info);
        // A second install must be a harmless no-op.
        init(LevelFilter::Error);

        log::info!(target: "feeling_core::test", "through the facade");
        let lines = recent_lines_in(temp.path(), 50);
        assert!(lines.iter().any(|l| l.ends_with("through the facade")));

        // Close the file so the temp folder can be deleted on every OS.
        LOGGER
            .get()
            .unwrap()
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .file = None;
    }
}
