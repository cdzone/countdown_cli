//! Shared application-level error codes for HTTP API and MCP tools.
//!
//! Stable machine-readable vocabulary (P1). Human-facing `message` text may
//! change; callers and fixtures must branch on `code`, never on message
//! substrings. Adding/removing/renaming a code is a contract change → bump.

use serde::{Deserialize, Serialize};

/// Application error code shared by HTTP `{ "error": { "code", "message" } }`
/// and MCP `structuredContent.error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    InvalidArgument,
    Unauthenticated,
    PermissionDenied,
    /// Consumer/producer contract mismatch (e.g. required field absent).
    ContractMismatch,
    UpstreamUnavailable,
    Internal,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::InvalidArgument => "invalid_argument",
            Self::Unauthenticated => "unauthenticated",
            Self::PermissionDenied => "permission_denied",
            Self::ContractMismatch => "contract_mismatch",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::Internal => "internal",
        }
    }

    /// Map a wire string (API body or MCP structuredContent) to a known code.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_found" => Some(Self::NotFound),
            "invalid_argument" => Some(Self::InvalidArgument),
            "unauthenticated" => Some(Self::Unauthenticated),
            "permission_denied" => Some(Self::PermissionDenied),
            "contract_mismatch" => Some(Self::ContractMismatch),
            "upstream_unavailable" => Some(Self::UpstreamUnavailable),
            "internal" => Some(Self::Internal),
            // Pre-0.7.3 aliases still seen from older stubs / docs samples.
            "unauthorized" => Some(Self::Unauthenticated),
            "forbidden" => Some(Self::PermissionDenied),
            _ => None,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_as_str_and_parse() {
        for code in [
            ErrorCode::NotFound,
            ErrorCode::InvalidArgument,
            ErrorCode::Unauthenticated,
            ErrorCode::PermissionDenied,
            ErrorCode::ContractMismatch,
            ErrorCode::UpstreamUnavailable,
            ErrorCode::Internal,
        ] {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(
            ErrorCode::parse("unauthorized"),
            Some(ErrorCode::Unauthenticated)
        );
        assert_eq!(
            ErrorCode::parse("forbidden"),
            Some(ErrorCode::PermissionDenied)
        );
        assert_eq!(ErrorCode::parse("nope"), None);
    }
}
