//! Read-only MCP server for countdown_cli.
//!
//! Talks to the countdown HTTP API only; it never touches the config file.
//! Every API request carries this server's own service token (never the MCP
//! caller's), a fresh `X-Request-Id`, and the caller id when one is known.

use std::{
    fmt,
    net::SocketAddr,
    path::Path,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use clap::Parser;
use countdown_cli::{
    audit::{truncated, AuditLog},
    auth::{bearer_challenge, TokenTable},
};
use rmcp::{
    handler::server::{tool::Extension, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};

const X_REQUEST_ID: &str = "x-request-id";
const X_COUNTDOWN_CALLER: &str = "x-countdown-caller";
/// Bounds the startup probe so a hung API cannot stall startup.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// File holding the plaintext bearer token presented to the API
    /// (one line; on Unix it must not be readable by group or others)
    #[arg(long, env = "COUNTDOWN_API_TOKEN_FILE")]
    api_token_file: PathBuf,
    /// Token table (SHA-256 digests) for callers of /mcp
    #[arg(long, env = "COUNTDOWN_MCP_TOKENS")]
    tokens: PathBuf,
    /// Audit log, JSON Lines, mode 0600; never stderr
    #[arg(
        long,
        env = "COUNTDOWN_MCP_AUDIT_LOG",
        default_value = "countdown-mcp-audit.jsonl"
    )]
    audit_log: PathBuf,
}

/// `Authorization: Bearer <token>` for the API, marked sensitive.
/// `Debug` is redacted so the token cannot reach a log line.
#[derive(Clone)]
struct ServiceToken(HeaderValue);

impl fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceToken(<redacted>)")
    }
}

/// Why the token file was rejected. Messages never include file contents.
#[derive(Debug)]
enum TokenFileError {
    Io(std::io::Error),
    #[cfg_attr(not(unix), allow(dead_code))]
    TooOpen(u32),
    Empty,
    InvalidChars,
}

impl fmt::Display for TokenFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenFileError::Io(err) => write!(f, "cannot read: {err}"),
            TokenFileError::TooOpen(mode) => {
                write!(
                    f,
                    "mode {mode:o} gives group or others access; chmod 600 it"
                )
            }
            TokenFileError::Empty => f.write_str("token is empty"),
            TokenFileError::InvalidChars => f.write_str(
                "token must be one line of visible ASCII with no spaces \
                 (only a final \\n or \\r\\n is stripped)",
            ),
        }
    }
}

impl ServiceToken {
    fn load(path: &Path) -> Result<Self, TokenFileError> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).map_err(TokenFileError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .map_err(TokenFileError::Io)?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(TokenFileError::TooOpen(mode));
            }
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(TokenFileError::Io)?;
        Self::parse(&bytes)
    }

    /// Strips exactly one trailing `\n` or `\r\n`; anything else that is not
    /// visible ASCII is rejected rather than trimmed.
    fn parse(bytes: &[u8]) -> Result<Self, TokenFileError> {
        let token = match bytes.strip_suffix(b"\n") {
            Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
            None => bytes,
        };
        if token.is_empty() {
            return Err(TokenFileError::Empty);
        }
        if !token.iter().all(u8::is_ascii_graphic) {
            return Err(TokenFileError::InvalidChars);
        }
        let mut value = HeaderValue::from_bytes(&[b"Bearer ", token].concat())
            .expect("visible ASCII is a valid header value");
        value.set_sensitive(true);
        Ok(ServiceToken(value))
    }

    /// The bare token, for the audience check against the entry table.
    fn plaintext(&self) -> &str {
        self.0
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
    }
}

/// Id of the authenticated MCP caller, put into the request extensions by
/// the `/mcp` entry middleware and forwarded as `X-Countdown-Caller`.
#[derive(Clone, Debug)]
struct Caller(String);

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ok,
    Unauthenticated,
    Forbidden,
    NotFound,
    UpstreamAuthFailed,
    UpstreamUnreachable,
    Error,
}

/// One audit record (spec 5.1). Only structured fields; no headers or bodies.
#[derive(Serialize)]
struct Record<'a> {
    ts: String,
    svc: &'static str,
    request_id: &'a str,
    caller: Option<&'a str>,
    on_behalf_of: Option<&'a str>,
    action: &'a str,
    target: Option<&'a str>,
    params: &'a serde_json::Value,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_fail: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    duration_ms: u64,
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

