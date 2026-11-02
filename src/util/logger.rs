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

use chrono::{DateTime, FixedOffset, Utc};

pub const CRITICAL: i64 = 5;
pub const FATAL: i64 = CRITICAL;
pub const ERROR: i64 = 4;
pub const WARNING: i64 = 3;
pub const WARN: i64 = WARNING;
pub const INFO: i64 = 2;
pub const DEBUG: i64 = 1;
pub const VERBOSE: i64 = 0;

pub struct Logger {
    level: i64,
    logf: Option<File>,
    out: Box<dyn Write + Send>,
    is_tty: bool,
}

impl Logger {
    pub fn new() -> Self {
        let stdout = std::io::stdout();
        let is_tty = stdout.is_terminal();
        Self {
            level: VERBOSE,
            logf: None,
            out: Box::new(stdout),
            is_tty,
        }
    }

    pub fn with_output(out: Box<dyn Write + Send>, is_tty: bool) -> Self {
        Self {
            level: VERBOSE,
            logf: None,
            out,
            is_tty,
        }
    }

    /// Reference `set_level`: disables the first `level` methods.
    pub fn set_level(&mut self, level: i64) {
        self.level = level.clamp(0, 3);
    }

    /// Apply the CLI/config verbosity using the reference expression
    /// `WARNING - log_verbose`.
    pub fn set_verbosity(&mut self, verbose: i64) {
        self.set_level(WARNING - verbose);
    }

    pub fn set_logfile(&mut self, path: &Path) -> std::io::Result<()> {
        self.cleanup();
        self.logf = Some(OpenOptions::new().create(true).append(true).open(path)?);
        Ok(())
    }

    pub fn cleanup(&mut self) {
        self.logf = None;
    }

    /// The `(device, inode)` identity of the log file this logger actually
    /// holds open, or `None` when no log file is open.
    ///
    /// The descriptor is only borrowed for its metadata: ownership, output
    /// format and lifetime stay exactly as before. A metadata failure is
    /// reported instead of smoothed over, so the caller can fail closed rather
    /// than invent or forget a private identity.
    #[cfg(unix)]
    pub(crate) fn held_file_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        use std::os::unix::fs::MetadataExt;
        match &self.logf {
            None => Ok(None),
            Some(file) => {
                let metadata = file.metadata()?;
                Ok(Some((metadata.dev(), metadata.ino())))
            }
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn held_file_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the held log file identity is only available on unix targets",
        ))
    }

    fn enabled(&self, level: i64) -> bool {
        match level {
            VERBOSE => self.level < 1,
            DEBUG => self.level < 2,
            INFO => self.level < 3,
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
        let color = if self.is_tty {
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
            let _ = write!(self.out, "\u{1b}[{color}m");
        }
        let line = format!("{:<4} - [{}] {}\n", label, now.format("%H:%M:%S"), message);
        // The reference ignores console write errors (for example a closed
        // Windows console) and keeps the line order.
        let _ = self.out.write_all(line.as_bytes());
        let _ = self.out.flush();
        if color.is_some() {
            let _ = self.out.write_all("\u{1b}[0m".as_bytes());
        }
        if let Some(file) = &mut self.logf {
            let record = format!("[{}] {}\n", now.format("%b %d %H:%M:%S"), message);
            file.write_all(record.as_bytes())?;
        }
        Ok(())
    }
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
}
