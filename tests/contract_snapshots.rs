//! Contract snapshots live next to the production paths they lock:
//! - MCP tools / outbound model view → `src/bin/countdown_mcp.rs` tests
//! - HTTP error bodies → `src/api.rs` tests (`http_error_bodies_match_fixture`)
//!
//! Fixtures: `tests/fixtures/mcp-tools.json`, `mcp-tool-outputs.json`,
//! `http-error-bodies.json`. When a snapshot changes, update the fixture in the
//! same PR as CHANGELOG and the version bump (`UPDATE_SNAPSHOTS=1`).
