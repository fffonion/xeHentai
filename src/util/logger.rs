//! Reference-compatible logger.
//!
//! Levels and the two output formats come from `xeHentai/util/logger.py`:
//! stdout uses `"%-4s - [%X] %s\n"`, the log file uses
//! `"[%b %d %X] %s\n"` in the fixed GMT+8 timezone.
//!
//! The whole reference level/method surface is kept even before every caller
//! lands, so unused entries are expected in the foundation milestone.
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset, Utc};

pub const CRITICAL: i64 = 5;
pub const FATAL: i64 = CRITICAL;
pub const ERROR: i64 = 4;
pub const WARNING: i64 = 3;
pub const WARN: i64 = WARNING;
pub const INFO: i64 = 2;
pub const DEBUG: i64 = 1;
pub const VERBOSE: i64 = 0;

/// Query keys whose values never reach a log line. The comparison is
/// case-insensitive on the percent-decoded key.
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "code",
    "cookie",
    "hash",
    "id_token",
    "key",
    "passwd",
    "password",
    "pwd",
    "refresh_token",
    "secret",
    "session",
    "session_id",
    "sessionid",
    "sig",
    "signature",
    "token",
];

struct LoggerInner {
    level: i64,
    logf: Option<File>,
    out: Box<dyn Write + Send>,
    is_tty: bool,
}

/// Reference-compatible logger whose sink is shared by cheap clones. Cloning
/// is how a `Session` reaches the single App logger without disjoint verbosity
/// state, duplicated files or interleaved buffers.
pub struct Logger {
    inner: Arc<Mutex<LoggerInner>>,
}

impl Clone for Logger {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Logger {
    fn inner(&self) -> std::sync::MutexGuard<'_, LoggerInner> {
        // A panic while formatting one line must not silence every later line.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn new() -> Self {
        let stdout = std::io::stdout();
        let is_tty = stdout.is_terminal();
        Self {
            inner: Arc::new(Mutex::new(LoggerInner {
                level: VERBOSE,
                logf: None,
                out: Box::new(stdout),
                is_tty,
            })),
        }
    }

