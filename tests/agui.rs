//! `countdown_agent --serve`: the AG-UI SSE endpoint and the page routes,
//! against a fake OpenAI-compatible gateway and the real MCP and API
//! processes (spec step 6, acceptance 1, 2, 5, 6, 8).

mod common;

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{body::Bytes, http::StatusCode, response::IntoResponse, Json, Router};
use common::{token_file, token_table, CliProcess, McpProcess, CALLER_TOKEN};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONFIG: &str = r#"[[countdown]]
title = "周年纪念-7c1e"
datetime = "2030-01-01 00:00:00"
"#;
const TITLE: &str = "周年纪念-7c1e";
const NOT_FOUND_ID: &str = "00000000-0000-4000-8000-000000000000";
const MARK: &str = "思考标记-5f2a";

/// What the fake gateway does for one request.
enum Reply {
    Message(Value),
    /// Answers after a delay (the model is "thinking").
    Late(Duration, Value),
    Status(u16),
}

type Requests = Arc<Mutex<Vec<Value>>>;

/// Fake OpenAI-compatible gateway; `script(n, body)` decides the n-th reply.
async fn fake_gateway(
    script: impl Fn(usize, &Value) -> Reply + Send + Sync + 'static,
) -> (String, Requests) {
    let seen: Requests = Arc::default();
    let script = Arc::new(script);
    let app = Router::new().fallback({
        let seen = seen.clone();
        move |body: Bytes| {
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(body.clone());
                seen.len() - 1
            };
            let reply = script(n, &body);
            async move {
                let message = match reply {
                    Reply::Status(code) => {
                        return StatusCode::from_u16(code).unwrap().into_response()
                    }
                    Reply::Late(delay, m) => {
                        tokio::time::sleep(delay).await;
                        m
                    }
                    Reply::Message(m) => m,
                };
                let finish = if message.get("tool_calls").is_some() {
                    "tool_calls"
                } else {
                    "stop"
                };
                Json(json!({
                    "id": format!("chatcmpl-{n}"), "object": "chat.completion",
                    "created": 0, "model": "fake",
                    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                }))
                .into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [{
        "id": "call_1", "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    }]})
}

fn text(content: &str) -> Value {
    json!({"role": "assistant", "content": content})
}

/// Real API with one countdown and the real MCP in front of it.
struct Stack {
    _dir: tempfile::TempDir,
    _api: CliProcess,
    mcp: McpProcess,
    agent_token: PathBuf,
}

async fn stack() -> Stack {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("c.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let table = dir.path().join("api-tokens.toml");
    std::fs::write(
        &table,
        token_table(&[("mcp-svc", common::SERVICE_TOKEN, "countdowns:read")]),
    )
    .unwrap();
    let audit = dir.path().join("api-audit.jsonl");
    let api = CliProcess::spawn(&[
        "-c".as_ref(),
        config.as_os_str(),
        "--api".as_ref(),
        "127.0.0.1:0".as_ref(),
        "--api-tokens".as_ref(),
        table.as_os_str(),
        "--audit-log".as_ref(),
        audit.as_os_str(),
    ]);
    assert!(
        api.wait_for_stderr("countdown API listening on"),
        "{}",
        api.stderr()
    );
    let port = common::parse_listening_port(&api.stderr());
    let base = format!("http://127.0.0.1:{port}");
    let http = common::http_client();
    let deadline = Instant::now() + common::PROCESS_TIMEOUT;
    loop {
        let probe = http
            .get(format!("{base}/countdowns"))
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        if probe.is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "API did not answer; stderr: {}",
            api.stderr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let svc = token_file(dir.path(), "svc.token", common::SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(&base, Some(&svc), |_| {});
    assert!(mcp.wait_for_stderr("listening on"), "{}", mcp.stderr());
    let agent_token = token_file(dir.path(), "agent.token", CALLER_TOKEN, 0o600);
    Stack {
        _dir: dir,
        _api: api,
        mcp,
        agent_token,
    }
}

/// A running `countdown_agent --serve 127.0.0.1:0`; killed on drop.
struct Server {
    child: Child,
    stderr: Arc<Mutex<String>>,
    addr: String,
}

impl Server {
    /// `web_dir`: `Some` sets COUNTDOWN_AGENT_WEB_DIR; `cwd` is where it runs.
    fn start(gateway: &str, s: &Stack, web_dir: Option<&Path>, cwd: Option<&Path>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_countdown_agent"));
        cmd.env_clear()
            .args(["--serve", "127.0.0.1:0"])
            .env("COUNTDOWN_AGENT_BASE_URL", gateway)
            .env("COUNTDOWN_AGENT_MODEL", "fake-model")
            .env("COUNTDOWN_MCP_URL", s.mcp.url())
            .env("COUNTDOWN_AGENT_MCP_TOKEN_FILE", &s.agent_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(dir) = web_dir {
            cmd.env("COUNTDOWN_AGENT_WEB_DIR", dir);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let mut server = Self {
            child,
            stderr,
            addr: String::new(),
        };
        let line = server
            .wait_line(|l| l["event"] == "listening", common::PROCESS_TIMEOUT)
            .unwrap_or_else(|| panic!("agent did not listen; stderr: {}", server.stderr()));
        server.addr = line["addr"].as_str().unwrap().to_owned();
        server
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// JSON lines the agent wrote to stderr.
    fn lines(&self) -> Vec<Value> {
        self.stderr()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn wait_line(&self, want: impl Fn(&Value) -> bool, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(l) = self.lines().into_iter().find(&want) {
                return Some(l);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn port(&self) -> u16 {
        self.addr.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("countdown_agent stderr:\n{}", self.stderr());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_input(question: &str) -> Value {
    json!({
        "threadId": "thread-1", "runId": "run-1", "state": {}, "tools": [],
        "context": [], "forwardedProps": {},
        "messages": [{"id": "u1", "role": "user", "content": question}],
    })
}

/// Runs one question to the end and returns the raw SSE body.
async fn run(server: &Server, question: &str) -> String {
    let resp = common::http_client()
        .post(server.url("/agui"))
        .header("accept", "text/event-stream")
        .json(&run_input(question))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    resp.text().await.unwrap()
}

/// AG-UI events in an SSE body (heartbeat comments skipped).
fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect()
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// Exactly one terminal event, and it is the last one.
fn assert_one_terminal(events: &[Value]) {
    let t = types(events);
    let terminals = t
        .iter()
        .filter(|t| **t == "RUN_FINISHED" || **t == "RUN_ERROR")
        .count();
    assert_eq!(terminals, 1, "{t:?}");
    assert!(
        matches!(t.last(), Some(&"RUN_FINISHED" | &"RUN_ERROR")),
        "{t:?}"
    );
}

fn tool_audit(s: &Stack) -> Vec<Value> {
    s.mcp
        .audit_lines()
        .into_iter()
        .filter(|l| l["action"].as_str().unwrap_or("").starts_with("tool:"))
        .collect()
}

const SUCCESS_ORDER: [&str; 9] = [
    "RUN_STARTED",
    "TOOL_CALL_START",
    "TOOL_CALL_ARGS",
    "TOOL_CALL_END",
    "TOOL_CALL_RESULT",
    "TEXT_MESSAGE_START",
    "TEXT_MESSAGE_CONTENT",
    "TEXT_MESSAGE_END",
    "RUN_FINISHED",
];

fn answer_with_tool() -> impl Fn(usize, &Value) -> Reply {
    |n, _| match n {
        0 => Reply::Message(tool_call("list_countdowns", json!({}))),
        _ => Reply::Message(text(&format!("最近的倒计时是「{TITLE}」。"))),
    }
}

/// Checks a successful run: order, request_id joined to the MCP audit, no token.
fn assert_success_run(body: &str, s: &Stack) {
    let ev = events(body);
    assert_eq!(types(&ev), SUCCESS_ORDER, "{body}");
    assert_one_terminal(&ev);
    assert_eq!(ev[0]["threadId"], "thread-1");
    assert_eq!(ev[0]["runId"], "run-1");
    assert_eq!(ev[1]["toolCallName"], "list_countdowns");
    let result: Value = serde_json::from_str(ev[4]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    let audit = tool_audit(s);
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(result["request_id"], audit[0]["request_id"]);
    assert!(ev[6]["delta"].as_str().unwrap().contains(TITLE), "{body}");
    assert!(!body.contains(CALLER_TOKEN), "token leaked: {body}");
}

/// Acceptance 1: start → tool call → tool result → text → finish, and the
/// request_id shown is the one in MCP's audit.
#[tokio::test(flavor = "multi_thread")]
async fn run_streams_tool_call_and_answer_in_order() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(answer_with_tool()).await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let body = run(&server, "最近的倒计时是哪个？").await;
    assert_success_run(&body, &s);
}

/// Raw HTTP/1.1 request so Host can be omitted or forged; returns the status.
async fn raw_status(port: u16, request: &str) -> u16 {
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    let mut got = 0;
    while !buf[..got].windows(2).any(|w| w == b"\r\n") {
        let n = tokio::time::timeout(common::HTTP_TIMEOUT, tcp.read(&mut buf[got..]))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed: {:?}", String::from_utf8_lossy(&buf[..got]));
        got += n;
    }
    let line = String::from_utf8_lossy(&buf[..got]).into_owned();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn request(path: &str, host: Option<&str>, origin: Option<&str>) -> String {
    let body = run_input("最近的倒计时是哪个？").to_string();
    let method = if path == "/agui" { "POST" } else { "GET" };
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    if let Some(h) = host {
        req.push_str(&format!("Host: {h}\r\n"));
    }
    if let Some(o) = origin {
        req.push_str(&format!("Origin: {o}\r\n"));
    }
    if method == "POST" {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("Connection: close\r\n\r\n");
    if method == "POST" {
        req.push_str(&body);
    }
    req
}

/// Acceptance 2: forged or missing Host and foreign Origin are 403 on the
/// page routes and on the AG-UI endpoint; local Host and Origin pass.
#[tokio::test(flavor = "multi_thread")]
async fn foreign_host_or_origin_is_403_on_every_route() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|_, _| Reply::Message(text("好的"))).await;
    let page = tempfile::tempdir().unwrap();
    std::fs::write(page.path().join("index.html"), "<p>页面</p>").unwrap();
    std::fs::write(page.path().join("app.js"), "// app").unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let port = server.port();
    let other = if port == 65535 { port - 1 } else { port + 1 };
    let (ip_host, name_host) = (format!("127.0.0.1:{port}"), format!("localhost:{port}"));
    let bad_hosts = [
        Some(format!("evil.example:{port}")),
        Some(format!("127.0.0.1:{other}")),
        None,
    ];
    let bad_origins = [
        "http://evil.example".to_owned(),
        format!("http://evil.example:{port}"),
        format!("http://127.0.0.1:{other}"),
        "null".to_owned(),
    ];
    for path in ["/", "/app.js", "/agui"] {
        for host in &bad_hosts {
            let status = raw_status(port, &request(path, host.as_deref(), None)).await;
            assert_eq!(status, 403, "{path} host {host:?}");
        }
        for origin in &bad_origins {
            let status = raw_status(port, &request(path, Some(&ip_host), Some(origin))).await;
            assert_eq!(status, 403, "{path} origin {origin}");
        }
        for host in [&ip_host, &name_host] {
            let status = raw_status(port, &request(path, Some(host), None)).await;
            assert_eq!(status, 200, "{path} host {host}");
            let origin = format!("http://{host}");
            let status = raw_status(port, &request(path, Some(host), Some(&origin))).await;
            assert_eq!(status, 200, "{path} host {host} origin {origin}");
        }
    }
}

fn assert_no_thinking(body: &str) {
    assert!(!body.contains(MARK), "thinking leaked: {body}");
    assert!(!body.contains("<think"), "thinking tag leaked: {body}");
    let ev = events(body);
    let contents = types(&ev)
        .into_iter()
        .filter(|t| *t == "TEXT_MESSAGE_CONTENT")
        .count();
    assert_eq!(contents, 1, "{body}");
    assert!(body.contains(TITLE), "{body}");
    assert_one_terminal(&ev);
}

/// Acceptance 5: `<think>` text never reaches any event; one CONTENT only.
#[tokio::test(flavor = "multi_thread")]
async fn think_block_is_never_sent() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|n, _| match n {
        0 => Reply::Message(tool_call("list_countdowns", json!({}))),
        _ => Reply::Message(text(&format!(
            "<think>{MARK}</think>最近的倒计时是「{TITLE}」。"
        ))),
    })
    .await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    assert_no_thinking(&run(&server, "最近的倒计时是哪个？").await);
}

/// Acceptance 5, second case: a separate reasoning field is never sent.
#[tokio::test(flavor = "multi_thread")]
async fn reasoning_field_is_never_sent() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|n, _| match n {
        0 => Reply::Message(tool_call("list_countdowns", json!({}))),
        _ => Reply::Message(json!({
            "role": "assistant", "reasoning_content": MARK, "reasoning": MARK,
            "content": format!("最近的倒计时是「{TITLE}」。"),
        })),
    })
    .await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    assert_no_thinking(&run(&server, "最近的倒计时是哪个？").await);
}

/// Acceptance 6a: a failing tool ends normally with 「查不到」 and RUN_FINISHED.
#[tokio::test(flavor = "multi_thread")]
async fn tool_failure_finishes_with_not_found() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|n, _| match n {
        0 => Reply::Message(tool_call("get_countdown", json!({"id": NOT_FOUND_ID}))),
        _ => Reply::Message(text("不该用到这句")),
    })
    .await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let body = run(&server, "那个倒计时还有多久？").await;
    let ev = events(&body);
    assert_eq!(types(&ev), SUCCESS_ORDER, "{body}");
    assert_one_terminal(&ev);
    assert!(!types(&ev).contains(&"RUN_ERROR"));
    let result: Value = serde_json::from_str(ev[4]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["reason"], "没有这个倒计时");
    let reply = ev[6]["delta"].as_str().unwrap();
    assert!(
        reply.contains("查不到") && reply.contains("没有这个倒计时"),
        "{reply}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the model never sees the failure"
    );
}

/// Same 「查不到」 path when the model answers without calling a tool.
#[tokio::test(flavor = "multi_thread")]
async fn answer_without_tool_finishes_with_not_found() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|_, _| Reply::Message(text("瞎编的答案"))).await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let body = run(&server, "最近的倒计时是哪个？").await;
    let ev = events(&body);
    assert_eq!(
        types(&ev),
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED"
        ],
        "{body}"
    );
    assert!(ev[2]["delta"].as_str().unwrap().contains("查不到"));
    assert!(!body.contains("瞎编的答案"));
}

/// Acceptance 6b: a model gateway error is RUN_ERROR only, nothing after it.
#[tokio::test(flavor = "multi_thread")]
async fn model_error_is_run_error_and_nothing_after() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|_, _| Reply::Status(500)).await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let body = run(&server, "最近的倒计时是哪个？").await;
    let ev = events(&body);
    assert_eq!(types(&ev), ["RUN_STARTED", "RUN_ERROR"], "{body}");
    assert_one_terminal(&ev);
    assert!(tool_audit(&s).is_empty());
}

/// Spec 3.7: a heartbeat comment every second while the model is slow.
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_every_second_while_waiting() {
    let s = stack().await;
    let (gateway, _) =
        fake_gateway(|_, _| Reply::Late(Duration::from_millis(3500), text("瞎编的答案"))).await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let body = run(&server, "最近的倒计时是哪个？").await;
    let pings = body.lines().filter(|l| l.starts_with(':')).count();
    assert!(pings >= 2, "expected heartbeats in {body:?}");
    assert_one_terminal(&events(&body));
}

/// Acceptance 6c: the client hangs up after RUN_STARTED while the model is
/// slow; the run is cancelled within 2 s and nothing more reaches the
/// gateway or MCP.
#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_cancels_the_run() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|_, _| {
        Reply::Late(
            Duration::from_secs(3),
            tool_call("list_countdowns", json!({})),
        )
    })
    .await;
    let page = tempfile::tempdir().unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let mut resp = common::http_client()
        .post(server.url("/agui"))
        .json(&run_input("最近的倒计时是哪个？"))
        .send()
        .await
        .unwrap();
    let mut got = String::new();
    while !got.contains("RUN_STARTED") {
        let chunk = resp.chunk().await.unwrap().expect("stream ended early");
        got.push_str(&String::from_utf8_lossy(&chunk));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while seen.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "gateway never asked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(resp);
    let hung_up = Instant::now();

    let ended = tokio::task::block_in_place(|| {
        server.wait_line(|l| l["event"] == "run_ended", Duration::from_secs(2))
    })
    .unwrap_or_else(|| panic!("run still going 2 s after hang-up: {}", server.stderr()));
    assert!(hung_up.elapsed() <= Duration::from_secs(2));
    assert_eq!(ended["outcome"], "cancelled", "{ended}");
    assert_eq!(ended["running"], 0, "{ended}");

    // Past the moment the gateway answers: no new request, no tool call.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(tool_audit(&s).is_empty(), "{:?}", tool_audit(&s));
    let ends = server
        .lines()
        .into_iter()
        .filter(|l| l["event"] == "run_ended")
        .count();
    assert_eq!(ends, 1);
}

/// Page routes serve the built files from COUNTDOWN_AGENT_WEB_DIR.
#[tokio::test(flavor = "multi_thread")]
async fn page_routes_serve_the_built_files() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(|_, _| Reply::Message(text("好的"))).await;
    let page = tempfile::tempdir().unwrap();
    std::fs::write(page.path().join("index.html"), "<p>页面</p>").unwrap();
    std::fs::write(page.path().join("app.js"), "// app").unwrap();
    let server = Server::start(&gateway, &s, Some(page.path()), None);
    let http = common::http_client();
    let index = http.get(server.url("/")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    assert!(index.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(index.text().await.unwrap(), "<p>页面</p>");
    let js = http.get(server.url("/app.js")).send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));
    assert!(server
        .lines()
        .iter()
        .all(|l| l["event"] != "web_dir_missing"));
}

/// Acceptance 8: missing page directory → one JSON warning, page 503 with
/// the path, and the AG-UI endpoint of the same instance still works.
#[tokio::test(flavor = "multi_thread")]
async fn missing_page_dir_is_503_and_agui_still_works() {
    let s = stack().await;
    let (gateway, _) = fake_gateway(answer_with_tool()).await;
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-dist");
    let server = Server::start(&gateway, &s, Some(&missing), None);
    let warnings: Vec<Value> = server
        .lines()
        .into_iter()
        .filter(|l| l["event"] == "web_dir_missing")
        .collect();
    assert_eq!(warnings.len(), 1, "{}", server.stderr());
    assert_eq!(warnings[0]["level"], "warn");
    assert_eq!(warnings[0]["web_dir"], missing.display().to_string());
    assert!(warnings[0]["reason"]
        .as_str()
        .unwrap()
        .contains("COUNTDOWN_AGENT_WEB_DIR"));
    let http = common::http_client();
    for path in ["/", "/app.js"] {
        let resp = http.get(server.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), 503, "{path}");
        let body = resp.text().await.unwrap();
        assert!(body.contains(&missing.display().to_string()), "{body}");
    }
    let body = run(&server, "最近的倒计时是哪个？").await;
    assert_success_run(&body, &s);
}

/// Acceptance 8, default path: without the variable, the page directory is
/// `web/dist` next to the executable, not under the current directory.
#[tokio::test(flavor = "multi_thread")]
async fn default_page_dir_is_next_to_the_executable() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_countdown_agent"));
    let expected = exe.parent().unwrap().join("web").join("dist");
    assert!(
        !expected.exists(),
        "test needs {} to be absent",
        expected.display()
    );
    let s = stack().await;
    let (gateway, _) = fake_gateway(|_, _| Reply::Message(text("好的"))).await;
    // A cwd that has web/dist: a cwd-relative default would find it.
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(cwd.path().join("web").join("dist")).unwrap();
    let server = Server::start(&gateway, &s, None, Some(cwd.path()));
    let warning = server
        .lines()
        .into_iter()
        .find(|l| l["event"] == "web_dir_missing")
        .unwrap_or_else(|| panic!("no warning: {}", server.stderr()));
    assert_eq!(warning["web_dir"], expected.display().to_string());
}

/// Spec 3.4: the page source loads nothing from another host.
#[test]
fn page_source_has_no_external_addresses() {
    let html =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html")).unwrap();
    for needle in ["http://", "https://", "src=\"//", "href=\"//"] {
        assert!(!html.contains(needle), "{needle} in web/index.html");
    }
    assert!(html.contains("src=\"/app.js\""));
}
