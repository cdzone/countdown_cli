//! countdown_mcp must reach the local API even when proxy env vars are set.

use std::{
    net::TcpListener as StdListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Returns the JSON-RPC message carried in a JSON or SSE response body.
fn rpc_body(body: &str) -> Value {
    if let Ok(v) = serde_json::from_str(body) {
        return v;
    }
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .find(|d| !d.is_empty())
        .unwrap_or_else(|| panic!("no data line in {body:?}"));
    serde_json::from_str(data).unwrap()
}

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

    let mcp_port = free_port();
    let _mcp = Kill(
        Command::new(env!("CARGO_BIN_EXE_countdown_mcp"))
            .args(["--api-url", &format!("http://{api_addr}")])
            .args(["--addr", &format!("127.0.0.1:{mcp_port}")])
            // A dead proxy: if the client honours it, the API is unreachable.
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let url = format!("http://127.0.0.1:{mcp_port}/mcp");
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let post = |body: Value, session: Option<String>| {
        let mut req = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(s) = session {
            req = req.header("mcp-session-id", s);
        }
        req.send()
    };

    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"test","version":"0"}}});
    let mut resp = None;
    for _ in 0..50 {
        match post(init.clone(), None).await {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let resp = resp.expect("countdown_mcp did not start");
    let session = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    let _ = resp.text().await.unwrap();

    post(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        session.clone(),
    )
    .await
    .unwrap();

    let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"list_countdowns","arguments":{}}});
    let body = post(call, session).await.unwrap().text().await.unwrap();
    let result = &rpc_body(&body)["result"];
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"id\":\"a\""), "{text}");
}
