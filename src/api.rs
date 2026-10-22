use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{Local, NaiveDateTime};
// no tokio read guard needed here
use countdown_cli::CountdownUpdateError;

use countdown_cli::{calculate_target_time, Countdown, CountdownStore, RepeatMode};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiCountdown {
    pub id: String,
    pub title: String,
    pub datetime: String,
    pub enabled: bool,
    pub repeat: Option<String>, // unified to "weekday" if present
    pub target_time: Option<String>,
    pub remaining_seconds: Option<i64>,
}

impl ApiCountdown {
    fn from_domain(countdown: &Countdown, now: NaiveDateTime) -> Option<Self> {
        let id = countdown.id.as_ref()?.clone();
        let target = calculate_target_time(&countdown.datetime, countdown.repeat.as_ref(), now);
        let remaining_seconds = target.map(|t| (t - now).num_seconds());
        Some(Self {
            id,
            title: countdown.title.clone(),
            datetime: countdown.datetime.clone(),
            enabled: countdown.enabled,
            repeat: countdown
                .repeat
                .as_ref()
                .map(|r| match r {
                    RepeatMode::Daily => "daily".to_string(),
                    RepeatMode::Weekday => "weekday".to_string(),
                }),
            target_time: target.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
            remaining_seconds,
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

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<CountdownStore>,
}

pub fn build_router(store: Arc<CountdownStore>) -> Router {
    Router::new()
        .route("/countdowns", get(list_countdowns).post(create_countdown))
        .route(
            "/countdowns/{id}",
            get(get_countdown).put(update_countdown).delete(delete_countdown),
        )
        .with_state(ApiState { store })
}

// no ListQuery; we parse query via HashMap

async fn list_countdowns(
    State(state): State<ApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let enabled_filter = query
        .get("enabled")
        .and_then(|v| v.parse::<bool>().ok());
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

async fn get_countdown(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
                    .map(|r| if matches!(r, RepeatMode::Daily) { "daily" } else { "weekday" })
                    .map(|s| s.to_string()),
                target_time: None,
                remaining_seconds: None,
            });
            (StatusCode::CREATED, Json(api)).into_response()
        }
        Err(e) => api_error("invalid_body", e.to_string(), StatusCode::BAD_REQUEST),
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
        Err(CountdownUpdateError::Other(e)) => {
            api_error("invalid_body", e, StatusCode::BAD_REQUEST)
        }
    }
}

async fn delete_countdown(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.api_delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error("not_found", "Countdown not found", StatusCode::NOT_FOUND),
        Err(e) => api_error("invalid_body", e.to_string(), StatusCode::BAD_REQUEST),
    }
}

// no-op: keep module private items

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use std::{fs, io::Write};
    use tempfile::tempdir;
    use tower::ServiceExt;
    use http_body_util::BodyExt as _;

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

    async fn setup_router_with_file(content: &str) -> (Router, Arc<CountdownStore>, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        let store = CountdownStore::new(path.clone(), true).await.unwrap();
        let router = build_router(store.clone());
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
            .oneshot(Request::post("/countdowns")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap())
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
            .oneshot(Request::get(format!("/countdowns/{}", created.id)).body(axum::body::Body::empty()).unwrap())
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
            .oneshot(Request::put(format!("/countdowns/{}", created.id))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let updated: ApiCountdown = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(updated.title, "C2");
        // DELETE
        let res = router
            .clone()
            .oneshot(Request::delete(format!("/countdowns/{}", updated.id)).body(axum::body::Body::empty()).unwrap())
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
            .oneshot(Request::get("/countdowns/does-not-exist").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        #[derive(serde::Deserialize)] struct TestErr { error: TestErrInner }
        #[derive(serde::Deserialize)] struct TestErrInner { code: String, message: String }
        let body: TestErr = serde_json::from_slice(&bytes).unwrap();
        let _ = &body.error.message;
        assert_eq!(body.error.code, "not_found");
        // POST invalid datetime
        let bad = serde_json::json!({"title":"X","datetime":"BAD","enabled":true});
        let res = router
            .clone()
            .oneshot(Request::post("/countdowns").header("content-type","application/json").body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap())).unwrap())
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
            .oneshot(Request::post("/countdowns").header("content-type","application/json").body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap())).unwrap())
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
            .oneshot(Request::post("/countdowns").header("content-type","application/json").body(axum::body::Body::from(serde_json::to_vec(&bad).unwrap())).unwrap())
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
            .oneshot(Request::put("/countdowns/nope").header("content-type","application/json").body(axum::body::Body::from(serde_json::to_vec(&good).unwrap())).unwrap())
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
            .oneshot(Request::delete("/countdowns/nope").body(axum::body::Body::empty()).unwrap())
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
            .oneshot(Request::get("/countdowns").body(axum::body::Body::empty()).unwrap())
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
        // simulate hot reload (ensure mtime difference)
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        store.reload().await.unwrap();
        // list should contain C with id
        let res = router
            .oneshot(Request::get("/countdowns").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let list: Vec<ApiCountdown> = serde_json::from_slice(&bytes).unwrap();
        let item = list.iter().find(|it| it.title == "C").expect("new item exists");
        assert!(!item.id.is_empty());
    }
}

