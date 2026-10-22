//! `/mcp` entry: bearer authentication, Origin allow-list, audience check,
//! and one audit line per request that is refused or per tool call.

mod common;

use std::time::Duration;

use common::{
    caller_auth, free_port, sha256_hex, token_file, token_table, McpClient, McpProcess, CALLER_ID,
    CALLER_TOKEN, SERVICE_TOKEN,
};
use serde_json::{json, Value};

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

/// Stub API: every request gets `status`.
async fn stub_api(status: u16) -> String {
    let app = axum::Router::new()
        .fallback(move || async move { axum::http::StatusCode::from_u16(status).unwrap() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn unreachable_api() -> String {
    format!("http://127.0.0.1:{}", free_port())
}

async fn started(api: &str) -> (McpProcess, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let svc = token_file(dir.path(), "svc.token", SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(api, Some(&svc), |_| {});
    assert!(mcp.wait_for_stderr("listening"), "{}", mcp.stderr());
    (mcp, dir)
}

fn http() -> reqwest::Client {
    common::http_client()
}

/// Sends `method` to `/mcp` with the given extra headers.
async fn send(mcp: &McpProcess, method: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = http()
        .request(method.parse().unwrap(), mcp.url())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    if method == "POST" {
        req = req.body(INIT);
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.unwrap()
}

fn entry_lines(mcp: &McpProcess) -> Vec<Value> {
    mcp.audit_lines()
        .into_iter()
        .filter(|l| l["action"].as_str().unwrap().starts_with("http:"))
        .collect()
}

fn challenge(resp: &reqwest::Response) -> String {
    resp.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_owned()
}

/// Acceptance 6: no header → 401; a valid token can initialize and call.
#[tokio::test(flavor = "multi_thread")]
async fn entry_requires_a_bearer_token() {
    let (mcp, _dir) = started(&unreachable_api()).await;
    let resp = send(&mcp, "POST", &[]).await;
    assert_eq!(resp.status(), 401);
    assert!(challenge(&resp).starts_with("Bearer "));
    // Unknown paths are behind the same layer.
    let nope = http()
        .get(mcp.url().replace("/mcp", "/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(nope.status(), 401);
    let wrong = send(&mcp, "POST", &[("authorization", "Bearer not-a-token")]).await;
    assert_eq!(wrong.status(), 401);
    assert!(challenge(&wrong).contains(r#"error="invalid_token""#));

    let client = McpClient::connect(mcp.url(), Some(&caller_auth())).await;
    let result = client.call_tool("list_countdowns", json!({})).await;
    assert!(result.get("content").is_some(), "{result}");
}

/// The refused path is client-chosen: the audit line keeps its first 64
/// characters only.
#[tokio::test(flavor = "multi_thread")]
async fn refused_path_is_cut_to_64_chars_in_audit() {
    let (mcp, _dir) = started(&unreachable_api()).await;
    let path: String = "/".to_owned()
        + &(0..5000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect::<String>();
    let resp = http()
        .get(mcp.url().replace("/mcp", &path))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let lines = entry_lines(&mcp);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let expected: String = path.chars().take(64).collect();
    assert_eq!(lines[0]["action"], format!("http:GET {expected}"));
    assert_eq!(lines[0]["auth_fail"], "missing");
}

/// Acceptance 2a (MCP part): malformed headers are 401 `malformed`, never 400.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_authorization_is_401_malformed() {
    let (mcp, _dir) = started(&unreachable_api()).await;
    let basic = send(&mcp, "POST", &[("authorization", "Basic xxx")]).await;
    assert_eq!(basic.status(), 401);
    assert!(
        !challenge(&basic).contains("error="),
        "{}",
        challenge(&basic)
    );
    let empty = send(&mcp, "POST", &[("authorization", "Bearer")]).await;
    assert_eq!(empty.status(), 401);
    assert!(challenge(&empty).contains(r#"error="invalid_token""#));
    let lines = entry_lines(&mcp);
    assert_eq!(lines.len(), 2, "{lines:?}");
    for l in &lines {
        assert_eq!(l["auth_fail"], "malformed");
        assert_eq!(l["outcome"], "unauthenticated");
        assert_eq!(l["action"], "http:POST /mcp");
    }
}

/// Acceptance 12b: POST, GET and DELETE without a token each leave a line.
#[tokio::test(flavor = "multi_thread")]
async fn refused_requests_are_audited_per_method() {
    let (mcp, _dir) = started(&unreachable_api()).await;
    for method in ["POST", "GET", "DELETE"] {
        let resp = send(&mcp, method, &[]).await;
        assert_eq!(resp.status(), 401, "{method}");
        assert!(challenge(&resp).starts_with("Bearer"));
    }
    let lines = entry_lines(&mcp);
    let actions: Vec<&str> = lines
        .iter()
        .map(|l| l["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        ["http:POST /mcp", "http:GET /mcp", "http:DELETE /mcp"]
    );
    for l in &lines {
        assert_eq!(l["outcome"], "unauthenticated");
        assert_eq!(l["auth_fail"], "missing");
        assert_eq!(l["caller"], Value::Null);
        assert_eq!(l["status"], 401);
        assert!(uuid::Uuid::parse_str(l["request_id"].as_str().unwrap()).is_ok());
    }
}

/// Acceptance 17: a present Origin must be allow-listed, even with a valid token.
#[tokio::test(flavor = "multi_thread")]
async fn origin_must_be_allow_listed() {
    let (mcp, _dir) = started(&unreachable_api()).await;
    let auth = caller_auth();
    let ok = send(&mcp, "POST", &[("authorization", &auth)]).await;
    assert_eq!(ok.status(), 200);
    let evil = send(
        &mcp,
        "POST",
        &[("authorization", &auth), ("origin", "http://evil.example")],
    )
    .await;
    assert_eq!(evil.status(), 403);
    let own = format!("http://127.0.0.1:{}", mcp.port);
    let mine = send(&mcp, "POST", &[("authorization", &auth), ("origin", &own)]).await;
    assert_eq!(mine.status(), 200);
    // A bad Origin with a bad token is a 401 first.
    let both = send(
        &mcp,
        "POST",
        &[
            ("authorization", "Bearer nope"),
            ("origin", "http://evil.example"),
        ],
    )
    .await;
    assert_eq!(both.status(), 401);

    let lines = entry_lines(&mcp);
    let forbidden: Vec<&Value> = lines.iter().filter(|l| l["status"] == 403).collect();
    assert_eq!(forbidden.len(), 1, "{lines:?}");
    assert_eq!(forbidden[0]["outcome"], "forbidden");
    assert_eq!(forbidden[0]["auth_fail"], "bad_origin");
    assert_eq!(forbidden[0]["caller"], CALLER_ID);
    assert_eq!(forbidden[0]["action"], "http:POST /mcp");
}

/// Every tool call leaves exactly one MCP line, also when the API fails.
#[tokio::test(flavor = "multi_thread")]
async fn failed_tool_calls_are_audited() {
    for (api, outcome) in [
        (unreachable_api(), "upstream_unreachable"),
        (stub_api(500).await, "error"),
        (stub_api(404).await, "not_found"),
    ] {
        let (mcp, _dir) = started(&api).await;
        let client = McpClient::connect(mcp.url(), Some(&caller_auth())).await;
        let result = client
            .call_tool("get_countdown", json!({"id": "abc"}))
            .await;
        assert_eq!(result["isError"], true, "{result}");
        let tools: Vec<Value> = mcp
            .audit_lines()
            .into_iter()
            .filter(|l| l["action"] == "tool:get_countdown")
            .collect();
        assert_eq!(tools.len(), 1, "{outcome}: {tools:?}");
        let l = &tools[0];
        assert_eq!(l["outcome"], outcome);
        assert_eq!(l["is_error"], true);
        assert_eq!(l["caller"], CALLER_ID);
        assert_eq!(l["target"], "abc");
        assert_eq!(l["params"]["id"], "abc");
        assert_eq!(result["_meta"]["request_id"], l["request_id"]);
        let stderr = mcp.finish();
        assert!(!stderr.contains(CALLER_TOKEN) && !stderr.contains(SERVICE_TOKEN));
    }
}

/// Acceptance 9: the API service token must not also open `/mcp`.
#[tokio::test(flavor = "multi_thread")]
async fn service_token_in_entry_table_refuses_startup() {
    let dir = tempfile::tempdir().unwrap();
    let svc = token_file(dir.path(), "svc.token", SERVICE_TOKEN, 0o600);
    let mixed = dir.path().join("mixed.toml");
    std::fs::write(
        &mixed,
        token_table(&[
            (CALLER_ID, CALLER_TOKEN, "countdowns:read"),
            ("oops", SERVICE_TOKEN, "countdowns:read"),
        ]),
    )
    .unwrap();
    let audit = dir.path().join("a.jsonl");
    let mut mcp = McpProcess::spawn(&unreachable_api(), Some(&svc), |cmd| {
        cmd.arg("--tokens")
            .arg(&mixed)
            .arg("--audit-log")
            .arg(&audit);
    });
    let status = mcp.wait_exit().expect("should refuse to start");
    assert_eq!(status.code(), Some(2));
    let stderr = mcp.stderr();
    assert!(
        stderr.contains("must not also be an MCP entry token"),
        "{stderr}"
    );
    assert!(!stderr.contains(SERVICE_TOKEN) && !stderr.contains(&sha256_hex(SERVICE_TOKEN)));
    assert!(!audit.exists(), "refused startup created the audit file");

    std::fs::write(
        &mixed,
        token_table(&[(CALLER_ID, CALLER_TOKEN, "countdowns:read")]),
    )
    .unwrap();
    let mcp = McpProcess::spawn(&unreachable_api(), Some(&svc), |cmd| {
        cmd.arg("--tokens")
            .arg(&mixed)
            .arg("--audit-log")
            .arg(&audit);
    });
    assert!(mcp.wait_for_stderr("listening"), "{}", mcp.stderr());
    tokio::time::sleep(Duration::from_millis(50)).await;
}
