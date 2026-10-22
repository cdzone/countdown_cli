//! Read-only MCP server for countdown_cli.
//!
//! Talks to the countdown HTTP API only; it never touches the config file.

use std::net::SocketAddr;

use clap::Parser;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "countdown_mcp",
    about = "Read-only MCP server over the countdown HTTP API"
)]
struct Args {
    /// Base URL of the countdown HTTP API
    #[arg(
        long,
        env = "COUNTDOWN_API_URL",
        default_value = "http://127.0.0.1:8080"
    )]
    api_url: reqwest::Url,
    /// Address to serve MCP on (loopback only)
    #[arg(long, env = "COUNTDOWN_MCP_ADDR", default_value = "127.0.0.1:8765")]
    addr: SocketAddr,
}

/// Fields of the API's countdown object that the tools expose.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Countdown {
    id: String,
    title: String,
    target_time: Option<String>,
    remaining_seconds: Option<i64>,
    repeat: Option<String>,
    enabled: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Only return countdowns due within this many hours (0 <= remaining_seconds <= within_hours*3600)
    within_hours: Option<u32>,
    /// Include disabled countdowns (default false)
    #[serde(default)]
    include_disabled: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetArgs {
    /// Countdown id
    id: String,
}

#[derive(Clone)]
struct CountdownMcp {
    api_url: reqwest::Url,
    http: reqwest::Client,
}

fn tool_error(msg: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text =
        serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn due_within(c: &Countdown, hours: u32) -> bool {
    let limit = i64::from(hours) * 3600;
    matches!(c.remaining_seconds, Some(s) if (0..=limit).contains(&s))
}

#[tool_router]
impl CountdownMcp {
    fn new(api_url: reqwest::Url) -> Self {
        Self {
            api_url,
            http: reqwest::Client::new(),
        }
    }

    /// `segments` are percent-encoded, so an id cannot escape `/countdowns/`.
    fn endpoint(&self, segments: &[&str]) -> reqwest::Url {
        let mut url = self.api_url.clone();
        url.path_segments_mut()
            .expect("http(s) URL has path segments")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(&self, url: reqwest::Url) -> Result<T, String> {
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|e| format!("countdown API unreachable at {url}: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("countdown not found".to_string());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("countdown API returned {status}: {body}"));
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("countdown API returned an unexpected body: {e}"))
    }

    #[tool(
        description = "List countdowns sorted by target time. Returns id, title, target_time, remaining_seconds, repeat, enabled."
    )]
    async fn list_countdowns(
        &self,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut url = self.endpoint(&["countdowns"]);
        if !args.include_disabled {
            url.query_pairs_mut().append_pair("enabled", "true");
        }
        let items: Vec<Countdown> = match self.fetch(url).await {
            Ok(items) => items,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let items: Vec<Countdown> = match args.within_hours {
            Some(h) => items.into_iter().filter(|c| due_within(c, h)).collect(),
            None => items,
        };
        json_result(&items)
    }

    #[tool(
        description = "Get one countdown by id. Returns id, title, target_time, remaining_seconds, repeat, enabled."
    )]
    async fn get_countdown(
        &self,
        Parameters(GetArgs { id }): Parameters<GetArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .fetch::<Countdown>(self.endpoint(&["countdowns", &id]))
            .await
        {
            Ok(item) => json_result(&item),
            Err(msg) => Ok(tool_error(format!("{msg} (id: {id})"))),
        }
    }
}

