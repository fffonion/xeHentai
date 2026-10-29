use std::fmt;

use vm::VmError;

/// Failure from compiling, binding, or driving a RustScript entry.
#[derive(Debug)]
pub struct HostError {
    kind: &'static str,
    message: String,
}

impl HostError {
    pub fn compile(message: impl Into<String>) -> Self {
        Self {
            kind: "compile",
            message: message.into(),
        }
    }

    pub fn bind(message: impl Into<String>) -> Self {
        Self {
            kind: "bind",
            message: message.into(),
        }
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            kind: "runtime",
            message: message.into(),
        }
    }

    pub fn from_vm(error: VmError) -> Self {
        Self::runtime(error.to_string())
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for HostError {}
