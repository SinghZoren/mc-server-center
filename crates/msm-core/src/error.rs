//! Stable error codes.
//!
//! Every error that can reach a user or a client carries an [`ErrorCode`]. The string form
//! (e.g. `"PATH_REJECTED"`) is part of the public protocol: clients map it to translated,
//! actionable messages via [`ErrorCode::message_key`]. **Never rename or reuse a code** — add a
//! new variant instead. The `codes_are_stable` test pins every existing string.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Machine-readable, stable error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ErrorCode {
    /// Unexpected internal failure; details are in the daemon log.
    Internal,
    /// The request was malformed or a parameter failed validation.
    InvalidArgument,
    /// The referenced resource does not exist.
    NotFound,
    /// The resource already exists (e.g. duplicate name or port).
    AlreadyExists,
    /// The caller is authenticated but lacks the required capability.
    Forbidden,
    /// The caller could not be authenticated.
    Unauthenticated,
    /// The operation is not allowed in the resource's current state.
    InvalidStateTransition,
    /// Graceful stop is impossible because the console is not attached.
    ConsoleUnavailable,
    /// A path was rejected by the path-safety rules.
    PathRejected,
    /// A path resolved through a symlink, junction or other reparse point.
    PathTraversesLink,
    /// The Minecraft EULA has not been accepted for this instance.
    EulaNotAccepted,
    /// A downloaded file did not match its expected hash.
    DownloadHashMismatch,
    /// A download URL or redirect target is not on the allowlist.
    DownloadHostNotAllowed,
    /// An upstream service returned an error or malformed data.
    UpstreamError,
    /// An upstream service is rate limiting us.
    RateLimited,
    /// The requested port is already in use.
    PortInUse,
    /// Not enough free disk space for the operation.
    InsufficientDiskSpace,
    /// The operation was cancelled.
    Cancelled,
    /// The client and daemon protocol versions are incompatible.
    IncompatibleProtocol,
    /// The command exists in the protocol but this daemon build does not implement it yet.
    NotImplemented,
    /// No suitable Java runtime is installed for this server.
    JavaNotFound,
}

impl ErrorCode {
    /// Every code, in declaration order. Used by tests and documentation generators.
    pub const ALL: &'static [ErrorCode] = &[
        Self::Internal,
        Self::InvalidArgument,
        Self::NotFound,
        Self::AlreadyExists,
        Self::Forbidden,
        Self::Unauthenticated,
        Self::InvalidStateTransition,
        Self::ConsoleUnavailable,
        Self::PathRejected,
        Self::PathTraversesLink,
        Self::EulaNotAccepted,
        Self::DownloadHashMismatch,
        Self::DownloadHostNotAllowed,
        Self::UpstreamError,
        Self::RateLimited,
        Self::PortInUse,
        Self::InsufficientDiskSpace,
        Self::Cancelled,
        Self::IncompatibleProtocol,
        Self::NotImplemented,
        Self::JavaNotFound,
    ];

    /// The stable wire string, e.g. `"PATH_REJECTED"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "INTERNAL",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::NotFound => "NOT_FOUND",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::Forbidden => "FORBIDDEN",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::InvalidStateTransition => "INVALID_STATE_TRANSITION",
            Self::ConsoleUnavailable => "CONSOLE_UNAVAILABLE",
            Self::PathRejected => "PATH_REJECTED",
            Self::PathTraversesLink => "PATH_TRAVERSES_LINK",
            Self::EulaNotAccepted => "EULA_NOT_ACCEPTED",
            Self::DownloadHashMismatch => "DOWNLOAD_HASH_MISMATCH",
            Self::DownloadHostNotAllowed => "DOWNLOAD_HOST_NOT_ALLOWED",
            Self::UpstreamError => "UPSTREAM_ERROR",
            Self::RateLimited => "RATE_LIMITED",
            Self::PortInUse => "PORT_IN_USE",
            Self::InsufficientDiskSpace => "INSUFFICIENT_DISK_SPACE",
            Self::Cancelled => "CANCELLED",
            Self::IncompatibleProtocol => "INCOMPATIBLE_PROTOCOL",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::JavaNotFound => "JAVA_NOT_FOUND",
        }
    }

    /// Translation key for the user-facing message, e.g. `"error.path_rejected"`.
    #[must_use]
    pub fn message_key(self) -> String {
        format!("error.{}", self.as_str().to_ascii_lowercase())
    }

    /// Whether retrying the same request later may succeed.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::UpstreamError | Self::RateLimited | Self::Internal
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error with a stable code and a developer-facing detail string.
///
/// `detail` is for logs and diagnostics. It must not contain secrets, and UIs should show the
/// translated message for `code` rather than `detail`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct MsmError {
    /// Stable machine-readable code.
    pub code: ErrorCode,
    /// Developer-facing context (redacted, no secrets).
    pub detail: String,
}

impl MsmError {
    /// Creates an error with the given code and detail.
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Pins the wire strings. If this test fails you renamed a code: add a new one instead.
    #[test]
    fn codes_are_stable() {
        let expected = [
            "INTERNAL",
            "INVALID_ARGUMENT",
            "NOT_FOUND",
            "ALREADY_EXISTS",
            "FORBIDDEN",
            "UNAUTHENTICATED",
            "INVALID_STATE_TRANSITION",
            "CONSOLE_UNAVAILABLE",
            "PATH_REJECTED",
            "PATH_TRAVERSES_LINK",
            "EULA_NOT_ACCEPTED",
            "DOWNLOAD_HASH_MISMATCH",
            "DOWNLOAD_HOST_NOT_ALLOWED",
            "UPSTREAM_ERROR",
            "RATE_LIMITED",
            "PORT_IN_USE",
            "INSUFFICIENT_DISK_SPACE",
            "CANCELLED",
            "INCOMPATIBLE_PROTOCOL",
            "NOT_IMPLEMENTED",
            "JAVA_NOT_FOUND",
        ];
        let actual: Vec<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn codes_are_unique() {
        let set: HashSet<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(set.len(), ErrorCode::ALL.len());
    }

    #[test]
    fn serde_matches_as_str() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *code);
        }
    }

    #[test]
    fn message_keys_are_lowercase_and_prefixed() {
        assert_eq!(ErrorCode::PathRejected.message_key(), "error.path_rejected");
    }

    #[test]
    fn error_display_includes_code() {
        let err = MsmError::new(ErrorCode::NotFound, "instance 123");
        assert_eq!(err.to_string(), "NOT_FOUND: instance 123");
    }
}
