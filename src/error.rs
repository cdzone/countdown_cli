//! Application-level error codes and the HTTP API's own wire vocabulary.
//!
//! MCP tools and the Agent branch on [`ErrorCode`]. The HTTP API keeps a
//! separate [`HttpErrorCode`] table (scheme A / 0.8.0): wire strings stay the
//! historical API names (`unauthorized` / `forbidden`, plus HTTP-only codes).
//! The MCP boundary maps every [`HttpErrorCode`] into [`ErrorCode`] — a missing
//! arm is a compile error.

use serde::{Deserialize, Serialize};

/// Application error code shared by MCP `structuredContent.error` and Agent.
///
/// Human-facing `message` text may change; callers and fixtures must branch on
/// `code`, never on message substrings. Adding/removing/renaming a code is a
/// contract change → bump.
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

    /// Map a wire string (MCP structuredContent) to a known code.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_found" => Some(Self::NotFound),
            "invalid_argument" => Some(Self::InvalidArgument),
            "unauthenticated" => Some(Self::Unauthenticated),
            "permission_denied" => Some(Self::PermissionDenied),
            "contract_mismatch" => Some(Self::ContractMismatch),
            "upstream_unavailable" => Some(Self::UpstreamUnavailable),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HTTP API error codes as they appear in `{ "error": { "code", "message" } }`.
///
/// Independent of [`ErrorCode`]. 401/403 keep the historical names
/// `unauthorized` / `forbidden`. HTTP-only validation codes stay here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    InvalidBody,
    InvalidDatetime,
    InvalidRepeat,
    PayloadTooLarge,
    AuditUnavailable,
    Internal,
}

impl HttpErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::InvalidBody => "invalid_body",
            Self::InvalidDatetime => "invalid_datetime",
            Self::InvalidRepeat => "invalid_repeat",
            Self::PayloadTooLarge => "payload_too_large",
            Self::AuditUnavailable => "audit_unavailable",
            Self::Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unauthorized" => Some(Self::Unauthorized),
            "forbidden" => Some(Self::Forbidden),
            "not_found" => Some(Self::NotFound),
            "invalid_body" => Some(Self::InvalidBody),
            "invalid_datetime" => Some(Self::InvalidDatetime),
            "invalid_repeat" => Some(Self::InvalidRepeat),
            "payload_too_large" => Some(Self::PayloadTooLarge),
            "audit_unavailable" => Some(Self::AuditUnavailable),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }

    /// Exhaustive map used at the MCP boundary. Adding an HTTP code without a
    /// new arm here fails to compile.
    pub const fn to_mcp(self) -> ErrorCode {
        match self {
            Self::Unauthorized => ErrorCode::Unauthenticated,
            Self::Forbidden => ErrorCode::PermissionDenied,
            Self::NotFound => ErrorCode::NotFound,
            Self::InvalidBody
            | Self::InvalidDatetime
            | Self::InvalidRepeat
            | Self::PayloadTooLarge => ErrorCode::InvalidArgument,
            Self::AuditUnavailable | Self::Internal => ErrorCode::Internal,
        }
    }
}

impl std::fmt::Display for HttpErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_round_trip() {
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
        // Pre-0.8.0 HTTP aliases are no longer accepted on the MCP vocabulary.
        assert_eq!(ErrorCode::parse("unauthorized"), None);
        assert_eq!(ErrorCode::parse("forbidden"), None);
        assert_eq!(ErrorCode::parse("nope"), None);
    }

    #[test]
    fn http_error_code_round_trip_and_mcp_map() {
        for code in [
            HttpErrorCode::Unauthorized,
            HttpErrorCode::Forbidden,
            HttpErrorCode::NotFound,
            HttpErrorCode::InvalidBody,
            HttpErrorCode::InvalidDatetime,
            HttpErrorCode::InvalidRepeat,
            HttpErrorCode::PayloadTooLarge,
            HttpErrorCode::AuditUnavailable,
            HttpErrorCode::Internal,
        ] {
            assert_eq!(HttpErrorCode::parse(code.as_str()), Some(code));
            let _ = code.to_mcp(); // exhaustiveness is the compile gate
        }
        assert_eq!(
            HttpErrorCode::Unauthorized.to_mcp(),
            ErrorCode::Unauthenticated
        );
        assert_eq!(
            HttpErrorCode::Forbidden.to_mcp(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            HttpErrorCode::InvalidDatetime.to_mcp(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(HttpErrorCode::parse("unauthenticated"), None);
        assert_eq!(HttpErrorCode::parse("permission_denied"), None);
    }
}