/// Writes `record`; if the file cannot be written, stderr is the only fallback.
fn write_audit(audit: &AuditLog, record: &Record<'_>) {
    if let Err(e) = audit.append(record) {
        eprintln!(
            "audit log unavailable ({e}); lost record request_id={} action={}",
            record.request_id, record.action
        );
    }
}

/// Why an upstream request did not produce a result.
struct Failure {
    outcome: Outcome,
    upstream_status: Option<reqwest::StatusCode>,
    message: String,
}

/// One tool call (or the startup probe), correlated with the API side by
/// `request_id`. Exactly one audit record is written per call.
struct Upstream {
    request_id: String,
    caller: Option<String>,
    action: &'static str,
    target: Option<String>,
    params: serde_json::Value,
    started: Instant,
}

impl Upstream {
    fn new(
        action: &'static str,
        parts: Option<&Parts>,
        target: Option<&str>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            caller: parts
                .and_then(|p| p.extensions.get::<Caller>())
                .map(|c| c.0.clone()),
            action,
            target: target.map(str::to_owned),
            params,
            started: Instant::now(),
        }
    }

    fn record(
        &self,
        audit: &AuditLog,
        outcome: Outcome,
        is_error: Option<bool>,
        upstream_status: Option<reqwest::StatusCode>,
    ) {
        write_audit(
            audit,
            &Record {
                ts: now_rfc3339(),
                svc: "mcp",
                request_id: &self.request_id,
                caller: self.caller.as_deref(),
                on_behalf_of: None,
                action: self.action,
                target: self.target.as_deref(),
                params: &self.params,
                outcome,
                auth_fail: None,
                status: None,
                is_error,
                upstream_status: upstream_status.map(|s| s.as_u16()),
                duration_ms: self.started.elapsed().as_millis() as u64,
            },
        );
    }

    /// Audits the finished tool call and tags the result with `request_id`
    /// in `_meta`, so a client can quote it when reporting a problem.
    fn finish(
        &self,
        audit: &AuditLog,
        result: Result<CallToolResult, Failure>,
    ) -> Result<CallToolResult, McpError> {
        let mut result = match result {
            Ok(r) => {
                self.record(audit, Outcome::Ok, Some(false), None);
                r
            }
            Err(f) => {
                self.record(audit, f.outcome, Some(true), f.upstream_status);
                tool_error(f.message)
            }
        };
        let mut meta = serde_json::Map::new();
        meta.insert("request_id".into(), self.request_id.clone().into());
        result.meta = Some(MetaObject(meta));
        Ok(result)
    }
}

/// Why the `/mcp` entry rejected a request (spec 6.2; same rules as the API).
enum AuthFail {
    Missing,
    Malformed { bearer: bool },
    UnknownToken,
}

impl AuthFail {
    fn audit_name(&self) -> &'static str {
        match self {
            AuthFail::Missing => "missing",
            AuthFail::Malformed { .. } => "malformed",
            AuthFail::UnknownToken => "unknown_token",
        }
    }

    /// 401 per RFC 6750: `error=` only when a bearer token was presented.
    /// MCP has no scopes in step 4, so no `scope=` hint.
    fn response(&self) -> Response {
        let invalid_token = match self {
            AuthFail::Missing => false,
            AuthFail::Malformed { bearer } => *bearer,
            AuthFail::UnknownToken => true,
        };
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                bearer_challenge(invalid_token, None),
            )],
        )
            .into_response()
    }
}

/// Only the first `Authorization` header counts, as on the API side.
fn authenticate(tokens: &TokenTable, headers: &HeaderMap) -> Result<String, AuthFail> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or(AuthFail::Missing)?
        .to_str()
        .map_err(|_| AuthFail::Malformed { bearer: false })?;
    let (scheme, token) = value.split_once(' ').unwrap_or((value, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthFail::Malformed { bearer: false });
    }
    match token.trim() {
        "" => Err(AuthFail::Malformed { bearer: true }),
        token => tokens
            .lookup(token)
            .map(|p| p.id().to_owned())
            .ok_or(AuthFail::UnknownToken),
    }
}

/// State of the `/mcp` entry middleware.
struct Entry {
    tokens: TokenTable,
    audit: AuditLog,
}