    pub fn with_output(out: Box<dyn Write + Send>, is_tty: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LoggerInner {
                level: VERBOSE,
                logf: None,
                out,
                is_tty,
            })),
        }
    }

    /// Reference `set_level`: disables the first `level` methods.
    pub fn set_level(&mut self, level: i64) {
        self.inner().level = level.clamp(0, 3);
    }

    /// Apply the CLI/config verbosity using the reference expression
    /// `WARNING - log_verbose`.
    pub fn set_verbosity(&mut self, verbose: i64) {
        self.set_level(WARNING - verbose);
    }

    pub fn set_logfile(&mut self, path: &Path) -> std::io::Result<()> {
        let mut inner = self.inner();
        // The reference drops the old file before reopening; a failed open
        // therefore leaves the logger without a file, not with a stale one.
        inner.logf = None;
        inner.logf = Some(OpenOptions::new().create(true).append(true).open(path)?);
        Ok(())
    }

    pub fn cleanup(&mut self) {
        self.inner().logf = None;
    }

    /// The identity of the log file this logger actually holds open, or `None`
    /// when no log file is open. It is `(device, inode)` on Unix and
    /// `(volume serial number, file index)` on Windows, matching the gallery
    /// reader's `FileId`.
    ///
    /// The descriptor is only borrowed for its metadata: ownership, output
    /// format and lifetime stay exactly as before. A metadata failure is
    /// reported instead of smoothed over, so the caller can fail closed rather
    /// than invent or forget a private identity.
    #[cfg(unix)]
    pub(crate) fn held_file_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        use std::os::unix::fs::MetadataExt;
        let inner = self.inner();
        match &inner.logf {
            None => Ok(None),
            Some(file) => {
                let metadata = file.metadata()?;
                Ok(Some((metadata.dev(), metadata.ino())))
            }
        }
    }

    /// Windows identity comes from the same pinned `cap_primitives` handle
    /// metadata the gallery reader uses: `Metadata::from_file` plus the
    /// doc-hidden `_WindowsByHandle` accessors. Both halves must be present;
    /// a half-known identity is an error, never a fabricated zero.
    #[cfg(windows)]
    pub(crate) fn held_file_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        use cap_primitives::fs::_WindowsByHandle as _;
        let inner = self.inner();
        match &inner.logf {
            None => Ok(None),
            Some(file) => {
                let metadata = cap_primitives::fs::Metadata::from_file(file)?;
                match (metadata.volume_serial_number(), metadata.file_index()) {
                    (Some(volume), Some(index)) => Ok(Some((u64::from(volume), index))),
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "the held log file identity is unavailable",
                    )),
                }
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn held_file_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the held log file identity is not available on this target",
        ))
    }

    fn enabled(&self, level: i64) -> bool {
        let inner = self.inner();
        match level {
            VERBOSE => inner.level < 1,
            DEBUG => inner.level < 2,
            INFO => inner.level < 3,
            _ => true,
        }
    }

    pub fn verbose(&mut self, message: &str) -> std::io::Result<()> {
        if !self.enabled(VERBOSE) {
            return Ok(());
        }
        self.log_at(now_gmt8(), "VERB", message)
    }

    pub fn debug(&mut self, message: &str) -> std::io::Result<()> {
        if !self.enabled(DEBUG) {
            return Ok(());
        }
        self.log_at(now_gmt8(), "DEBG", message)
    }

    pub fn info(&mut self, message: &str) -> std::io::Result<()> {
        if !self.enabled(INFO) {
            return Ok(());
        }
        self.log_at(now_gmt8(), "INFO", message)
    }

    pub fn warning(&mut self, message: &str) -> std::io::Result<()> {
        self.log_at(now_gmt8(), "WARN", message)
    }

    pub fn warn(&mut self, message: &str) -> std::io::Result<()> {
        self.warning(message)
    }

    pub fn error(&mut self, message: &str) -> std::io::Result<()> {
        self.log_at(now_gmt8(), "ERROR", message)
    }

    pub fn critical(&mut self, message: &str) -> std::io::Result<()> {
        self.log_at(now_gmt8(), "CRITICAL", message)
    }

    /// Write one record at an explicit time; the convenience methods use the
    /// current GMT+8 time.
    pub fn log_at(
        &mut self,
        now: DateTime<FixedOffset>,
        label: &str,
        message: &str,
    ) -> std::io::Result<()> {
        let mut inner = self.inner();
        let color = if inner.is_tty {
            match label {
                "ERROR" | "CRITICAL" => Some("31"),
                "WARN" => Some("33"),
                "DEBG" => Some("32"),
                "VERB" => Some("36"),
                _ => None,
            }
        } else {
            None
        };
        if let Some(color) = color {
            let _ = write!(inner.out, "\u{1b}[{color}m");
        }
        let line = format!("{:<4} - [{}] {}\n", label, now.format("%H:%M:%S"), message);
        // The reference ignores console write errors (for example a closed
        // Windows console) and keeps the line order.
        let _ = inner.out.write_all(line.as_bytes());
        let _ = inner.out.flush();
        if color.is_some() {
            let _ = inner.out.write_all("\u{1b}[0m".as_bytes());
        }
        if let Some(file) = &mut inner.logf {
            let record = format!("[{}] {}\n", now.format("%b %d %H:%M:%S"), message);
            file.write_all(record.as_bytes())?;
        }
        Ok(())
    }
}

/// Return a log-safe form of `raw`: no userinfo, no fragment and no value for
/// a sensitive query key. Anything unparseable becomes a fixed placeholder so
/// an embedded credential can never be echoed just because the URL was bad.
pub fn safe_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return "<unparseable-url>".to_string();
    };
    let _ = url.set_password(None);
    let _ = url.set_username("");
    url.set_fragment(None);
    if let Some(query) = url.query().map(redact_query) {
        url.set_query(Some(&query));
    }
    url.to_string()
}

