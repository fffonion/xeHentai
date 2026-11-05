//! One error type for the whole updater: loading the trusted business modules
//! (`Compile`/`Bind`/`Runtime`/`Shape`), the trusted source binding, and the
//! mandatory native package/installation checks.
//!
//! `Package` is every rejection of a downloaded package (signature, digest,
//! schema, target/ABI, entry list, archive safety). `Install` is every
//! rejection of a candidate application attempt BEFORE any live file changes
//! (staging, candidate preflight). `Apply` is a failure AFTER the first live
//! mutation began: the installed tree may then be mixed, so the caller must
//! stop instead of treating it like a rejection. The distinction matters: a
//! `Package`/`Install` rejection never touches the live installation, while an
//! `Apply` failure is a real error of a switch that already started.

use std::path::PathBuf;

#[derive(Debug)]
pub enum UpdateError {
    Io {
        path: PathBuf,
        message: String,
    },
    Compile {
        script: &'static str,
        message: String,
    },
    Bind {
        script: &'static str,
        message: String,
    },
    Runtime {
        script: &'static str,
        message: String,
    },
    Shape {
        what: &'static str,
        message: String,
    },
    Binding(String),
    Package(String),
    Install(String),
    /// A live replacement (or the applied record after it) failed. Some files
    /// may already be new, so this is not a candidate rejection.
    Apply(String),
    /// Another process owns the per-install transaction lock. A startup that
    /// sees this must stop with a retryable outcome instead of running the
    /// business App on a tree that is mid-switch.
    Busy(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Compile { script, message } => write!(f, "{script} compile error: {message}"),
            Self::Bind { script, message } => write!(f, "{script} host binding error: {message}"),
            Self::Runtime { script, message } => write!(f, "{script} runtime error: {message}"),
            Self::Shape { what, message } => write!(f, "{what} shape error: {message}"),
            Self::Binding(message) => write!(f, "update source binding error: {message}"),
            Self::Package(message) => write!(f, "update package rejected: {message}"),
            Self::Install(message) => write!(f, "update installation failed: {message}"),
            Self::Apply(message) => write!(f, "update application failed: {message}"),
            Self::Busy(message) => write!(f, "update installation is busy: {message}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl UpdateError {
    pub fn package(message: impl Into<String>) -> Self {
        Self::Package(message.into())
    }

    pub fn install(message: impl Into<String>) -> Self {
        Self::Install(message.into())
    }

    /// Tag a failure raised after the first live mutation. The phase, not the
    /// underlying variant, decides how startup reacts, so the original
    /// message (including its path) is kept.
    pub fn during_apply(self) -> Self {
        match self {
            Self::Apply(message) => Self::Apply(message),
            other => Self::Apply(other.to_string()),
        }
    }

    /// True only when live files may already have changed.
    pub fn is_apply(&self) -> bool {
        matches!(self, Self::Apply(_))
    }

    pub fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: error.to_string(),
        }
    }
}
