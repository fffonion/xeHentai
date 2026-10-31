//! Application entry point.
//!
//! This build implements the configuration, logging, task, session and file
//! foundation. It loads the configuration through the real RSS path, restores
//! the real session objects, and then reports honestly that downloading is not
//! implemented yet.

rust_i18n::i18n!("locales", fallback = "en_us");

mod config;
mod r#const;
mod core;
mod filters;
mod i18n;
mod proxy;
mod task;
mod util;
mod worker;

use std::path::Path;

use config::Config;
use util::logger::Logger;

fn main() {
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    std::process::exit(run(&root));
}

/// Run configuration/session initialization and return the process exit code.
///
/// Exit codes: 0 reserved for a complete run, 1 for configuration/log/session
/// setup errors, 2 because this foundation build does not implement
/// downloading.
pub fn run(root: &Path) -> i32 {
    let loaded = match Config::load(root) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("xeHentai: configuration error: {error}");
            return 1;
        }
    };

    let mut logger = Logger::new();
    logger.set_verbosity(loaded.config.log_verbose);
    let log_path = Path::new(&loaded.config.log_path);
    let log_path = if log_path.is_absolute() {
        log_path.to_path_buf()
    } else {
        root.join(log_path)
    };
    if let Err(error) = logger.set_logfile(&log_path) {
        eprintln!(
            "xeHentai: cannot open log file {}: {error}",
            log_path.display()
        );
        return 1;
    }
    let origin = match loaded.origin {
        config::Origin::Rss => "config.rss",
        config::Origin::ImportedPython => "config.py (imported once)",
        config::Origin::Defaults => "built-in defaults",
    };
    let message = format!(
        "xeHentai {} configuration loaded from {origin}; file {}",
        r#const::VERSION_DISPLAY,
        loaded.path.display()
    );
    if let Err(error) = logger.info(&message) {
        eprintln!(
            "xeHentai: cannot write log file {}: {error}",
            log_path.display()
        );
        return 1;
    }
    // FILEPATH in the reference is the application directory; the legacy
    // cookie file lives next to the program, not in the working directory.
    let filepath = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf());
    let mut app = match core::App::new(loaded.config, logger, root, &filepath) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("xeHentai: session error: {error}");
            return 1;
        }
    };
    eprintln!(
        "xeHentai: this build implements the configuration, task, session and file foundation; downloading is not implemented"
    );
    app.logger_mut().cleanup();
    2
}

#[cfg(test)]
mod tests;
