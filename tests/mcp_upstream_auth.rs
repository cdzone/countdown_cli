//! How `countdown_mcp` authenticates to the API and reports upstream
//! failures, checked against the real binary and a recording stub API.

mod common;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Router,
};
use common::{token_file, McpClient, McpProcess, SERVICE_TOKEN};
use serde_json::{json, Value};

const CLIENT_TOKEN: &str = "client-token-should-never-reach-the-api";
const BODY_MARKER: &str = "BODY-MARKER-7f3a";

type Seen = Arc<Mutex<Vec<HeaderMap>>>;

/// Stub API: the first `ok_first` requests get an empty list, later ones a
/// 401 whose body carries `BODY_MARKER`. Records every request's headers.
async fn stub_api(ok_first: usize) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new().fallback({
        let seen = seen.clone();
        move |headers: HeaderMap| {
            seen.lock().unwrap().push(headers);
            let n = count.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < ok_first {
                    (StatusCode::OK, "[]".to_owned()).into_response()
                } else {
                    let body = json!({"error":{"code":"unauthorized","message":BODY_MARKER}});
                    (StatusCode::UNAUTHORIZED, body.to_string()).into_response()
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn json_lines(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn assert_no_secrets(stderr: &str) {
    assert!(!stderr.contains(SERVICE_TOKEN), "{stderr}");
    assert!(!stderr.contains(BODY_MARKER), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_sees_the_service_token_not_the_callers() {
    let (api, seen) = stub_api(usize::MAX).await;
    let dir = tempfile::tempdir().unwrap();
    // Written by `echo "$TOKEN" > file`: the trailing newline must not be sent.
    let token = token_file(
        dir.path(),
        "api.token",
        &format!("{SERVICE_TOKEN}\n"),
        0o600,
    );
    let mcp = McpProcess::spawn(&api, Some(&token), |_| {});

    let client = McpClient::connect(mcp.url(), Some(&format!("Bearer {CLIENT_TOKEN}"))).await;
    let result = client.call_tool("list_countdowns", json!({})).await;
    assert_eq!(result["isError"], json!(false), "{result}");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "startup probe + one tool call");
    for headers in seen.iter() {
        assert_eq!(headers["authorization"], format!("Bearer {SERVICE_TOKEN}"));
        let id = headers["x-request-id"].to_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{id}");
        for value in headers.values() {
            assert!(!value.to_str().unwrap_or("").contains(CLIENT_TOKEN));
        }
    }
    assert_ne!(seen[0]["x-request-id"], seen[1]["x-request-id"]);
    assert_no_secrets(&mcp.finish());
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_401_is_fixed_tool_error_and_one_json_line() {
    let (api, seen) = stub_api(1).await; // probe passes, the tool call is rejected
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "api.token", SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(&api, Some(&token), |_| {});

    let client = McpClient::connect(mcp.url(), None).await;
    let result = client.call_tool("list_countdowns", json!({})).await;
    assert_eq!(result["isError"], json!(true), "{result}");
    assert_eq!(
        result["content"][0]["text"],
        "countdown API rejected the MCP service credential (HTTP 401)"
    );
    assert!(!result.to_string().contains(BODY_MARKER), "{result}");

    let tool_request_id = seen.lock().unwrap()[1]["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let stderr = mcp.finish();
    assert_no_secrets(&stderr);
    let lines = json_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    let line = &lines[0];
    assert_eq!(line["svc"], "mcp");
    assert_eq!(line["request_id"], tool_request_id.as_str());
    assert_eq!(line["action"], "tool:list_countdowns");
    assert_eq!(line["outcome"], "upstream_auth_failed");
    assert_eq!(line["upstream_status"], 401);
    assert!(line["ts"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_credential_at_startup_exits_2() {
    let (api, _) = stub_api(0).await;
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "api.token", SERVICE_TOKEN, 0o600);
    let mut mcp = McpProcess::spawn(&api, Some(&token), |_| {});

    let status = mcp
        .wait_exit()
        .expect("should exit on a rejected credential");
    assert_eq!(status.code(), Some(2));
    let stderr = mcp.stderr();
    assert!(
        stderr.contains("rejected the MCP service credential"),
        "{stderr}"
    );
    assert!(!stderr.contains("listening"), "{stderr}");
    assert_no_secrets(&stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_api_at_startup_is_reported_and_serving_continues() {
    let port = common::free_port();
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "api.token", SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(&format!("http://127.0.0.1:{port}"), Some(&token), |_| {});

    assert!(mcp.wait_for_stderr("listening"), "{}", mcp.stderr());
    let stderr = mcp.finish();
    let lines = json_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert_eq!(lines[0]["svc"], "mcp");
    assert_eq!(lines[0]["outcome"], "upstream_unreachable");
    assert!(uuid::Uuid::parse_str(lines[0]["request_id"].as_str().unwrap()).is_ok());
    assert_no_secrets(&stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn unusable_token_file_exits_2_without_echoing_it() {
    let (api, seen) = stub_api(usize::MAX).await;
    let dir = tempfile::tempdir().unwrap();
    let two_lines = format!("{SERVICE_TOKEN}\nx\n");
    let mut cases = vec![
        ("--api-token-file", None),
        ("cannot read", Some(dir.path().join("absent.token"))),
        (
            "token is empty",
            Some(token_file(dir.path(), "empty.token", "\n", 0o600)),
        ),
        (
            "one line",
            Some(token_file(dir.path(), "two-lines.token", &two_lines, 0o600)),
        ),
    ];
    if cfg!(unix) {
        let open = token_file(dir.path(), "open.token", SERVICE_TOKEN, 0o644);
        cases.push(("mode 644", Some(open)));
    }
    for (reason, path) in cases {
        let mut mcp = McpProcess::spawn(&api, path.as_deref(), |_| {});
        let status = mcp.wait_exit().expect(reason);
        assert_eq!(status.code(), Some(2), "{reason}");
        let stderr = mcp.stderr();
        assert!(stderr.contains(reason), "{reason}: {stderr}");
        assert_no_secrets(&stderr);
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "no request without a token"
    );
}
