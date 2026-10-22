//! AG-UI server for the agent (step 6): `POST /agui` streams one run as
//! Server-Sent Events, and two fixed routes serve the optional web page.
//!
//! The AG-UI endpoint does not depend on the page: it works for any AG-UI
//! client, and keeps working when the page files are missing.

use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitCode,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestUserMessage,
};
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::{wrappers::ReceiverStream, Stream};

use super::{Shared, Step, SYSTEM_PROMPT};

pub const DEFAULT_ADDR: &str = "127.0.0.1:8780";
pub const WEB_DIR_VAR: &str = "COUNTDOWN_AGENT_WEB_DIR";
/// SSE comment line sent every second, so a closed connection is noticed
/// while the model is still thinking (spec 3.7).
const HEARTBEAT: Duration = Duration::from_secs(1);
/// Upper bound on events queued for one run; a run sends far fewer.
const EVENT_QUEUE: usize = 256;

/// The ten AG-UI events this server sends; field names follow
/// @ag-ui/core 1.0.3 (checked against `tests/fixtures/agui-events.json`).
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum AgUiEvent {
    RunStarted {
        thread_id: String,
        run_id: String,
    },
    RunFinished {
        thread_id: String,
        run_id: String,
    },
    RunError {
        message: String,
        code: String,
    },
    TextMessageStart {
        message_id: String,
        role: &'static str,
    },
    TextMessageContent {
        message_id: String,
        delta: String,
    },
    TextMessageEnd {
        message_id: String,
    },
    ToolCallStart {
        tool_call_id: String,
        tool_call_name: String,
    },
    ToolCallArgs {
        tool_call_id: String,
        delta: String,
    },
    ToolCallEnd {
        tool_call_id: String,
    },
    ToolCallResult {
        message_id: String,
        tool_call_id: String,
        content: String,
        role: &'static str,
    },
}

/// The part of AG-UI's `RunAgentInput` this server reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunInput {
    thread_id: Option<String>,
    run_id: Option<String>,
    #[serde(default)]
    messages: Vec<InputMessage>,
}

#[derive(Deserialize)]
struct InputMessage {
    role: String,
    #[serde(default)]
    content: Value,
}

/// `127.0.0.1:PORT` only: the server never listens anywhere else.
pub fn listen_addr(value: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = value
        .parse()
        .map_err(|e| format!("--serve {value}: not an address: {e}"))?;
    if addr.ip() != Ipv4Addr::LOCALHOST {
        return Err(format!("--serve {value}: only 127.0.0.1 is allowed"));
    }
    Ok(addr)
}

/// `COUNTDOWN_AGENT_WEB_DIR`, else `web/dist` next to the executable; never
/// relative to the current directory.
pub fn web_dir(var: Option<String>, exe: Option<&Path>) -> PathBuf {
    match var {
        Some(dir) => PathBuf::from(dir),
        None => exe
            .and_then(Path::parent)
            .unwrap_or(Path::new("/"))
            .join("web")
            .join("dist"),
    }
}

#[derive(Serialize)]
struct Line<'a> {
    level: &'static str,
    svc: &'static str,
    event: &'static str,
    #[serde(flatten)]
    fields: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

fn log(level: &'static str, event: &'static str, fields: Value, reason: Option<&str>) {
    let line = Line {
        level,
        svc: "agent",
        event,
        fields,
        reason,
    };
    eprintln!(
        "{}",
        serde_json::to_string(&line).expect("log line is JSON")
    );
}

const WEB_DIR_HINT: &str =
    "先构建前端（cd web && npm ci && npm run build）或设置 COUNTDOWN_AGENT_WEB_DIR";

struct AppState {
    shared: Arc<Shared>,
    web_dir: PathBuf,
    /// Port actually listened on; Host and Origin must name it.
    port: u16,
    running: Arc<AtomicUsize>,
}