impl Entry {
    fn record(
        &self,
        action: &str,
        caller: Option<&str>,
        outcome: Outcome,
        auth_fail: &'static str,
        status: StatusCode,
        started: Instant,
    ) {
        write_audit(
            &self.audit,
            &Record {
                ts: now_rfc3339(),
                svc: "mcp",
                request_id: &uuid::Uuid::new_v4().to_string(),
                caller,
                on_behalf_of: None,
                action,
                target: None,
                params: &serde_json::Value::Null,
                outcome,
                auth_fail: Some(auth_fail),
                status: Some(status.as_u16()),
                is_error: None,
                upstream_status: None,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
}

/// Authenticates every request before rmcp sees it (installed with
/// `Router::layer`, so unknown paths are covered too). A bad `Origin` is
/// refused by rmcp's allow-list; this layer only audits that 403.
async fn entry_guard(State(entry): State<Arc<Entry>>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    // The path is client-chosen (unknown paths land here too): cap it.
    let action = format!("http:{} {}", req.method(), truncated(req.uri().path()));
    let has_origin = req.headers().contains_key(header::ORIGIN);
    let caller = match authenticate(&entry.tokens, req.headers()) {
        Ok(caller) => caller,
        Err(fail) => {
            entry.record(
                &action,
                None,
                Outcome::Unauthenticated,
                fail.audit_name(),
                StatusCode::UNAUTHORIZED,
                started,
            );
            return fail.response();
        }
    };
    req.extensions_mut().insert(Caller(caller.clone()));
    let resp = next.run(req).await;
    if resp.status() == StatusCode::FORBIDDEN && has_origin {
        entry.record(
            &action,
            Some(&caller),
            Outcome::Forbidden,
            "bad_origin",
            StatusCode::FORBIDDEN,
            started,
        );
    }
    resp
}

/// Origins allowed on `/mcp` (spec 3.3a): the server's own loopback names.
fn allowed_origins(port: u16) -> [String; 2] {
    [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]
}

/// The app served on the MCP address: rmcp under `/mcp`, behind the guard.
fn app(mcp: CountdownMcp, entry: Arc<Entry>, port: u16) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(mcp.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_allowed_origins(allowed_origins(port)),
    );
    axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(entry, entry_guard))
}

fn rejected_credential(status: reqwest::StatusCode) -> String {
    format!(
        "countdown API rejected the MCP service credential (HTTP {})",
        status.as_u16()
    )
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
    audit: AuditLog,
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

/// Result of the startup probe that `main` has to act on.
#[derive(Debug, PartialEq)]
enum Probe {
    Accepted,
    Rejected(reqwest::StatusCode),
}

#[tool_router]
impl CountdownMcp {
    fn new(api_url: reqwest::Url, token: &ServiceToken, audit: AuditLog) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, token.0.clone());
        Self {
            api_url,
            // The API is local; ignore HTTP_PROXY and friends so a proxy
            // configured for the shell cannot make it look unreachable.
            http: reqwest::Client::builder()
                .no_proxy()
                .default_headers(headers)
                .build()
                .expect("build HTTP client"),
            audit,
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

    fn get(&self, url: reqwest::Url, up: &Upstream) -> reqwest::RequestBuilder {
        let mut req = self.http.get(url).header(X_REQUEST_ID, &up.request_id);
        if let Some(caller) = up.caller.as_deref() {
            if let Ok(value) = HeaderValue::from_str(caller) {
                req = req.header(X_COUNTDOWN_CALLER, value);
            }
        }
        req
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(
        &self,
        url: reqwest::Url,
        up: &Upstream,
    ) -> Result<T, Failure> {
        let fail = |outcome, upstream_status, message| Failure {
            outcome,
            upstream_status,
            message,
        };
        let resp = self.get(url.clone(), up).send().await.map_err(|e| {
            fail(
                Outcome::UpstreamUnreachable,
                None,
                format!("countdown API unreachable at {url}: {e}"),
            )
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(fail(
                Outcome::NotFound,
                Some(status),
                "countdown not found".to_string(),
            ));
        }
        // The API's body is not passed on: it is not meant for the model.
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(fail(
                Outcome::UpstreamAuthFailed,
                Some(status),
                rejected_credential(status),
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(fail(
                Outcome::Error,
                Some(status),
                format!("countdown API returned {status}: {body}"),
            ));
        }
        resp.json::<T>().await.map_err(|e| {
            fail(
                Outcome::Error,
                Some(status),
                format!("countdown API returned an unexpected body: {e}"),
            )
        })
    }

    /// Checks the service token once at startup. An unreachable API is only
    /// audited, since it may start after this server.
    async fn probe(&self) -> Probe {
        let up = Upstream::new("probe:GET /countdowns", None, None, serde_json::Value::Null);
        let mut url = self.endpoint(&["countdowns"]);
        url.query_pairs_mut().append_pair("enabled", "true");
        let status = match self.get(url, &up).timeout(PROBE_TIMEOUT).send().await {
            Ok(resp) => resp.status(),
            Err(_) => {
                up.record(&self.audit, Outcome::UpstreamUnreachable, None, None);
                return Probe::Accepted;
            }
        };
        match status {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                up.record(&self.audit, Outcome::UpstreamAuthFailed, None, Some(status));
                Probe::Rejected(status)
            }
            s if !s.is_success() => {
                up.record(&self.audit, Outcome::Error, None, Some(s));
                Probe::Accepted
            }
            _ => Probe::Accepted,
        }
    }

    #[tool(
        description = "List countdowns sorted by target time. Returns id, title, target_time, remaining_seconds, repeat, enabled."
    )]
    async fn list_countdowns(
        &self,
        Parameters(args): Parameters<ListArgs>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "within_hours": args.within_hours,
            "include_disabled": args.include_disabled,
        });
        let up = Upstream::new("tool:list_countdowns", Some(&parts), None, params);
        let mut url = self.endpoint(&["countdowns"]);
        if !args.include_disabled {
            url.query_pairs_mut().append_pair("enabled", "true");
        }
        let result = self
            .fetch::<Vec<Countdown>>(url, &up)
            .await
            .and_then(|items| {
                let items: Vec<Countdown> = match args.within_hours {
                    Some(h) => items.into_iter().filter(|c| due_within(c, h)).collect(),
                    None => items,
                };
                json_result(&items).map_err(|e| Failure {
                    outcome: Outcome::Error,
                    upstream_status: None,
                    message: e.message.to_string(),
                })
            });
        up.finish(&self.audit, result)
    }

