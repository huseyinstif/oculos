//! Typed error kinds shared by every layer.
//!
//! Backends keep returning `anyhow::Result`, but wrap the errors that callers
//! can act on in an [`OculosError`] (via the helper constructors below). The
//! HTTP and MCP layers then look for it anywhere in the error chain to pick a
//! status code / machine-readable `code` instead of guessing from the message.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Element, window or application does not exist (or no longer exists).
    NotFound,
    /// The request itself is malformed (bad key name, unknown element type…).
    InvalidInput,
    /// The element / platform does not support the requested operation.
    Unsupported,
    /// A wait operation ran out of time.
    Timeout,
    /// The OS refused access (e.g. macOS Accessibility permission missing).
    PermissionDenied,
}

impl ErrorKind {
    /// Stable machine-readable identifier used in API responses.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "not_found",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Timeout => "timeout",
            ErrorKind::PermissionDenied => "permission_denied",
        }
    }
}

#[derive(Debug)]
pub struct OculosError {
    pub kind: ErrorKind,
    pub message: String,
}

impl fmt::Display for OculosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OculosError {}

fn make(kind: ErrorKind, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(OculosError {
        kind,
        message: msg.into(),
    })
}

pub fn not_found(msg: impl Into<String>) -> anyhow::Error {
    make(ErrorKind::NotFound, msg)
}

pub fn invalid_input(msg: impl Into<String>) -> anyhow::Error {
    make(ErrorKind::InvalidInput, msg)
}

pub fn unsupported(msg: impl Into<String>) -> anyhow::Error {
    make(ErrorKind::Unsupported, msg)
}

pub fn timeout(msg: impl Into<String>) -> anyhow::Error {
    make(ErrorKind::Timeout, msg)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn permission_denied(msg: impl Into<String>) -> anyhow::Error {
    make(ErrorKind::PermissionDenied, msg)
}

/// The first [`ErrorKind`] found in the error chain, if any.
pub fn kind_of(err: &anyhow::Error) -> Option<ErrorKind> {
    err.chain()
        .find_map(|e| e.downcast_ref::<OculosError>())
        .map(|e| e.kind)
}

/// Standard "stale / unknown element id" error.
pub fn element_not_found(oculos_id: &str) -> anyhow::Error {
    not_found(format!(
        "Element '{oculos_id}' not found or no longer available — call find/tree again to get a fresh oculos_id."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn kind_survives_context() {
        let err: anyhow::Result<()> = Err(not_found("gone"));
        let err = err.context("while clicking").unwrap_err();
        assert_eq!(kind_of(&err), Some(ErrorKind::NotFound));
    }

    #[test]
    fn plain_errors_have_no_kind() {
        assert_eq!(kind_of(&anyhow::anyhow!("boom")), None);
    }
}
