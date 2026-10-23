//! `countdown_agent` against a fake OpenAI-compatible gateway and the real
//! MCP server and API processes (spec step 5, acceptance 1–5).

mod common;

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    Json, Router,
};
use common::{token_file, token_table, CliProcess, McpProcess, CALLER_ID, CALLER_TOKEN};
use serde_json::{json, Value};

const TITLE: &str = "周年纪念-7c1e";
const CONFIG: &str = r#"[[countdown]]
title = "周年纪念-7c1e"
datetime = "2030-01-01 00:00:00"
"#;
const NOT_FOUND_ID: &str = "00000000-0000-4000-8000-000000000000";

/// One request the fake gateway received.
#[derive(Clone)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
}

type Requests = Arc<Mutex<Vec<Seen>>>;

/// Fake OpenAI-compatible gateway: `script(n, body)` gives the assistant
/// message for the n-th request (0-based). Returns the base URL (`…/v1`).
async fn fake_gateway(
    script: impl Fn(usize, &Value) -> Value + Send + Sync + 'static,
) -> (String, Requests) {
    let seen: Requests = Arc::default();
    let script = Arc::new(script);
    let app = Router::new().fallback({
        let seen = seen.clone();
        move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(Seen {
                    path: uri.path().to_owned(),
                    headers,
                    body: body.clone(),
                });
                seen.len() - 1
            };
            let message = script(n, &body);
            let finish = if message.get("tool_calls").is_some() {
                "tool_calls"
            } else {
                "stop"
            };
            async move {
                Json(json!({
                    "id": format!("chatcmpl-{n}"), "object": "chat.completion",
                    "created": 0, "model": "fake",
                    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                }))
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

/// Title of the first countdown in the last tool message of a request, so
/// the fake model can only name what the real MCP actually returned.
fn title_from_tool_result(body: &Value) -> String {
    let tool = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool message in {body}"));
    let data: Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
    data[0]["title"].as_str().unwrap().to_owned()
}

/// Real API with one countdown, real MCP in front of it, and the agent's
/// MCP entry token file (the `agent-dev` row, countdowns:read only).
struct Stack {
    _dir: tempfile::TempDir,
    api_audit: PathBuf,
    _api: CliProcess,
    mcp: McpProcess,
    agent_token: PathBuf,
}

async fn start_api(dir: &Path, audit: &Path) -> (CliProcess, String) {
    let config = dir.join("c.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let table = dir.join("api-tokens.toml");
    std::fs::write(
        &table,
        token_table(&[("mcp-svc", common::SERVICE_TOKEN, "countdowns:read")]),
    )
    .unwrap();
    let addr = format!("127.0.0.1:{}", common::free_port());
    let api = CliProcess::spawn(&[
        "-c".as_ref(),
        config.as_os_str(),
        "--api".as_ref(),
        addr.as_ref(),
        "--api-tokens".as_ref(),
        table.as_os_str(),
        "--audit-log".as_ref(),
        audit.as_os_str(),
    ]);
    let base = format!("http://{addr}");
    let http = common::http_client();
    let deadline = Instant::now() + common::PROCESS_TIMEOUT;
    while Instant::now() < deadline {
        let probe = http
            .get(format!("{base}/countdowns"))
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        if probe.is_ok() {
            return (api, base);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("API at {base} did not answer; stderr: {}", api.stderr());
}

fn start_mcp(dir: &Path, api_url: &str) -> McpProcess {
    let svc = token_file(dir, "svc.token", common::SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(api_url, Some(&svc), |_| {});
    assert!(mcp.wait_for_stderr("listening on"), "{}", mcp.stderr());
    mcp
}

async fn stack() -> Stack {
    let dir = tempfile::tempdir().unwrap();
    let api_audit = dir.path().join("api-audit.jsonl");
    let (api, base) = start_api(dir.path(), &api_audit).await;
    let mcp = start_mcp(dir.path(), &base);
    let agent_token = token_file(dir.path(), "agent.token", CALLER_TOKEN, 0o600);
    Stack {
        _dir: dir,
        api_audit,
        _api: api,
        mcp,
        agent_token,
    }
}

/// The agent with a clean environment plus `env`; stdin gets `input`.
/// Runs off the async runtime so the fake gateway keeps serving.
async fn run_agent(env: Vec<(&'static str, String)>, args: &[&str], input: &str) -> Output {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let input = input.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_countdown_agent"))
            .env_clear()
            .envs(env)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let deadline = Instant::now() + common::PROCESS_TIMEOUT;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let out = child.wait_with_output().unwrap();
                panic!(
                    "countdown_agent did not exit within {:?}; stderr: {}",
                    common::PROCESS_TIMEOUT,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn agent_env(gateway: &str, mcp_url: &str, token: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("COUNTDOWN_AGENT_BASE_URL", gateway.to_owned()),
        ("COUNTDOWN_AGENT_MODEL", "fake-model".to_owned()),
        ("COUNTDOWN_MCP_URL", mcp_url.to_owned()),
        (
            "COUNTDOWN_AGENT_MCP_TOKEN_FILE",
            token.display().to_string(),
        ),
    ]
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Parsed `调用记录 {...}` lines from the agent's stderr.
fn call_records(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("调用记录 "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn tool_audit_lines(lines: &[Value]) -> Vec<Value> {
    lines
        .iter()
        .filter(|l| l["action"].as_str().unwrap_or("").starts_with("tool:"))
        .cloned()
        .collect()
}

/// The MCP entry token must not show up in anything the stack writes.
fn assert_no_entry_token(texts: &[&str]) {
    for text in texts {
        assert!(!text.contains(CALLER_TOKEN), "entry token leaked: {text}");
    }
}

/// Acceptance 1: tool_call → real MCP → real API; the answer names the
/// countdown and the call record carries the request_id both audits have.
#[tokio::test(flavor = "multi_thread")]
async fn tool_call_reaches_the_real_mcp_and_the_answer_names_the_countdown() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|n, body| match n {
        0 => tool_call("list_countdowns", json!({})),
        _ => text(&format!(
            "最近的倒计时是「{}」。",
            title_from_tool_result(body)
        )),
    })
    .await;
    let out = run_agent(
        agent_env(&gateway, &s.mcp.url(), &s.agent_token),
        &["最近的倒计时是哪个？"],
        "",
    )
    .await;
    let (stdout, stderr) = (stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stdout.contains(TITLE), "{stdout}");

    let records = call_records(&stderr);
    assert_eq!(records.len(), 1, "{stderr}");
    let record = &records[0];
    assert_eq!(record["tool"], "list_countdowns");
    assert_eq!(record["args"], json!({}));
    assert_eq!(record["ok"], true);
    assert!(record["duration_ms"].is_u64());

    // The request_id is MCP's, and it joins the MCP and API audit lines.
    let mcp_lines = tool_audit_lines(&s.mcp.audit_lines());
    assert_eq!(mcp_lines.len(), 1, "{mcp_lines:?}");
    assert_eq!(record["request_id"], mcp_lines[0]["request_id"]);
    assert_eq!(
        mcp_lines[0]["caller"], CALLER_ID,
        "caller from the entry token"
    );
    let api_lines = common::read_audit(&s.api_audit);
    assert!(
        api_lines
            .iter()
            .any(|l| l["request_id"] == record["request_id"]),
        "{api_lines:?}"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|r| r.path == "/v1/chat/completions"));
    assert_eq!(seen[0].body["model"], "fake-model");
    let offered: Vec<&str> = seen[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(offered.len(), 3, "{offered:?}");
    assert!(
        offered.contains(&"list_countdowns")
            && offered.contains(&"get_countdown")
            && offered.contains(&"get_next_countdown")
    );
    assert_no_entry_token(&[
        &stdout,
        &stderr,
        &s.mcp.stderr(),
        &std::fs::read_to_string(&s.mcp.audit).unwrap(),
        &std::fs::read_to_string(&s.api_audit).unwrap(),
    ]);
}

/// Acceptance 2: MCP not running → exit 2 and one JSON alert line.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_down_at_startup_exits_2_with_one_json_alert() {
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "agent.token", CALLER_TOKEN, 0o600);
    let (gateway, seen) = fake_gateway(|_, _| text("不该被问到")).await;
    let mcp_url = format!("http://127.0.0.1:{}/mcp", common::free_port());
    let out = run_agent(
        agent_env(&gateway, &mcp_url, &token),
        &["有哪些倒计时？"],
        "",
    )
    .await;
    let stderr = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    let alerts: Vec<Value> = stderr
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(alerts.len(), 1, "{stderr}");
    assert_eq!(alerts[0]["level"], "alert");
    assert_eq!(alerts[0]["event"], "mcp_unavailable");
    assert_eq!(alerts[0]["mcp_url"], mcp_url);
    assert!(stdout(&out).is_empty());
    assert!(
        seen.lock().unwrap().is_empty(),
        "model asked before MCP check"
    );
    assert_no_entry_token(&[&stderr]);
}

/// Acceptance 3 (isError): the program replies 「查不到」 itself; the model
/// is not asked again, so it gets no chance to make something up.
#[tokio::test(flavor = "multi_thread")]
async fn tool_is_error_gets_fixed_reply_without_asking_the_model_again() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|n, _| match n {
        0 => tool_call("get_countdown", json!({"id": NOT_FOUND_ID})),
        _ => text("FABRICATED-明天就到"),
    })
    .await;
    let out = run_agent(
        agent_env(&gateway, &s.mcp.url(), &s.agent_token),
        &["那个倒计时还剩多久？"],
        "",
    )
    .await;
    let (stdout, stderr) = (stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout.trim(), "查不到：没有这个倒计时。");
    assert_eq!(seen.lock().unwrap().len(), 1, "model asked after a failure");
    let records = call_records(&stderr);
    assert_eq!(records.len(), 1, "{stderr}");
    assert_eq!(records[0]["ok"], false);
    assert_eq!(records[0]["error"], "没有这个倒计时");
    assert_eq!(
        records[0]["request_id"],
        tool_audit_lines(&s.mcp.audit_lines())[0]["request_id"]
    );
    assert_no_entry_token(&[&stdout, &stderr, &s.mcp.stderr()]);
}

/// Acceptance 3 (403): the API refuses the MCP service credential on the
/// tool call (the startup probe passed); same fixed reply, no second ask.
#[tokio::test(flavor = "multi_thread")]
async fn api_403_gets_fixed_reply_without_asking_the_model_again() {
    let calls = Arc::new(Mutex::new(0usize));
    let api = Router::new().fallback(move || {
        let n = {
            let mut c = calls.lock().unwrap();
            *c += 1;
            *c
        };
        async move {
            if n == 1 {
                (StatusCode::OK, "[]").into_response()
            } else {
                (StatusCode::FORBIDDEN, "{}").into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let mcp = start_mcp(dir.path(), &api_url);
    let token = token_file(dir.path(), "agent.token", CALLER_TOKEN, 0o600);
    let (gateway, seen) = fake_gateway(|n, _| match n {
        0 => tool_call("list_countdowns", json!({})),
        _ => text("FABRICATED-明天就到"),
    })
    .await;
    let out = run_agent(agent_env(&gateway, &mcp.url(), &token), &["有哪些？"], "").await;
    let stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout.trim(), "查不到：数据接口拒绝了访问（401/403）。");
    assert!(!stdout.contains("FABRICATED"));
    assert_eq!(seen.lock().unwrap().len(), 1, "model asked after a failure");
}

/// Acceptance 4a: a direct answer with no tool call is not shown; this also
/// runs the multi-turn mode (one question per stdin line).
#[tokio::test(flavor = "multi_thread")]
async fn answer_without_tool_call_is_replaced_by_not_found() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|_, _| text("MODEL-GUESS：还剩 3 天")).await;
    let out = run_agent(
        agent_env(&gateway, &s.mcp.url(), &s.agent_token),
        &[],
        "还剩几天？\n那另一个呢？\n",
    )
    .await;
    let stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stdout.contains("MODEL-GUESS"), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    for line in lines {
        assert!(line.starts_with("查不到：模型没有调用工具"), "{line}");
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // The second turn carries the first question and the fixed reply.
    let history = seen[1].body["messages"].to_string();
    assert!(history.contains("还剩几天？") && history.contains("查不到"));
    assert!(!history.contains("MODEL-GUESS"), "{history}");
    assert!(call_records(&stderr(&out)).is_empty());
}

/// Acceptance 4b: thinking in `<think>` blocks and reasoning fields never
/// reaches the answer, the call record, the audits, or the tool result.
#[tokio::test(flavor = "multi_thread")]
async fn thinking_is_never_shown_recorded_or_sent_back() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|n, body| match n {
        0 => {
            let mut m = tool_call("list_countdowns", json!({}));
            m["content"] = json!("<think>THINK-A 我先查一下列表</think>");
            m["reasoning"] = json!("THINK-B");
            m["reasoning_content"] = json!("THINK-C");
            m
        }
        _ => json!({
            "role": "assistant",
            "content": format!(
                "<think>THINK-D 工具返回了一条</think>最近的是「{}」。",
                title_from_tool_result(body)
            ),
            "reasoning": "THINK-E",
            "reasoning_content": "THINK-F",
        }),
    })
    .await;
    let out = run_agent(
        agent_env(&gateway, &s.mcp.url(), &s.agent_token),
        &["最近的是哪个？"],
        "",
    )
    .await;
    let (stdout, stderr) = (stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout.trim(), format!("最近的是「{TITLE}」。"));
    assert_eq!(call_records(&stderr).len(), 1, "{stderr}");
    let mcp_audit = std::fs::read_to_string(&s.mcp.audit).unwrap();
    let api_audit = std::fs::read_to_string(&s.api_audit).unwrap();
    for (what, text) in [
        ("answer", stdout.as_str()),
        ("stderr / call record", stderr.as_str()),
        ("MCP audit", mcp_audit.as_str()),
        ("API audit", api_audit.as_str()),
    ] {
        assert!(!text.contains("THINK-"), "thinking in {what}: {text}");
    }
    // Not sent back to the model, neither as history nor as a tool result.
    let second = seen.lock().unwrap()[1].body["messages"].to_string();
    assert!(!second.contains("THINK-"), "{second}");
}

/// Spec 4.5: at most 5 tool calls per question by default.
#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_stop_at_the_limit() {
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|_, _| tool_call("list_countdowns", json!({}))).await;
    let out = run_agent(
        agent_env(&gateway, &s.mcp.url(), &s.agent_token),
        &["一直查"],
        "",
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        "查不到：单个问题的工具调用超过上限 5 次，已停止。"
    );
    assert_eq!(call_records(&stderr(&out)).len(), 5);
    assert_eq!(tool_audit_lines(&s.mcp.audit_lines()).len(), 5);
    assert_eq!(seen.lock().unwrap().len(), 6);
}

/// Acceptance 5: `OPENAI_*` variables change nothing. Requests go only to
/// COUNTDOWN_AGENT_BASE_URL and carry none of their values.
#[tokio::test(flavor = "multi_thread")]
async fn openai_env_vars_are_ignored() {
    const LEAK: &str = "sk-LEAK-OPENAI-KEY-91ab";
    let s = stack().await;
    let (gateway, seen) = fake_gateway(|n, body| match n {
        0 => tool_call("list_countdowns", json!({})),
        _ => text(&title_from_tool_result(body)),
    })
    .await;
    let (decoy, decoy_seen) = fake_gateway(|_, _| text("DECOY")).await;
    let mut env = agent_env(&gateway, &s.mcp.url(), &s.agent_token);
    env.extend([
        ("OPENAI_BASE_URL", decoy),
        ("OPENAI_API_KEY", LEAK.to_owned()),
        ("OPENAI_ADMIN_KEY", format!("{LEAK}-admin")),
        ("OPENAI_ORG_ID", format!("{LEAK}-org")),
        ("OPENAI_PROJECT_ID", format!("{LEAK}-project")),
    ]);
    let out = run_agent(env, &["有哪些？"], "").await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains(TITLE));
    assert!(
        decoy_seen.lock().unwrap().is_empty(),
        "request went to OPENAI_BASE_URL"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for request in seen.iter() {
        assert_eq!(request.headers["authorization"], "Bearer ollama");
        for (name, value) in &request.headers {
            let value = value.to_str().unwrap_or("");
            assert!(!value.contains("LEAK"), "{name}: {value}");
        }
    }
}

/// Acceptance 5: a gateway or MCP address off this machine is refused before
/// anything is sent; missing settings are named. All exit 2.
#[tokio::test(flavor = "multi_thread")]
async fn bad_settings_refuse_startup_with_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "agent.token", CALLER_TOKEN, 0o600);
    let ok_mcp = "http://127.0.0.1:1/mcp";
    let cases: Vec<(Vec<(&'static str, String)>, &str)> = vec![
        (
            agent_env("http://192.0.2.1:11434/v1", ok_mcp, &token),
            "COUNTDOWN_AGENT_BASE_URL",
        ),
        (
            agent_env(
                "http://127.0.0.1:11434/v1",
                "http://192.0.2.1:8765/mcp",
                &token,
            ),
            "COUNTDOWN_MCP_URL",
        ),
        (
            vec![(
                "COUNTDOWN_AGENT_MCP_TOKEN_FILE",
                token.display().to_string(),
            )],
            "COUNTDOWN_AGENT_MODEL",
        ),
        (
            vec![("COUNTDOWN_AGENT_MODEL", "m".to_owned())],
            "COUNTDOWN_AGENT_MCP_TOKEN_FILE",
        ),
    ];
    for (env, named) in cases {
        let out = run_agent(env, &["q"], "").await;
        assert_eq!(out.status.code(), Some(2), "{named}: {}", stderr(&out));
        assert!(stderr(&out).contains(named), "{named}: {}", stderr(&out));
    }
}
