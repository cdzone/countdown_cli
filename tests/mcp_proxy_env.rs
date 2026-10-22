//! countdown_mcp must reach the local API even when proxy env vars are set.

mod common;

use axum::{routing::get, Json, Router};
use common::{token_file, McpClient, McpProcess, SERVICE_TOKEN};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn list_works_with_proxy_env_set() {
    let api = Router::new().route(
        "/countdowns",
        get(|| async {
            Json(json!([{
                "id": "a", "title": "a", "datetime": "2026-12-31 18:00:00",
                "enabled": true, "repeat": null,
                "target_time": "2026-12-31 18:00:00", "remaining_seconds": 100
            }]))
        }),
    );
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(api_listener, api).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "api.token", SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(&format!("http://{api_addr}"), Some(&token), |cmd| {
        // A dead proxy: if the client honours it, the API is unreachable.
        cmd.env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
    });

    let client = McpClient::connect(mcp.url(), None).await;
    let result = client.call_tool("list_countdowns", json!({})).await;
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"id\":\"a\""), "{text}");
}