#[tool_handler]
impl ServerHandler for CountdownMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if !args.addr.ip().is_loopback() {
        eprintln!("refusing to listen on non-loopback address {}", args.addr);
        std::process::exit(2);
    }
    let listener = match tokio::net::TcpListener::bind(args.addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {}: {e}", args.addr);
            std::process::exit(1);
        }
    };
    let api_url = args.api_url;
    let service = StreamableHttpService::new(
        move || Ok(CountdownMcp::new(api_url.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    eprintln!("countdown_mcp listening on http://{}/mcp", args.addr);
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, Query},
        http::StatusCode,
        response::IntoResponse,
        routing::get,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;

    fn item(id: &str, remaining: Option<i64>, enabled: bool) -> Value {
        json!({
            "id": id, "title": id, "datetime": "2026-10-22 12:00",
            "enabled": enabled, "repeat": null,
            "target_time": remaining.map(|_| "2026-10-22 12:00:00"),
            "remaining_seconds": remaining,
        })
    }

    /// Stub of the countdown API with the same routes and status codes.
    async fn stub_api() -> reqwest::Url {
        let all = vec![
            item("past", Some(-1), true),
            item("now", Some(0), true),
            item("edge", Some(3600), true),
            item("later", Some(3601), true),
            item("none", None, true),
            item("off", Some(10), false),
        ];
        let list = {
            let all = all.clone();
            move |Query(q): Query<HashMap<String, String>>| {
                let all = all.clone();
                async move {
                    let want = q.get("enabled").map(|v| v == "true");
                    let v: Vec<Value> = all
                        .into_iter()
                        .filter(|c| want.is_none_or(|w| c["enabled"] == w))
                        .collect();
                    Json(v)
                }
            }
        };
        let one = move |Path(id): Path<String>| {
            let all = all.clone();
            async move {
                match all.into_iter().find(|c| c["id"] == id.as_str()) {
                    Some(c) => Json(c).into_response(),
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error":{"code":"not_found"}})),
                    )
                        .into_response(),
                }
            }
        };
        let app = Router::new()
            .route("/countdowns", get(list))
            .route("/countdowns/{id}", get(one));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}").parse().unwrap()
    }

    async fn unreachable_url() -> reqwest::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}").parse().unwrap()
    }

    fn text(r: &CallToolResult) -> String {
        r.content[0].as_text().expect("text content").text.clone()
    }

    fn ids(r: &CallToolResult) -> Vec<String> {
        let v: Vec<Countdown> = serde_json::from_str(&text(r)).unwrap();
        v.into_iter().map(|c| c.id).collect()
    }

    fn list(within_hours: Option<u32>, include_disabled: bool) -> Parameters<ListArgs> {
        Parameters(ListArgs {
            within_hours,
            include_disabled,
        })
    }

    #[tokio::test]
    async fn unreachable_api_is_tool_error_not_protocol_error() {
        let mcp = CountdownMcp::new(unreachable_url().await);
        let r = mcp.list_countdowns(list(None, false)).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("unreachable"), "{}", text(&r));
        let r = mcp
            .get_countdown(Parameters(GetArgs { id: "x".into() }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("unreachable"), "{}", text(&r));
    }

    #[tokio::test]
    async fn get_missing_is_tool_error_and_existing_succeeds() {
        let mcp = CountdownMcp::new(stub_api().await);
        let r = mcp
            .get_countdown(Parameters(GetArgs { id: "nope".into() }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("not found"), "{}", text(&r));

        let r = mcp
            .get_countdown(Parameters(GetArgs { id: "edge".into() }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(false));
        let c: Countdown = serde_json::from_str(&text(&r)).unwrap();
        assert_eq!(c.remaining_seconds, Some(3600));
    }

    #[tokio::test]
    async fn id_cannot_escape_the_countdown_path() {
        let mcp = CountdownMcp::new(stub_api().await);
        let r = mcp
            .get_countdown(Parameters(GetArgs {
                id: "../countdowns".into(),
            }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("not found"), "{}", text(&r));
    }

    #[tokio::test]
    async fn list_hides_disabled_by_default() {
        let mcp = CountdownMcp::new(stub_api().await);
        let r = mcp.list_countdowns(list(None, false)).await.unwrap();
        assert!(!ids(&r).contains(&"off".to_string()));
        let r = mcp.list_countdowns(list(None, true)).await.unwrap();
        assert!(ids(&r).contains(&"off".to_string()));
    }

    #[tokio::test]
    async fn within_hours_keeps_zero_to_limit_inclusive() {
        let mcp = CountdownMcp::new(stub_api().await);
        let r = mcp.list_countdowns(list(Some(1), false)).await.unwrap();
        assert_eq!(ids(&r), vec!["now", "edge"]);
    }

    #[test]
    fn list_args_default_include_disabled_false() {
        let a: ListArgs = serde_json::from_value(json!({})).unwrap();
        assert!(!a.include_disabled);
        assert_eq!(a.within_hours, None);
    }
}
