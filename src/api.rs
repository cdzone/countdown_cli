use std::{collections::HashMap, sync::Arc, time::Instant};

use axum::{
    body::{to_bytes, Body},
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{Local, NaiveDateTime};
use countdown_cli::{
    audit::{truncated, AuditLog},
    auth::{bearer_challenge, Principal, Scope, TokenTable},
    CountdownUpdateError,
};
use serde_json::{json, Value};

use countdown_cli::{calculate_target_time, next_occurrence, Countdown, CountdownStore, RepeatMode};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiCountdown {
    pub id: String,
    pub title: String,
    pub datetime: String,
    pub enabled: bool,
    pub repeat: Option<String>, // unified to "weekday" if present
    /// Current-period target (elapsed display). Unchanged semantics.
    pub target_time: Option<String>,
    /// Seconds until/since [`Self::target_time`] (negative after the period's point).
    pub remaining_seconds: Option<i64>,
    /// Next occurrence target for Agent "which is next" (from [`next_occurrence`]).
    pub next_target_time: Option<String>,
    /// Seconds until [`Self::next_target_time`] (≥ 0 for recurring modes that still fire).
    pub next_remaining_seconds: Option<i64>,
}

impl ApiCountdown {
    fn from_domain(countdown: &Countdown, now: NaiveDateTime) -> Option<Self> {
        let id = countdown.id.as_ref()?.clone();
        let target = calculate_target_time(&countdown.datetime, countdown.repeat.as_ref(), now);
        let remaining_seconds = target.map(|t| (t - now).num_seconds());
        let next = next_occurrence(&countdown.datetime, countdown.repeat.as_ref(), now);
        let next_remaining_seconds = next.map(|t| (t - now).num_seconds());
        Some(Self {
            id,
            title: countdown.title.clone(),
            datetime: countdown.datetime.clone(),
            enabled: countdown.enabled,
            repeat: countdown.repeat.as_ref().map(|r| match r {
                RepeatMode::Daily => "daily".to_string(),
                RepeatMode::Weekday => "weekday".to_string(),
            }),
            target_time: target.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
            remaining_seconds,
            next_target_time: next.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
            next_remaining_seconds,
        })
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateOrUpdateRequest {
    pub title: String,
    pub datetime: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub repeat: Option<String>, // accept "daily" / "weekday" / "workday"
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorInner,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiErrorInner {
    pub code: &'static str,
    pub message: String,
}

fn api_error(code: &'static str, message: impl Into<String>, status: StatusCode) -> Response {
    let body = ApiErrorBody {
        error: ApiErrorInner {
            code,
            message: message.into(),
        },
    };
    (status, Json(body)).into_response()
}

fn internal_error<E: std::fmt::Display>(e: E) -> Response {
    api_error("internal", e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<CountdownStore>,
}

/// Who may call the API, and where each call is recorded.
#[derive(Clone)]
pub struct Gate {
    tokens: Arc<TokenTable>,
    audit: AuditLog,
}

impl Gate {
    pub fn new(tokens: TokenTable, audit: AuditLog) -> Self {
        Self {
            tokens: Arc::new(tokens),
            audit,
        }
    }
}

pub fn build_router(store: Arc<CountdownStore>, gate: Gate) -> Router {
    Router::new()
        .route("/countdowns", get(list_countdowns).post(create_countdown))
        .route(
            "/countdowns/{id}",
            get(get_countdown)
                .put(update_countdown)
                .delete(delete_countdown),
        )
        .with_state(ApiState { store })
        // `layer`, not `route_layer`: unknown paths and methods must hit the
        // gate too, so they answer 401/403 instead of 404/405.
        .layer(middleware::from_fn_with_state(gate, guard))
}

const X_REQUEST_ID: &str = "x-request-id";
const X_COUNTDOWN_CALLER: &str = "x-countdown-caller";
const X_AUDIT_STATUS: &str = "x-audit-status";
/// Same cap axum's `Json` extractor applies by default.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// Body fields whose presence is recorded; values never are.
const WRITE_FIELDS: [&str; 4] = ["title", "datetime", "enabled", "repeat"];

/// Route template for `path` plus the countdown id in it, if the path is one
/// the API serves.
fn route_of(path: &str) -> Option<(&'static str, Option<&str>)> {
    match path.strip_prefix("/countdowns")? {
        "" => Some(("/countdowns", None)),
        rest => match rest.strip_prefix('/') {
            Some(id) if !id.is_empty() && !id.contains('/') => Some(("/countdowns/{id}", Some(id))),
            _ => None,
        },
    }
}

/// The single place that maps routes to scopes. Anything not listed is
/// refused, so a route added later stays unusable until it is listed here.
fn required_scope(method: &Method, route: &str) -> Option<Scope> {
    match (method.as_str(), route) {
        ("GET", "/countdowns" | "/countdowns/{id}") => Some(Scope::CountdownsRead),
        ("POST", "/countdowns") | ("PUT" | "DELETE", "/countdowns/{id}") => {
            Some(Scope::CountdownsWrite)
        }
        _ => None,
    }
}

enum AuthFail {
    Missing,
    /// Not a bearer credential, or `Bearer` with no token after it.
    Malformed {
        bearer: bool,
    },
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
    fn response(&self) -> Response {
        let (invalid_token, message) = match self {
            AuthFail::Missing => (false, "missing bearer token"),
            AuthFail::Malformed { bearer } => (*bearer, "malformed authorization header"),
            AuthFail::UnknownToken => (true, "invalid bearer token"),
        };
        let challenge = bearer_challenge(invalid_token, Some(Scope::CountdownsRead));
        with_challenge(
            api_error("unauthorized", message, StatusCode::UNAUTHORIZED),
            challenge,
        )
    }
}

fn authenticate<'t>(
    tokens: &'t TokenTable,
    headers: &HeaderMap,
) -> Result<&'t Principal, AuthFail> {
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
        token => tokens.lookup(token).ok_or(AuthFail::UnknownToken),
    }
}

fn forbidden(scope: Option<Scope>) -> Response {
    let challenge = match scope {
        Some(scope) => format!(
            "Bearer error=\"insufficient_scope\", scope=\"{}\"",
            scope.as_str()
        ),
        None => "Bearer error=\"insufficient_scope\"".to_string(),
    };
    with_challenge(
        api_error("forbidden", "insufficient scope", StatusCode::FORBIDDEN),
        challenge,
    )
}

fn with_challenge(mut res: Response, challenge: String) -> Response {
    if let Ok(value) = HeaderValue::try_from(challenge) {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    res
}

/// One audit record. A write leaves an `attempt` line before it runs and a
/// `result` line after; everything else leaves only the `result` line.
#[derive(serde::Serialize)]
struct AuditLine {
    ts: String,
    svc: &'static str,
    phase: &'static str,
    request_id: String,
    caller: Option<String>,
    on_behalf_of: Option<String>,
    action: String,
    target: Option<String>,
    params: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_fail: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

impl AuditLine {
    fn new(req: &Request) -> Self {
        let headers = req.headers();
        let request_id = headers
            .get(X_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| uuid::Uuid::parse_str(v).ok())
            .unwrap_or_else(uuid::Uuid::new_v4)
            .to_string();
        let on_behalf_of = headers
            .get(X_COUNTDOWN_CALLER)
            .and_then(|v| v.to_str().ok())
            .map(truncated);
        let route = route_of(req.uri().path());
        let params = match route {
            Some(("/countdowns", _)) if req.method() == Method::GET => read_params(req.uri()),
            _ => Value::Null,
        };
        Self {
            ts: Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            svc: "api",
            phase: "result",
            request_id,
            caller: None,
            on_behalf_of,
            action: format!("{} {}", req.method(), route.map_or("<unmapped>", |r| r.0)),
            target: route.and_then(|r| r.1).map(truncated),
            params,
            outcome: None,
            auth_fail: None,
            status: None,
            duration_ms: None,
        }
    }
}

fn read_params(uri: &Uri) -> Value {
    let query = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default();
    match query.get("enabled") {
        Some(v) => json!({ "enabled": truncated(v) }),
        None => json!({}),
    }
}

/// Which known fields a write body carried, plus the title length; never
/// the values themselves.
fn write_params(body: &[u8]) -> Value {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return Value::Null;
    };
    let present: Vec<&str> = WRITE_FIELDS
        .into_iter()
        .filter(|f| fields.contains_key(*f))
        .collect();
    let title_len = fields
        .get("title")
        .and_then(Value::as_str)
        .map(|t| t.chars().count());
    json!({ "fields": present, "title_len": title_len })
}

fn outcome_of(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200..=299 => "ok",
        401 => "unauthenticated",
        403 => "forbidden",
        404 => "not_found",
        400..=499 => "invalid",
        _ => "error",
    }
}

impl Gate {
    /// Writes the `result` line; if that fails the response still goes out,
    /// flagged with `X-Audit-Status: unavailable`.
    fn finish(
        &self,
        mut line: AuditLine,
        mut res: Response,
        auth_fail: Option<&'static str>,
        started: Instant,
    ) -> Response {
        line.phase = "result";
        line.outcome = Some(outcome_of(res.status()));
        line.auth_fail = auth_fail;
        line.status = Some(res.status().as_u16());
        line.duration_ms = Some(started.elapsed().as_millis() as u64);
        if self.audit.append(&line).is_err() {
            res.headers_mut()
                .insert(X_AUDIT_STATUS, HeaderValue::from_static("unavailable"));
        }
        res
    }
}

/// Authenticates, checks scope, and audits every request before routing.
async fn guard(State(gate): State<Gate>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let mut line = AuditLine::new(&req);
    let principal = match authenticate(&gate.tokens, req.headers()) {
        Ok(principal) => principal,
        Err(fail) => return gate.finish(line, fail.response(), Some(fail.audit_name()), started),
    };
    line.caller = Some(principal.id().to_owned());
    let scope =
        route_of(req.uri().path()).and_then(|(route, _)| required_scope(req.method(), route));
    let Some(scope) = scope.filter(|s| principal.allows(*s)) else {
        return gate.finish(line, forbidden(scope), Some("insufficient_scope"), started);
    };
    if scope != Scope::CountdownsWrite {
        let res = next.run(req).await;
        return gate.finish(line, res, None, started);
    }

    let (parts, body) = req.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        let res = api_error(
            "payload_too_large",
            "request body too large",
            StatusCode::PAYLOAD_TOO_LARGE,
        );
        return gate.finish(line, res, None, started);
    };
    line.phase = "attempt";
    line.params = write_params(&body);
    // No attempt line, no write: an unrecorded change must not happen.
    if gate.audit.append(&line).is_err() {
        return api_error(
            "audit_unavailable",
            "audit log unavailable",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    let res = next.run(Request::from_parts(parts, Body::from(body))).await;
    gate.finish(line, res, None, started)
}

// no ListQuery; we parse query via HashMap

async fn list_countdowns(
    State(state): State<ApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let enabled_filter = query.get("enabled").and_then(|v| v.parse::<bool>().ok());
    let now = Local::now().naive_local();
    let data = state.store.read_data().await;
    let mut items: Vec<ApiCountdown> = data
        .countdown
        .iter()
        .filter(|c| enabled_filter.map(|f| c.enabled == f).unwrap_or(true))
        .filter_map(|c| ApiCountdown::from_domain(c, now))
        .collect();
    items.sort_by_key(|a| {
        a.target_time
            .as_ref()
            .map(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap())
    });
    Json(items)
}

async fn get_countdown(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let now = Local::now().naive_local();
    let data = state.store.read_data().await;
    if let Some(found) = data.countdown.iter().find(|c| c.id.as_deref() == Some(&id)) {
        if let Some(api) = ApiCountdown::from_domain(found, now) {
            return Json(api).into_response();
        }
    }
    api_error("not_found", "Countdown not found", StatusCode::NOT_FOUND)
}

fn parse_repeat(s: &str) -> Option<RepeatMode> {
    match s.to_lowercase().as_str() {
        "daily" => Some(RepeatMode::Daily),
        "weekday" | "workday" => Some(RepeatMode::Weekday),
        _ => None,
    }
}

fn validate_and_build_domain(
    body: &CreateOrUpdateRequest,
) -> Result<(String, bool, Option<RepeatMode>), (&'static str, String)> {
    if body.title.trim().is_empty() {
        return Err(("invalid_body", "title must not be empty".into()));
    }
    // datetime format
    if NaiveDateTime::parse_from_str(&body.datetime, "%Y-%m-%d %H:%M:%S").is_err() {
        return Err(("invalid_datetime", "invalid datetime format".into()));
    }
    // repeat
    let repeat = match &body.repeat {
        None => Ok(None),
        Some(s) => parse_repeat(s)
            .map(Some)
            .ok_or(("invalid_repeat", "invalid repeat".into())),
    }?;
    Ok((body.title.clone(), body.enabled, repeat))
}

async fn create_countdown(
    State(state): State<ApiState>,
    Json(body): Json<CreateOrUpdateRequest>,
) -> impl IntoResponse {
    let (title, enabled, repeat) = match validate_and_build_domain(&body) {
        Ok(v) => v,
        Err((code, msg)) => return api_error(code, msg, StatusCode::UNPROCESSABLE_ENTITY),
    };
    let datetime = body.datetime.clone();
    match state
        .store
        .api_create(title, datetime, enabled, repeat)
        .await
    {
        Ok(created) => {
            let now = Local::now().naive_local();
            let api = ApiCountdown::from_domain(&created, now).unwrap_or(ApiCountdown {
                id: created.id.clone().unwrap_or_default(),
                title: created.title.clone(),
                datetime: created.datetime.clone(),
                enabled: created.enabled,
                repeat: created
                    .repeat
                    .as_ref()
                    .map(|r| {
                        if matches!(r, RepeatMode::Daily) {
                            "daily"
                        } else {
                            "weekday"
                        }
                    })
                    .map(|s| s.to_string()),
                target_time: None,
                remaining_seconds: None,
                next_target_time: None,
                next_remaining_seconds: None,
            });
            (StatusCode::CREATED, Json(api)).into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn update_countdown(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<CreateOrUpdateRequest>,
) -> impl IntoResponse {
    let (title, enabled, repeat) = match validate_and_build_domain(&body) {
        Ok(v) => v,
        Err((code, msg)) => return api_error(code, msg, StatusCode::UNPROCESSABLE_ENTITY),
    };
    let datetime = body.datetime.clone();
    match state
        .store
        .api_update(&id, title, datetime, enabled, repeat)
        .await
    {
        Ok(updated) => {
            let now = Local::now().naive_local();
            let api = ApiCountdown::from_domain(&updated, now).unwrap();
            Json(api).into_response()
        }
        Err(CountdownUpdateError::NotFound) => {
            api_error("not_found", "Countdown not found", StatusCode::NOT_FOUND)
        }
        Err(CountdownUpdateError::Other(e)) => internal_error(e),
    }
}

async fn delete_countdown(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.api_delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error("not_found", "Countdown not found", StatusCode::NOT_FOUND),
        Err(e) => internal_error(e),
    }
}

// no-op: keep module private items

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use std::{fs, io::Write};
    use tempfile::tempdir;
    use tower::ServiceExt;

    const WRITE_TOKEN: &str = "write-token-0123456789abcdef0123456789abcdef";
    const READ_TOKEN: &str = "read-token-0123456789abcdef0123456789abcdef0";
    const WRITE_AUTH: &str = "Bearer write-token-0123456789abcdef0123456789abcdef";
    const READ_AUTH: &str = "Bearer read-token-0123456789abcdef0123456789abcdef0";

    #[test]
    fn from_domain_daily_after_target_keeps_elapsed_and_adds_next() {
        let now = NaiveDateTime::parse_from_str("2025-01-15 19:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let c = Countdown {
            id: Some("daily".into()),
            title: "standup".into(),
            datetime: "2024-08-30 18:00:00".into(),
            enabled: true,
            repeat: Some(RepeatMode::Daily),
        };
        let api = ApiCountdown::from_domain(&c, now).unwrap();
        assert_eq!(api.target_time.as_deref(), Some("2025-01-15 18:00:00"));
        assert_eq!(api.remaining_seconds, Some(-3600));
        assert_eq!(api.next_target_time.as_deref(), Some("2025-01-16 18:00:00"));
        assert_eq!(api.next_remaining_seconds, Some(23 * 3600));
    }

    #[test]
    fn from_domain_weekday_weekend_next_is_monday() {
        let now = NaiveDateTime::parse_from_str("2025-01-18 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let c = Countdown {
            id: Some("wd".into()),
            title: "standup".into(),
            datetime: "2024-08-30 18:00:00".into(),
            enabled: true,
            repeat: Some(RepeatMode::Weekday),
        };
        let api = ApiCountdown::from_domain(&c, now).unwrap();
        // Elapsed display still Friday; next occurrence Monday.
        assert_eq!(api.target_time.as_deref(), Some("2025-01-17 18:00:00"));
        assert!(api.remaining_seconds.unwrap() < 0);
        assert_eq!(api.next_target_time.as_deref(), Some("2025-01-20 18:00:00"));
        assert!(api.next_remaining_seconds.unwrap() > 0);
    }

    fn sha256_hex(s: &str) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn test_gate(audit: &std::path::Path) -> Gate {
        let table = format!(
            "[[token]]\nid = \"writer\"\nsha256 = \"{}\"\nscopes = [\"countdowns:write\"]\n\
             [[token]]\nid = \"reader\"\nsha256 = \"{}\"\nscopes = [\"countdowns:read\"]\n",
            sha256_hex(WRITE_TOKEN),
            sha256_hex(READ_TOKEN)
        );
        Gate::new(
            TokenTable::parse(&table).unwrap(),
            AuditLog::open(audit).unwrap(),
        )
    }

    fn audit_path(config: &std::path::Path) -> std::path::PathBuf {
        config.with_file_name("audit.jsonl")
    }

    /// Every audit line, each parsed on its own.
    fn audit_lines(config: &std::path::Path) -> Vec<serde_json::Value> {
        fs::read_to_string(audit_path(config))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("each audit line is JSON"))
            .collect()
    }

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: String,
    }

    impl Reply {
        fn challenge(&self) -> &str {
            self.headers
                .get("www-authenticate")
                .map(|v| v.to_str().unwrap())
                .unwrap_or("")
        }
        fn code(&self) -> String {
            let v: serde_json::Value = serde_json::from_str(&self.body).unwrap();
            v["error"]["code"].as_str().unwrap().to_string()
        }
    }

    async fn send(
        router: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Reply {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if !body.is_empty() {
            req = req.header("content-type", "application/json");
        }
        let res = router
            .clone()
            .oneshot(req.body(axum::body::Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    const GOOD_BODY: &str = r#"{"title":"T","datetime":"2026-11-01 12:00:00"}"#;

    async fn first_id(router: &Router) -> String {
        let r = send(
            router,
            "GET",
            "/countdowns",
            &[("authorization", READ_AUTH)],
            "",
        )
        .await;
        let list: Vec<ApiCountdown> = serde_json::from_str(&r.body).unwrap();
        list[0].id.clone()
    }

    fn sample_toml() -> String {
        r#"# sample demo

[[countdown]]
title = "A"
datetime = "2026-12-31 18:00:00"
# keep comment

[[countdown]]
title = "B"
datetime = "2026-01-02 09:00:00"
repeat = "workday"
"#
        .to_string()
    }

    async fn setup_router_with_file(
        content: &str,
    ) -> (
        Router,
        Arc<CountdownStore>,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        let store = CountdownStore::new(path.clone(), true).await.unwrap();
        let router = build_router(store.clone(), test_gate(&audit_path(&path)));
        (router, store, dir, path)
    }

    #[tokio::test]
    async fn post_get_put_delete_happy_path() {
        let (router, _store, _dir, _path) = setup_router_with_file(&sample_toml()).await;
        // POST
        let body = serde_json::json!({
            "title": "C",
            "datetime": "2026-11-01 12:00:00",
            "enabled": true,
            "repeat": "daily"
        });
        let res = router
            .clone()
            .oneshot(
                Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let created: ApiCountdown = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(created.title, "C");
        assert!(!created.id.is_empty());
        // GET single
        let res = router
            .clone()
            .oneshot(
                Request::get(format!("/countdowns/{}", created.id))
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // PUT
        let body = serde_json::json!({
            "title": "C2",
            "datetime": "2026-11-02 12:30:00",
            "enabled": false,
            "repeat": "weekday"
        });
        let res = router
            .clone()
            .oneshot(
                Request::put(format!("/countdowns/{}", created.id))
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let updated: ApiCountdown = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(updated.title, "C2");
        // DELETE
        let res = router
            .clone()
            .oneshot(
                Request::delete(format!("/countdowns/{}", updated.id))
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn error_cases_return_codes() {
        let (router, _store, _dir, _path) = setup_router_with_file(&sample_toml()).await;
        // GET 404
        let res = router
            .clone()
            .oneshot(
                Request::get("/countdowns/does-not-exist")
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        #[derive(serde::Deserialize)]
        struct TestErr {
            error: TestErrInner,
        }
        #[derive(serde::Deserialize)]
        struct TestErrInner {
            code: String,
            message: String,
        }
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "not_found");
        // POST invalid datetime
        let bad = serde_json::json!({"title":"X","datetime":"BAD","enabled":true});
        let res = router
            .clone()
            .oneshot(
                Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "invalid_datetime");
        // POST invalid repeat
        let bad = serde_json::json!({"title":"X","datetime":"2026-01-01 00:00:00","repeat":"x"});
        let res = router
            .clone()
            .oneshot(
                Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "invalid_repeat");
        // POST empty title
        let bad = serde_json::json!({"title":"","datetime":"2026-01-01 00:00:00"});
        let res = router
            .clone()
            .oneshot(
                Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "invalid_body");
        // PUT 404
        let good = serde_json::json!({"title":"X","datetime":"2026-01-01 00:00:00"});
        let res = router
            .clone()
            .oneshot(
                Request::put("/countdowns/nope")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&good).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "not_found");
        // DELETE 404
        let res = router
            .clone()
            .oneshot(
                Request::delete("/countdowns/nope")
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "not_found");
    }

    #[tokio::test]
    async fn id_injection_backup_and_comments_preserved() {
        let content = sample_toml();
        let (router, _store, _dir, path) = setup_router_with_file(&content).await;
        // on initialization with api_mode=true, .bak should exist and equal original
        let bak = path.with_extension("toml.bak");
        assert!(bak.exists());
        assert_eq!(fs::read_to_string(bak).unwrap(), content);
        // After id injection, the file should be identical except new id lines
        let written = fs::read_to_string(&path).unwrap();
        let without_id: Vec<&str> = written
            .lines()
            .filter(|l| !l.trim_start().starts_with("id ="))
            .collect();
        let orig_lines: Vec<&str> = content.lines().collect();
        assert_eq!(without_id, orig_lines);
        // Ensure GET returns items with id
        let res = router
            .clone()
            .oneshot(
                Request::get("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let list: Vec<ApiCountdown> = serde_json::from_slice(&bytes).unwrap();
        assert!(list.iter().all(|c| !c.id.is_empty()));
    }

    #[tokio::test]
    async fn manual_append_then_hot_reload_makes_new_item_visible_with_id() {
        let (router, store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        // 文件修改时间有粒度：先等一下，保证手改后的 mtime 和启动时不同
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // append one item manually (no id)
        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                r#"
[[countdown]]
title = "C"
datetime = "2026-10-11 10:20:30"
"#
            )
            .unwrap();
        }
        assert!(store.reload_if_changed().await.unwrap());
        // list should contain C with id
        let res = router
            .oneshot(
                Request::get("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let list: Vec<ApiCountdown> = serde_json::from_slice(&bytes).unwrap();
        let item = list
            .iter()
            .find(|it| it.title == "C")
            .expect("new item exists");
        assert!(!item.id.is_empty());
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains(&format!("id = \"{}\"", item.id)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_posts_produce_exact_count_and_persist() {
        // initial file with one countdown
        let initial = r#"
[[countdown]]
title = "only-one"
datetime = "2026-12-31 00:00:00"
"#;
        let (router, store, _dir, path) = setup_router_with_file(initial).await;

        // spawn 20 concurrent POST
        let mut joins = Vec::new();
        for i in 0..20 {
            let app = router.clone();
            let body = serde_json::json!({
                "title": format!("n{idx}", idx=i),
                "datetime": "2026-10-22 12:34:56",
                "enabled": true,
                "repeat": "daily"
            });
            joins.push(tokio::spawn(async move {
                let req = Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();
                let _ = app.clone().oneshot(req).await.unwrap();
            }));
        }
        for j in joins {
            j.await.unwrap();
        }

        // read back file and memory
        let contents = fs::read_to_string(&path).unwrap();
        let doc = contents.parse::<toml_edit::DocumentMut>().unwrap();
        let len_file = doc
            .get("countdown")
            .and_then(|it| it.as_array_of_tables())
            .map(|arr| arr.len())
            .unwrap_or(0);
        let len_mem = store.read_data().await.countdown.len();
        assert_eq!(len_file, 21, "file should have 21 items");
        assert_eq!(len_mem, 21, "memory should have 21 items");
    }

    #[tokio::test]
    async fn io_error_returns_500_internal() {
        // create file and then remove write permission on dir to force IO error
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, sample_toml()).unwrap();
        let store = CountdownStore::new(path.clone(), true).await.unwrap();
        let audit = tempdir().unwrap();
        let router = build_router(store.clone(), test_gate(&audit.path().join("a.jsonl")));
        // chmod dir to read/execute only (UNIX), after router created to force POST failure
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(dir.path()).unwrap().permissions();
            perm.set_mode(0o555);
            fs::set_permissions(dir.path(), perm).unwrap();
        }
        let body = serde_json::json!({
            "title": "will-fail",
            "datetime": "2026-10-22 12:34:56",
            "enabled": true
        });
        let res = router
            .oneshot(
                Request::post("/countdowns")
                    .header("authorization", WRITE_AUTH)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        #[derive(serde::Deserialize)]
        struct E {
            error: Code,
        }
        #[derive(serde::Deserialize)]
        struct Code {
            code: String,
        }
        let e: E = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(e.error.code, "internal");
    }

    #[tokio::test]
    async fn every_route_without_a_token_is_401_and_audited() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let routes = [
            ("GET", "/countdowns"),
            ("POST", "/countdowns"),
            ("GET", "/countdowns/x"),
            ("PUT", "/countdowns/x"),
            ("DELETE", "/countdowns/x"),
        ];
        for (method, uri) in routes {
            let r = send(&router, method, uri, &[], GOOD_BODY).await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert_eq!(
                r.challenge(),
                r#"Bearer realm="countdown", scope="countdowns:read""#
            );
            assert_eq!(r.code(), "unauthorized");
        }
        let lines = audit_lines(&path);
        assert_eq!(lines.len(), 5);
        for line in lines {
            assert_eq!(line["phase"], "result");
            assert_eq!(line["outcome"], "unauthenticated");
            assert_eq!(line["auth_fail"], "missing");
            assert_eq!(line["caller"], serde_json::Value::Null);
            assert_eq!(line["status"], 401);
        }
    }

    #[tokio::test]
    async fn unknown_token_is_401_even_on_unknown_paths() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let token = "not-a-known-token-7c1d";
        let auth = format!("Bearer {token}");
        for uri in ["/countdowns", "/nope"] {
            let r = send(&router, "GET", uri, &[("authorization", &auth)], "").await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
            assert_eq!(r.code(), "unauthorized");
            assert!(r.challenge().contains(r#"error="invalid_token""#));
            assert!(!r.body.contains(token));
        }
        let lines = audit_lines(&path);
        assert!(lines.iter().all(|l| l["auth_fail"] == "unknown_token"));
        assert_eq!(lines[1]["action"], "GET <unmapped>");
    }

    #[tokio::test]
    async fn malformed_authorization_is_401_not_400() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let basic = send(
            &router,
            "GET",
            "/countdowns",
            &[("authorization", "Basic eHh4")],
            "",
        )
        .await;
        assert_eq!(basic.status, StatusCode::UNAUTHORIZED);
        assert!(
            !basic.challenge().contains("error="),
            "{}",
            basic.challenge()
        );
        for bare in ["Bearer", "Bearer   "] {
            let r = send(
                &router,
                "GET",
                "/countdowns",
                &[("authorization", bare)],
                "",
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{bare:?}");
            assert_eq!(
                r.challenge(),
                r#"Bearer realm="countdown", error="invalid_token", scope="countdowns:read""#
            );
        }
        let lines = audit_lines(&path);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l["auth_fail"] == "malformed"));
    }

    #[tokio::test]
    async fn read_token_cannot_write_and_nothing_changes() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let id = first_id(&router).await;
        let before = fs::read(&path).unwrap();
        let item = format!("/countdowns/{id}");
        let cases = [
            ("POST", "/countdowns", GOOD_BODY),
            ("POST", "/countdowns", "{not json"),
            ("PUT", item.as_str(), GOOD_BODY),
            ("DELETE", item.as_str(), ""),
        ];
        for (method, uri, body) in cases {
            let r = send(&router, method, uri, &[("authorization", READ_AUTH)], body).await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {body}");
            assert_eq!(r.code(), "forbidden");
            assert_eq!(
                r.challenge(),
                r#"Bearer error="insufficient_scope", scope="countdowns:write""#
            );
        }
        assert_eq!(fs::read(&path).unwrap(), before);
        let denied: Vec<_> = audit_lines(&path)
            .into_iter()
            .filter(|l| l["status"] == 403)
            .collect();
        assert_eq!(denied.len(), 4);
        for line in denied {
            assert_eq!(line["phase"], "result");
            assert_eq!(line["outcome"], "forbidden");
            assert_eq!(line["auth_fail"], "insufficient_scope");
            assert_eq!(line["caller"], "reader");
        }
    }

    #[tokio::test]
    async fn unmapped_methods_and_paths_are_403_for_valid_tokens() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let id = first_id(&router).await;
        let before = fs::read(&path).unwrap();
        let item = format!("/countdowns/{id}");
        for (method, uri) in [
            ("PATCH", item.as_str()),
            ("GET", "/nope"),
            ("DELETE", "/countdowns"),
        ] {
            let r = send(
                &router,
                method,
                uri,
                &[("authorization", WRITE_AUTH)],
                GOOD_BODY,
            )
            .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {uri}");
            assert_eq!(r.code(), "forbidden");
            assert_eq!(r.challenge(), r#"Bearer error="insufficient_scope""#);
        }
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_write_leaves_attempt_then_result_under_one_request_id() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let id = first_id(&router).await;
        let r = send(
            &router,
            "DELETE",
            &format!("/countdowns/{id}"),
            &[
                ("authorization", WRITE_AUTH),
                ("x-countdown-caller", "agent-dev"),
            ],
            "",
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        let lines = audit_lines(&path);
        let [.., attempt, result] = lines.as_slice() else {
            panic!("expected two lines for the write: {lines:?}");
        };
        assert_eq!(attempt["phase"], "attempt");
        assert_eq!(result["phase"], "result");
        assert_eq!(attempt["request_id"], result["request_id"]);
        for key in ["status", "outcome", "duration_ms"] {
            assert!(attempt.get(key).is_none(), "attempt has {key}");
            assert!(result.get(key).is_some(), "result lacks {key}");
        }
        assert_eq!(result["action"], "DELETE /countdowns/{id}");
        assert_eq!(result["target"], id.as_str());
        assert_eq!(result["caller"], "writer");
        assert_eq!(result["on_behalf_of"], "agent-dev");
        assert_eq!(result["svc"], "api");
        assert_eq!(result["outcome"], "ok");
        assert_eq!(result["status"], 204);
        assert!(result["ts"].as_str().is_some());
    }

    #[tokio::test]
    async fn reads_keep_an_incoming_request_id_and_log_one_line() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let rid = "6f0c2a3e-1b2c-4d5e-8f90-123456789abc";
        let long_caller = "c".repeat(100);
        let r = send(
            &router,
            "GET",
            "/countdowns?enabled=true",
            &[
                ("authorization", READ_AUTH),
                ("x-request-id", rid),
                ("x-countdown-caller", &long_caller),
            ],
            "",
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.headers.get("x-audit-status").is_none());
        let lines = audit_lines(&path);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line["phase"], "result");
        assert_eq!(line["request_id"], rid);
        assert_eq!(line["action"], "GET /countdowns");
        assert_eq!(line["params"], serde_json::json!({"enabled": "true"}));
        assert_eq!(line["on_behalf_of"].as_str().unwrap().len(), 64);
        // A request id that is not a UUID is replaced, not trusted.
        send(
            &router,
            "GET",
            "/countdowns",
            &[("authorization", READ_AUTH), ("x-request-id", "x y")],
            "",
        )
        .await;
        let fresh = audit_lines(&path)[1]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(uuid::Uuid::parse_str(&fresh).is_ok());
    }

    #[tokio::test]
    async fn audit_never_holds_tokens_digests_or_titles() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let marker = "TITLE-MARKER-91ab";
        let body = format!(r#"{{"title":"{marker}","datetime":"2026-11-01 12:00:00"}}"#);
        let r = send(
            &router,
            "POST",
            "/countdowns",
            &[("authorization", WRITE_AUTH)],
            &body,
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED);
        send(
            &router,
            "GET",
            "/countdowns",
            &[("authorization", READ_AUTH)],
            "",
        )
        .await;
        send(
            &router,
            "GET",
            "/countdowns",
            &[("authorization", "Bearer leaked-guess-55")],
            "",
        )
        .await;
        let text = fs::read_to_string(audit_path(&path)).unwrap();
        for secret in [WRITE_TOKEN, READ_TOKEN, "leaked-guess-55", marker] {
            assert!(!text.contains(secret), "{secret} in audit");
        }
        for secret in [WRITE_TOKEN, READ_TOKEN] {
            assert!(!text.contains(&sha256_hex(secret)), "digest in audit");
        }
        let attempt = &audit_lines(&path)[0];
        assert_eq!(
            attempt["params"],
            serde_json::json!({"fields": ["title", "datetime"], "title_len": marker.len()})
        );
    }

    /// Makes the audit path unwritable by putting a directory there.
    fn break_audit(config: &std::path::Path) {
        let audit = audit_path(config);
        let _ = fs::remove_file(&audit);
        fs::create_dir(&audit).unwrap();
    }

    #[tokio::test]
    async fn without_an_attempt_line_a_write_is_refused() {
        let (router, _store, _dir, path) = setup_router_with_file(&sample_toml()).await;
        let id = first_id(&router).await;
        let before = fs::read(&path).unwrap();
        break_audit(&path);
        let item = format!("/countdowns/{id}");
        let r = send(
            &router,
            "DELETE",
            &item,
            &[("authorization", WRITE_AUTH)],
            "",
        )
        .await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.code(), "audit_unavailable");
        assert_eq!(fs::read(&path).unwrap(), before);
        let r = send(&router, "GET", &item, &[("authorization", READ_AUTH)], "").await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers["x-audit-status"], "unavailable");
    }

    #[tokio::test]
    async fn a_lost_result_line_is_flagged_on_the_response() {
        // The handler breaks the audit path mid-request, so the attempt line
        // lands but the result line cannot.
        let dir = tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let breaker = {
            let config = config.clone();
            move || async move {
                break_audit(&config);
                StatusCode::NO_CONTENT
            }
        };
        let router = Router::new()
            .route("/countdowns/{id}", axum::routing::delete(breaker))
            .layer(middleware::from_fn_with_state(
                test_gate(&audit_path(&config)),
                guard,
            ));
        let r = send(
            &router,
            "DELETE",
            "/countdowns/x",
            &[("authorization", WRITE_AUTH)],
            "",
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(r.headers["x-audit-status"], "unavailable");
    }
}