fn redact_query(query: &str) -> String {
    let mut out = String::new();
    for (index, segment) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        let Some((key, _)) = segment.split_once('=') else {
            out.push_str(segment);
            continue;
        };
        let decoded = percent_encoding::percent_decode_str(key)
            .decode_utf8_lossy()
            .to_ascii_lowercase();
        if SENSITIVE_QUERY_KEYS.contains(&decoded.as_str()) {
            out.push_str(key);
            out.push_str("=REDACTED");
        } else {
            out.push_str(segment);
        }
    }
    out
}

pub fn now_gmt8() -> DateTime<FixedOffset> {
    let offset = FixedOffset::east_opt(8 * 3600).expect("GMT+8 offset");
    Utc::now().with_timezone(&offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fixed() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-31T23:04:05+08:00").unwrap()
    }

    fn logger() -> (Logger, Sink) {
        let sink = Sink(Arc::new(Mutex::new(Vec::new())));
        (Logger::with_output(Box::new(sink.clone()), false), sink)
    }

    fn text(sink: &Sink) -> String {
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn stdout_format_matches_the_reference() {
        let (mut logger, sink) = logger();
        logger.log_at(fixed(), "INFO", "hello world").unwrap();
        logger.log_at(fixed(), "VERB", "v").unwrap();
        logger.log_at(fixed(), "DEBG", "d").unwrap();
        logger.log_at(fixed(), "WARN", "w").unwrap();
        logger.log_at(fixed(), "ERROR", "e").unwrap();
        logger.log_at(fixed(), "CRITICAL", "c").unwrap();
        assert_eq!(
            text(&sink),
            "INFO - [23:04:05] hello world\n\
             VERB - [23:04:05] v\n\
             DEBG - [23:04:05] d\n\
             WARN - [23:04:05] w\n\
             ERROR - [23:04:05] e\n\
             CRITICAL - [23:04:05] c\n"
        );
    }

    #[test]
    fn log_file_format_is_gmt8_with_month_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("eh.log");
        let (mut logger, _sink) = logger();
        logger.set_logfile(&path).unwrap();
        logger.log_at(fixed(), "INFO", "first").unwrap();
        logger.log_at(fixed(), "ERROR", "second").unwrap();
        logger.cleanup();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[Oct 31 23:04:05] first\n[Oct 31 23:04:05] second\n"
        );
    }

    #[test]
    fn set_level_disables_verbose_then_debug_then_info() {
        for (level, expected) in [
            (0, vec!["VERB", "DEBG", "INFO", "WARN", "ERROR"]),
            (1, vec!["DEBG", "INFO", "WARN", "ERROR"]),
            (2, vec!["INFO", "WARN", "ERROR"]),
            (3, vec!["WARN", "ERROR"]),
            (9, vec!["WARN", "ERROR"]),
            (-4, vec!["VERB", "DEBG", "INFO", "WARN", "ERROR"]),
        ] {
            let (mut logger, sink) = logger();
            logger.set_level(level);
            logger.verbose("x").unwrap();
            logger.debug("x").unwrap();
            logger.info("x").unwrap();
            logger.warning("x").unwrap();
            logger.error("x").unwrap();
            let labels: Vec<String> = text(&sink)
                .lines()
                .map(|line| line[..line.find(' ').unwrap()].to_string())
                .collect();
            assert_eq!(labels, expected, "level {level}");
        }
    }

    #[test]
    fn verbosity_uses_the_reference_warning_minus_value_expression() {
        let (mut logger, sink) = logger();
        logger.set_verbosity(2);
        logger.verbose("x").unwrap();
        logger.debug("x").unwrap();
        logger.info("x").unwrap();
        let labels: Vec<String> = text(&sink)
            .lines()
            .map(|line| line[..line.find(' ').unwrap()].to_string())
            .collect();
        assert_eq!(
            labels,
            ["DEBG", "INFO"],
            "WARNING - 2 disables only VERB, matching core.py"
        );
    }

    #[test]
    fn tty_output_uses_the_reference_ansi_colors() {
        let sink = Sink(Arc::new(Mutex::new(Vec::new())));
        let mut logger = Logger::with_output(Box::new(sink.clone()), true);
        logger.log_at(fixed(), "ERROR", "bad").unwrap();
        logger.log_at(fixed(), "WARN", "warn").unwrap();
        logger.log_at(fixed(), "INFO", "neutral").unwrap();
        assert_eq!(
            text(&sink),
            "\u{1b}[31mERROR - [23:04:05] bad\n\u{1b}[0m\
             \u{1b}[33mWARN - [23:04:05] warn\n\u{1b}[0m\
             INFO - [23:04:05] neutral\n"
        );
    }

    /// The getter reports the descriptor the logger actually holds, not the
    /// current path spelling: after the private name is retargeted the logger
    /// keeps appending to the original file.
    #[cfg(unix)]
    #[test]
    fn held_file_identity_tracks_the_open_descriptor_not_the_spelling() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("live.log");
        let (mut logger, _sink) = logger();
        logger.set_logfile(&path).unwrap();
        let held = logger
            .held_file_identity()
            .unwrap()
            .expect("an open logger must report its held file");
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(held, (metadata.dev(), metadata.ino()));

        let other = root.path().join("other.log");
        std::fs::write(&other, b"OTHER").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert_eq!(
            logger.held_file_identity().unwrap(),
            Some(held),
            "the held descriptor, not the new spelling, is the private object"
        );
        logger.log_at(fixed(), "INFO", "after retarget").unwrap();
        assert_eq!(std::fs::read(&other).unwrap(), b"OTHER");
        logger.cleanup();
        assert_eq!(logger.held_file_identity().unwrap(), None);
    }

    #[test]
    fn clone_shares_the_level_and_the_output_sink() {
        let (mut logger, sink) = logger();
        logger.set_verbosity(3);
        let mut shared = logger.clone();
        shared.verbose("from the clone").unwrap();
        logger.set_verbosity(2);
        shared.verbose("hidden").unwrap();
        logger.verbose("also hidden").unwrap();
        let text = text(&sink);
        assert!(text.contains("VERB - ["), "{text}");
        assert!(text.contains("from the clone"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn safe_url_strips_userinfo_fragment_and_sensitive_query_values() {
        assert_eq!(
            safe_url(
                "https://alice:s3cret@exhentai.org/g/123/abcdef0123/?page=2&token=abc#access_token=zzz"
            ),
            "https://exhentai.org/g/123/abcdef0123/?page=2&token=REDACTED"
        );
        assert_eq!(
            safe_url("http://user:pass@proxy.test:8080/path?API_KEY=top-secret&b=4"),
            "http://proxy.test:8080/path?API_KEY=REDACTED&b=4"
        );
        assert_eq!(
            safe_url("https://e-hentai.org/s/abc123/1-2?p=keystamp%3D1&sig=deadbeef"),
            "https://e-hentai.org/s/abc123/1-2?p=keystamp%3D1&sig=REDACTED"
        );
        assert_eq!(
            safe_url("https://example.org/plain"),
            "https://example.org/plain"
        );
        assert_eq!(safe_url("not a url"), "<unparseable-url>");
        assert!(!safe_url("https://u:p@example.org/?password=p").contains("p@"));
    }

    #[test]
    fn safe_url_never_echoes_a_percent_encoded_sensitive_key() {
        let safe = safe_url("https://example.org/?%74oken=leak&page=1");
        assert_eq!(safe, "https://example.org/?%74oken=REDACTED&page=1");
        assert!(!safe.contains("leak"), "{safe}");
    }
}