pub async fn serve(shared: Arc<Shared>, addr: SocketAddr, web_dir: PathBuf) -> ExitCode {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            log(
                "alert",
                "listen_failed",
                json!({ "addr": addr.to_string() }),
                Some(&e.to_string()),
            );
            return ExitCode::from(2);
        }
    };
    let bound = listener.local_addr().expect("bound address");
    if !web_dir.is_dir() {
        log(
            "warn",
            "web_dir_missing",
            json!({ "web_dir": web_dir.display().to_string() }),
            Some(WEB_DIR_HINT),
        );
    }
    let state = Arc::new(AppState {
        shared,
        web_dir,
        port: bound.port(),
        running: Arc::default(),
    });
    log(
        "info",
        "listening",
        json!({ "addr": bound.to_string() }),
        None,
    );
    match axum::serve(listener, router(state)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log("alert", "serve_failed", json!({}), Some(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

/// Page routes and the AG-UI route are separate; both sit behind the same
/// Host and Origin checks.
fn router(state: Arc<AppState>) -> Router {
    let page = Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js));
    let agui = Router::new().route("/agui", post(run));
    page.merge(agui)
        .layer(middleware::from_fn_with_state(state.clone(), local_only))
        .with_state(state)
}

/// Host first (DNS rebinding), then Origin; anything else is 403.
async fn local_only(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if !allowed(req.headers(), state.port) {
        return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
    }
    next.run(req).await
}

fn allowed(headers: &HeaderMap, port: u16) -> bool {
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_ok = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| local.iter().any(|l| l == h));
    if !host_ok {
        return false;
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .is_ok_and(|o| local.iter().any(|l| o == format!("http://{l}"))),
    }
}

async fn index_html(State(state): State<Arc<AppState>>) -> Response {
    page_file(&state.web_dir, "index.html", "text/html; charset=utf-8").await
}

async fn app_js(State(state): State<Arc<AppState>>) -> Response {
    page_file(&state.web_dir, "app.js", "text/javascript; charset=utf-8").await
}

/// Read from disk on each request; a missing build is 503 naming the path.
async fn page_file(dir: &Path, name: &str, content_type: &'static str) -> Response {
    let path = dir.join(name);
    match tokio::fs::read(&path).await {
        Ok(body) => ([(header::CONTENT_TYPE, content_type)], body).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!(
                "页面文件缺失：{}。{WEB_DIR_HINT}。AG-UI 接口 POST /agui 不受影响。\n",
                path.display()
            ),
        )
            .into_response(),
    }
}

/// Aborts the run task when the SSE stream is dropped (client gone).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The run's events; owns the run task, so dropping the stream cancels it.
struct RunStream {
    events: ReceiverStream<Result<Event, Infallible>>,
    _task: AbortOnDrop,
}

impl Stream for RunStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.events).poll_next(cx)
    }
}

/// Counts a run as running until dropped (finished or aborted), then logs
/// how it ended and how many runs are left.
struct RunGuard {
    running: Arc<AtomicUsize>,
    run_id: String,
    outcome: &'static str,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let left = self.running.fetch_sub(1, Ordering::SeqCst) - 1;
        log(
            "info",
            "run_ended",
            json!({ "run_id": self.run_id, "outcome": self.outcome, "running": left }),
            None,
        );
    }
}

fn history(messages: &[InputMessage]) -> Option<Vec<ChatCompletionRequestMessage>> {
    let mut out: Vec<ChatCompletionRequestMessage> =
        vec![ChatCompletionRequestSystemMessage::from(SYSTEM_PROMPT).into()];
    let mut last_is_user = false;
    for m in messages {
        let Some(text) = m.content.as_str() else {
            continue;
        };
        match m.role.as_str() {
            "user" => {
                out.push(ChatCompletionRequestUserMessage::from(text).into());
                last_is_user = true;
            }
            "assistant" => {
                out.push(ChatCompletionRequestAssistantMessage::from(text).into());
                last_is_user = false;
            }
            _ => {}
        }
    }
    last_is_user.then_some(out)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn run(State(state): State<Arc<AppState>>, Json(input): Json<RunInput>) -> Response {
    let Some(messages) = history(&input.messages) else {
        return (
            StatusCode::BAD_REQUEST,
            "the last message must be a user message with text content\n",
        )
            .into_response();
    };
    let thread_id = input.thread_id.unwrap_or_else(new_id);
    let run_id = input.run_id.unwrap_or_else(new_id);
    let (tx, rx) = mpsc::channel(EVENT_QUEUE);
    state.running.fetch_add(1, Ordering::SeqCst);
    let guard = RunGuard {
        running: state.running.clone(),
        run_id: run_id.clone(),
        outcome: "cancelled",
    };
    let shared = state.shared.clone();
    let task = tokio::spawn(run_task(shared, messages, thread_id, run_id, tx, guard));
    let stream = RunStream {
        events: ReceiverStream::new(rx),
        _task: AbortOnDrop(task),
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT).text("ping"))
        .into_response()
}

