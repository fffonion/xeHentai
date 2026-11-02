//! One error type for the whole updater: loading the trusted business modules
//! (`Compile`/`Bind`/`Runtime`/`Shape`), the trusted source binding, and the
//! mandatory native package/installation checks.
//!
//! `Package` is every rejection of a downloaded package (signature, digest,
//! schema, target/ABI, entry list, archive safety). `Install` is every
//! rejection of an activation attempt (staging, candidate preflight,
//! replacement, re-execution, rollback). The distinction matters: a `Package`
//! rejection never touches the live installation, while an `Install` rejection
//! is what the startup controller reports before restoring the previous
//! release.

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

    pub fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: error.to_string(),
        }
    }
}