    #[tool(
        description = "Get one countdown by id. Returns id, title, target_time, remaining_seconds, repeat, enabled."
    )]
    async fn get_countdown(
        &self,
        Parameters(GetArgs { id }): Parameters<GetArgs>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({ "id": id });
        let up = Upstream::new("tool:get_countdown", Some(&parts), Some(&id), params);
        let result = match self
            .fetch::<Countdown>(self.endpoint(&["countdowns", &id]), &up)
            .await
        {
            Ok(item) => json_result(&item).map_err(|e| Failure {
                outcome: Outcome::Error,
                upstream_status: None,
                message: e.message.to_string(),
            }),
            Err(mut f) => {
                f.message = format!("{} (id: {id})", f.message);
                Err(f)
            }
        };
        up.finish(&self.audit, result)
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
    let tokens = match TokenTable::load(&args.tokens) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("--tokens {}: {e}", args.tokens.display());
            std::process::exit(2);
        }
    };
    let token = match ServiceToken::load(&args.api_token_file) {
        Ok(token) => token,
        Err(e) => {
            eprintln!("--api-token-file {}: {e}", args.api_token_file.display());
            std::process::exit(2);
        }
    };
    // Audience isolation (spec 3.2): the API credential must not open /mcp.
    if tokens.lookup(token.plaintext()).is_some() {
        eprintln!("the API service token must not also be an MCP entry token (--tokens)");
        std::process::exit(2);
    }
    let audit = match AuditLog::open(&args.audit_log) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("--audit-log {}: {e}", args.audit_log.display());
            std::process::exit(2);
        }
    };
    let mcp = CountdownMcp::new(args.api_url, &token, audit.clone());
    if let Probe::Rejected(status) = mcp.probe().await {
        eprintln!("{}; check --api-token-file", rejected_credential(status));
        std::process::exit(2);
    }
    let entry = Arc::new(Entry { tokens, audit });
    let router = app(mcp, entry, args.addr.port());
    eprintln!("countdown_mcp listening on http://{}/mcp", args.addr);
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::{
        extract::{Path, Query},
        http::StatusCode,
        response::IntoResponse,
        routing::get,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

    fn get_args(id: &str) -> Parameters<GetArgs> {
        Parameters(GetArgs { id: id.into() })
    }

    const SERVICE_TOKEN: &str = "svc-token-0123456789abcdef0123456789abcdef";

    fn mcp(api_url: reqwest::Url) -> CountdownMcp {
        let path = std::env::temp_dir().join(format!("mcp-unit-{}.jsonl", uuid::Uuid::new_v4()));
        CountdownMcp::new(
            api_url,
            &ServiceToken::parse(SERVICE_TOKEN.as_bytes()).unwrap(),
            AuditLog::open(&path).unwrap(),
        )
    }

    /// Request parts as rmcp hands them to a tool, optionally with a caller.
    fn parts(caller: Option<&str>) -> Extension<Parts> {
        let (mut parts, ()) = axum::http::Request::new(()).into_parts();
        if let Some(c) = caller {
            parts.extensions.insert(Caller(c.to_owned()));
        }
        Extension(parts)
    }

    fn anon() -> Extension<Parts> {
        parts(None)
    }

    #[tokio::test]
    async fn unreachable_api_is_tool_error_not_protocol_error() {
        let mcp = mcp(unreachable_url().await);
        let r = mcp
            .list_countdowns(list(None, false), anon())
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("unreachable"), "{}", text(&r));
        let r = mcp.get_countdown(get_args("x"), anon()).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("unreachable"), "{}", text(&r));
    }

    #[tokio::test]
    async fn get_missing_is_tool_error_and_existing_succeeds() {
        let mcp = mcp(stub_api().await);
        let r = mcp.get_countdown(get_args("nope"), anon()).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("not found"), "{}", text(&r));

        let r = mcp.get_countdown(get_args("edge"), anon()).await.unwrap();
        assert_eq!(r.is_error, Some(false));
        let c: Countdown = serde_json::from_str(&text(&r)).unwrap();
        assert_eq!(c.remaining_seconds, Some(3600));
    }

    #[tokio::test]
    async fn id_cannot_escape_the_countdown_path() {
        let mcp = mcp(stub_api().await);
        let r = mcp
            .get_countdown(get_args("../countdowns"), anon())
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("not found"), "{}", text(&r));
    }

    #[tokio::test]
    async fn list_hides_disabled_by_default() {
        let mcp = mcp(stub_api().await);
        let r = mcp
            .list_countdowns(list(None, false), anon())
            .await
            .unwrap();
        assert!(!ids(&r).contains(&"off".to_string()));
        let r = mcp.list_countdowns(list(None, true), anon()).await.unwrap();
        assert!(ids(&r).contains(&"off".to_string()));
    }

    #[tokio::test]
    async fn within_hours_keeps_zero_to_limit_inclusive() {
        let mcp = mcp(stub_api().await);
        let r = mcp
            .list_countdowns(list(Some(1), false), anon())
            .await
            .unwrap();
        assert_eq!(ids(&r), vec!["now", "edge"]);
    }

    #[test]
    fn list_args_default_include_disabled_false() {
        let a: ListArgs = serde_json::from_value(json!({})).unwrap();
        assert!(!a.include_disabled);
        assert_eq!(a.within_hours, None);
    }

    /// API stub that answers every request with `status` and `body`, and
    /// records the headers of each request it receives.
    async fn recording_api(
        status: StatusCode,
        body: &'static str,
    ) -> (reqwest::Url, Arc<Mutex<Vec<HeaderMap>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback({
            let seen = seen.clone();
            move |headers: HeaderMap| {
                seen.lock().unwrap().push(headers);
                async move { (status, [("content-type", "application/json")], body) }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}").parse().unwrap(), seen)
    }

    const SECRET_BODY: &str = r#"{"error":{"code":"unauthorized","message":"BODY-MARKER-7f3a"}}"#;

    #[tokio::test]
    async fn every_api_request_carries_the_service_token() {
        let (url, seen) = recording_api(StatusCode::OK, "[]").await;
        let mcp = mcp(url);
        mcp.list_countdowns(list(None, false), anon())
            .await
            .unwrap();
        mcp.get_countdown(get_args("a"), anon()).await.unwrap();
        assert_eq!(mcp.probe().await, Probe::Accepted);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        for h in seen.iter() {
            assert_eq!(
                h.get("authorization").unwrap(),
                &format!("Bearer {SERVICE_TOKEN}")
            );
        }
    }

    #[tokio::test]
    async fn each_call_gets_a_fresh_request_id_and_forwards_the_caller() {
        let (url, seen) = recording_api(StatusCode::OK, "[]").await;
        let mcp = mcp(url);
        mcp.list_countdowns(list(None, false), parts(Some("agent-dev")))
            .await
            .unwrap();
        mcp.list_countdowns(list(None, false), anon())
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        let ids: Vec<uuid::Uuid> = seen
            .iter()
            .map(|h| h[X_REQUEST_ID].to_str().unwrap().parse().unwrap())
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert!(ids.iter().all(|id| id.get_version_num() == 4));
        assert_eq!(seen[0][X_COUNTDOWN_CALLER], "agent-dev");
        assert!(seen[1].get(X_COUNTDOWN_CALLER).is_none());
    }

    #[tokio::test]
    async fn rejected_credential_is_fixed_text_without_api_body() {
        for (status, code) in [
            (StatusCode::UNAUTHORIZED, 401),
            (StatusCode::FORBIDDEN, 403),
        ] {
            let (url, _) = recording_api(status, SECRET_BODY).await;
            let mcp = mcp(url);
            let expected =
                format!("countdown API rejected the MCP service credential (HTTP {code})");

            let r = mcp
                .list_countdowns(list(None, false), anon())
                .await
                .unwrap();
            assert_eq!(r.is_error, Some(true));
            assert_eq!(text(&r), expected);

            let r = mcp.get_countdown(get_args("a"), anon()).await.unwrap();
            assert_eq!(r.is_error, Some(true));
            assert!(text(&r).starts_with(&expected), "{}", text(&r));
            assert!(!text(&r).contains("BODY-MARKER"), "{}", text(&r));
        }
    }

    #[tokio::test]
    async fn other_api_errors_still_pass_the_body_through() {
        let (url, _) = recording_api(StatusCode::INTERNAL_SERVER_ERROR, SECRET_BODY).await;
        let r = mcp(url)
            .list_countdowns(list(None, false), anon())
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("BODY-MARKER"), "{}", text(&r));
    }

    #[tokio::test]
    async fn probe_fails_only_on_rejected_credential() {
        for (status, expected) in [
            (StatusCode::OK, Probe::Accepted),
            (StatusCode::INTERNAL_SERVER_ERROR, Probe::Accepted),
            (
                StatusCode::UNAUTHORIZED,
                Probe::Rejected(StatusCode::UNAUTHORIZED),
            ),
            (
                StatusCode::FORBIDDEN,
                Probe::Rejected(StatusCode::FORBIDDEN),
            ),
        ] {
            let (url, _) = recording_api(status, SECRET_BODY).await;
            assert_eq!(mcp(url).probe().await, expected, "{status}");
        }
        assert_eq!(mcp(unreachable_url().await).probe().await, Probe::Accepted);
    }

    fn header_of(bytes: &[u8]) -> HeaderValue {
        ServiceToken::parse(bytes).unwrap().0
    }

    #[test]
    fn token_file_trailing_newline_is_stripped_once() {
        let plain = header_of(SERVICE_TOKEN.as_bytes());
        assert_eq!(plain, format!("Bearer {SERVICE_TOKEN}"));
        for suffix in ["\n", "\r\n"] {
            let raw = format!("{SERVICE_TOKEN}{suffix}");
            assert_eq!(header_of(raw.as_bytes()), plain, "{suffix:?}");
        }
        assert!(plain.is_sensitive());
    }

    #[test]
    fn token_file_rejects_empty_and_non_token_content() {
        for raw in ["", "\n", "\r\n"] {
            assert!(
                matches!(
                    ServiceToken::parse(raw.as_bytes()),
                    Err(TokenFileError::Empty)
                ),
                "{raw:?}"
            );
        }
        for raw in [
            "abc def", "abc\tdef", "abc\n\n", "abc\ndef", "abc\r", " abc", "abc \n", "tök",
        ] {
            let err = ServiceToken::parse(raw.as_bytes()).unwrap_err();
            assert!(matches!(err, TokenFileError::InvalidChars), "{raw:?}");
            assert!(!err.to_string().contains("abc"), "{err}");
        }
    }

    #[test]
    fn service_token_debug_is_redacted() {
        let token = ServiceToken::parse(SERVICE_TOKEN.as_bytes()).unwrap();
        let dump = format!("{token:?}");
        assert!(!dump.contains(SERVICE_TOKEN), "{dump}");
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_not_be_group_or_world_accessible() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.token");
        std::fs::write(&path, format!("{SERVICE_TOKEN}\n")).unwrap();
        for (mode, ok) in [
            (0o600, true),
            (0o400, true),
            (0o640, false),
            (0o644, false),
            (0o604, false),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let r = ServiceToken::load(&path);
            assert_eq!(r.is_ok(), ok, "{mode:o}");
            if let Err(e) = r {
                assert!(!e.to_string().contains(SERVICE_TOKEN), "{e}");
            }
        }
    }
}