fn sse(event: &AgUiEvent) -> Result<Event, Infallible> {
    Ok(Event::default().data(serde_json::to_string(event).expect("event is JSON")))
}

/// One run: RUN_STARTED, tool calls, one text message, then exactly one
/// terminal event (RUN_FINISHED, or RUN_ERROR when the model gateway fails).
async fn run_task(
    shared: Arc<Shared>,
    messages: Vec<ChatCompletionRequestMessage>,
    thread_id: String,
    run_id: String,
    tx: mpsc::Sender<Result<Event, Infallible>>,
    mut guard: RunGuard,
) {
    let send = |e: AgUiEvent| {
        // Full queue or closed stream: the client is gone or too slow; the
        // stream drop aborts this task.
        let _ = tx.try_send(sse(&e));
    };
    send(AgUiEvent::RunStarted {
        thread_id: thread_id.clone(),
        run_id: run_id.clone(),
    });
    let mut on_step = |step: Step<'_>| match step {
        Step::ToolStart { id, name, args } => {
            send(AgUiEvent::ToolCallStart {
                tool_call_id: id.to_owned(),
                tool_call_name: name.to_owned(),
            });
            if !args.is_empty() {
                send(AgUiEvent::ToolCallArgs {
                    tool_call_id: id.to_owned(),
                    delta: args.to_owned(),
                });
            }
            send(AgUiEvent::ToolCallEnd {
                tool_call_id: id.to_owned(),
            });
        }
        Step::ToolResult {
            id,
            outcome,
            request_id,
        } => {
            let content = match outcome {
                Ok(text) => json!({ "ok": true, "request_id": request_id, "result": text }),
                Err(reason) => json!({ "ok": false, "request_id": request_id, "reason": reason }),
            };
            send(AgUiEvent::ToolCallResult {
                message_id: new_id(),
                tool_call_id: id.to_owned(),
                content: content.to_string(),
                role: "tool",
            });
        }
    };
    match shared.answer(messages, &mut on_step).await {
        Ok(reply) => {
            // Already stripped of thinking: sent whole, in one CONTENT.
            let message_id = new_id();
            send(AgUiEvent::TextMessageStart {
                message_id: message_id.clone(),
                role: "assistant",
            });
            send(AgUiEvent::TextMessageContent {
                message_id: message_id.clone(),
                delta: reply,
            });
            send(AgUiEvent::TextMessageEnd { message_id });
            send(AgUiEvent::RunFinished { thread_id, run_id });
            guard.outcome = "finished";
        }
        Err(e) => {
            send(AgUiEvent::RunError {
                message: format!("模型网关出错：{e}"),
                code: "model_error".into(),
            });
            guard.outcome = "error";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = host {
            h.insert(header::HOST, v.parse().unwrap());
        }
        if let Some(v) = origin {
            h.insert(header::ORIGIN, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn host_and_origin_must_name_this_port() {
        for (host, origin, ok) in [
            (Some("127.0.0.1:8780"), None, true),
            (Some("localhost:8780"), None, true),
            (Some("127.0.0.1:8780"), Some("http://127.0.0.1:8780"), true),
            (Some("127.0.0.1:8780"), Some("http://localhost:8780"), true),
            (None, None, false),
            (Some("evil.example:8780"), None, false),
            (Some("127.0.0.1:9999"), None, false),
            (Some("127.0.0.1"), None, false),
            (
                Some("evil.example:8780"),
                Some("http://127.0.0.1:8780"),
                false,
            ),
            (Some("127.0.0.1:8780"), Some("http://evil.example"), false),
            (Some("127.0.0.1:8780"), Some("http://127.0.0.1:9999"), false),
            (
                Some("127.0.0.1:8780"),
                Some("https://127.0.0.1:8780"),
                false,
            ),
            (Some("127.0.0.1:8780"), Some("null"), false),
        ] {
            assert_eq!(
                allowed(&headers(host, origin), 8780),
                ok,
                "{host:?} {origin:?}"
            );
        }
    }

    #[test]
    fn web_dir_is_next_to_the_executable_unless_set() {
        assert_eq!(
            web_dir(None, Some(Path::new("/opt/app/bin/countdown_agent"))),
            PathBuf::from("/opt/app/bin/web/dist")
        );
        assert_eq!(
            web_dir(Some("/srv/page".into()), Some(Path::new("/opt/x"))),
            PathBuf::from("/srv/page")
        );
    }

    #[test]
    fn only_127_0_0_1_is_listened_on() {
        assert!(listen_addr("127.0.0.1:0").is_ok());
        for bad in [
            "0.0.0.0:8780",
            "[::1]:8780",
            "10.0.0.1:8780",
            "localhost:8780",
        ] {
            assert!(listen_addr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn history_needs_a_last_user_message() {
        let m = |role: &str, content: &str| InputMessage {
            role: role.into(),
            content: json!(content),
        };
        assert_eq!(history(&[m("user", "q")]).map(|h| h.len()), Some(2));
        assert_eq!(
            history(&[m("user", "q"), m("assistant", "a"), m("user", "q2")]).map(|h| h.len()),
            Some(4)
        );
        assert!(history(&[]).is_none());
        assert!(history(&[m("user", "q"), m("assistant", "a")]).is_none());
    }

    /// Field-name check against the fixed samples generated from
    /// @ag-ui/core 1.0.3 (`web/scripts/gen-fixtures.mjs`); needs no node.
    #[test]
    fn events_match_the_ag_ui_core_samples() {
        let raw = include_str!("../../../tests/fixtures/agui-events.json");
        let samples: Value = serde_json::from_str(raw).unwrap();
        let s = |k: &str| k.to_owned();
        let ours = [
            AgUiEvent::RunStarted {
                thread_id: s("t1"),
                run_id: s("r1"),
            },
            AgUiEvent::RunFinished {
                thread_id: s("t1"),
                run_id: s("r1"),
            },
            AgUiEvent::RunError {
                message: s("模型网关出错"),
                code: s("model_error"),
            },
            AgUiEvent::TextMessageStart {
                message_id: s("m1"),
                role: "assistant",
            },
            AgUiEvent::TextMessageContent {
                message_id: s("m1"),
                delta: s("答案"),
            },
            AgUiEvent::TextMessageEnd {
                message_id: s("m1"),
            },
            AgUiEvent::ToolCallStart {
                tool_call_id: s("c1"),
                tool_call_name: s("list_countdowns"),
            },
            AgUiEvent::ToolCallArgs {
                tool_call_id: s("c1"),
                delta: s("{}"),
            },
            AgUiEvent::ToolCallEnd {
                tool_call_id: s("c1"),
            },
            AgUiEvent::ToolCallResult {
                message_id: s("m2"),
                tool_call_id: s("c1"),
                content: s("{\"ok\":true}"),
                role: "tool",
            },
        ];
        let samples = samples["events"].as_array().unwrap();
        assert_eq!(samples.len(), ours.len());
        for (ours, sample) in ours.iter().zip(samples) {
            assert_eq!(&serde_json::to_value(ours).unwrap(), sample);
        }
    }
}
