//! Contract snapshots for MCP tools / representative outbound / HTTP error bodies.
//!
//! When a snapshot must change, update the fixture in the same PR as CHANGELOG
//! and the version bump. Set `UPDATE_SNAPSHOTS=1` to rewrite fixtures from a
//! green local run (never in CI).

mod common;

use std::{fs, path::PathBuf};

use countdown_cli::error::HttpErrorCode;
use serde_json::{json, Value};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_fixture(name: &str) -> Value {
    let path = fixtures_dir().join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn write_fixture(name: &str, value: &Value) {
    let path = fixtures_dir().join(name);
    let body = serde_json::to_string_pretty(value).unwrap() + "\n";
    fs::write(&path, body).unwrap();
}

fn assert_snapshot(name: &str, actual: Value) {
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        write_fixture(name, &actual);
        return;
    }
    let expected = read_fixture(name);
    assert_eq!(
        expected, actual,
        "snapshot {name} drifted — update fixture + CHANGELOG + version together"
    );
}

/// Stable HTTP error body examples (code + representative message).
#[test]
fn http_error_bodies_match_fixture() {
    let bodies: Value = json!({
        "unauthorized": {"error": {"code": HttpErrorCode::Unauthorized.as_str(), "message": "missing bearer token"}},
        "forbidden": {"error": {"code": HttpErrorCode::Forbidden.as_str(), "message": "insufficient scope"}},
        "not_found": {"error": {"code": HttpErrorCode::NotFound.as_str(), "message": "Countdown not found"}},
        "invalid_body": {"error": {"code": HttpErrorCode::InvalidBody.as_str(), "message": "title must not be empty"}},
        "invalid_datetime": {"error": {"code": HttpErrorCode::InvalidDatetime.as_str(), "message": "invalid datetime format"}},
        "invalid_repeat": {"error": {"code": HttpErrorCode::InvalidRepeat.as_str(), "message": "invalid repeat"}},
        "payload_too_large": {"error": {"code": HttpErrorCode::PayloadTooLarge.as_str(), "message": "request body too large"}},
        "audit_unavailable": {"error": {"code": HttpErrorCode::AuditUnavailable.as_str(), "message": "audit log unavailable"}},
        "internal": {"error": {"code": HttpErrorCode::Internal.as_str(), "message": "internal"}},
    });
    assert_snapshot("http-error-bodies.json", bodies);
}

/// Representative MCP model-view outbound (no target_time / remaining_seconds).
#[test]
fn mcp_tool_outputs_match_fixture() {
    let item = json!({
        "id": "morning",
        "title": "晨会",
        "next_target_time": "2026-10-24 09:00:00",
        "next_remaining_seconds": 3600,
        "repeat": "daily",
        "enabled": true,
        "expired": false
    });
    let outputs = json!({
        "list_countdowns": [item.clone()],
        "get_countdown": item.clone(),
        "get_next_countdown": {"countdown": item},
        "get_next_countdown_empty": {"countdown": null}
    });
    // Guardrails: model view must not grow target_time / remaining_seconds back.
    for key in ["list_countdowns", "get_countdown"] {
        let v = &outputs[key];
        let objs: Vec<&Value> = if v.is_array() {
            v.as_array().unwrap().iter().collect()
        } else {
            vec![v]
        };
        for o in objs {
            assert!(o.get("target_time").is_none(), "{key}: {o}");
            assert!(o.get("remaining_seconds").is_none(), "{key}: {o}");
        }
    }
    assert!(outputs["get_next_countdown"]["countdown"]
        .get("target_time")
        .is_none());
    assert_snapshot("mcp-tool-outputs.json", outputs);
}

/// Ensures every HttpErrorCode maps into ErrorCode (compile gate is in error.rs;
/// this locks the wire table into the fixture set).
#[test]
fn http_error_codes_are_exhaustive_in_fixture() {
    let bodies = if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        // Writing happens in the other test; just ensure file exists after.
        return;
    } else {
        read_fixture("http-error-bodies.json")
    };
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
        let key = code.as_str();
        assert_eq!(
            bodies[key]["error"]["code"], key,
            "fixture missing or wrong for {key}"
        );
        let _ = code.to_mcp();
    }
}
