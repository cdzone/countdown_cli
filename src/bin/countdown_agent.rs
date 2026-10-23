//! Command-line agent: answers countdown questions with a local
//! OpenAI-compatible model gateway (Ollama) and the read-only MCP tools.
//!
//! Data comes only from the MCP tools `list_countdowns`, `get_countdown`, and
//! `get_next_countdown`; this program never calls the countdown API. Whenever a
//! tool fails, or the model answers without calling a tool, the user gets a
//! fixed 「查不到」 reply instead of whatever the model would have said.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
    process::ExitCode,
    time::{Duration, Instant},
};

use async_openai::{
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessage, ChatCompletionRequestToolMessage,
        ChatCompletionRequestUserMessage, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequestArgs, FunctionObject,
    },
    Client,
};
use countdown_cli::auth::read_token_file;
use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    RoleClient, ServiceExt,
};
use serde::Serialize;
use serde_json::Value;

#[path = "agent/agui.rs"]
mod agui;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434/v1";
const DEFAULT_MCP_URL: &str = "http://127.0.0.1:8765/mcp";
/// Ollama requires a non-empty key but ignores it.
const PLACEHOLDER_API_KEY: &str = "ollama";
const DEFAULT_MAX_TOOL_CALLS: usize = 5;
/// The read-only tools the model may call; nothing else is offered to it.
const TOOLS: [&str; 3] = ["list_countdowns", "get_countdown", "get_next_countdown"];
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Reasoning models on a laptop can take minutes to answer.
const MODEL_TIMEOUT: Duration = Duration::from_secs(300);

const SYSTEM_PROMPT: &str = "你是倒计时助手。凡是涉及倒计时的问题，必须先调用工具查询，\
只根据工具返回的数据用中文简短回答；不要编造工具没有返回的内容。\
用户问最近的／下一个／马上到期的倒计时，必须调用 get_next_countdown，\
不要用 list_countdowns 自己挑。问有哪些／哪些过期了用 list_countdowns；\
已知 id 用 get_countdown。";

const USAGE: &str = "usage: countdown_agent [question...]
       countdown_agent --serve [127.0.0.1:PORT]
  With a question: answer it once and exit. Without: read one question per line.
  --serve: AG-UI server (POST /agui, SSE) plus the web page, default 127.0.0.1:8780.
  Environment:
    COUNTDOWN_AGENT_MODEL           model name (required; must support tool calls)
    COUNTDOWN_AGENT_MCP_TOKEN_FILE  file with the MCP entry token (required, mode 600)
    COUNTDOWN_AGENT_BASE_URL        default http://127.0.0.1:11434/v1 (loopback only)
    COUNTDOWN_AGENT_API_KEY         default: a fixed placeholder
    COUNTDOWN_MCP_URL               default http://127.0.0.1:8765/mcp (loopback only)
    COUNTDOWN_AGENT_MAX_TOOL_CALLS  tool calls per question, default 5
    COUNTDOWN_AGENT_WEB_DIR         web page files, default web/dist next to the executable";

struct Config {
    base_url: String,
    api_key: String,
    model: String,
    mcp_url: String,
    mcp_token: String,
    max_tool_calls: usize,
}

/// Only these hosts are accepted, so neither questions nor the MCP entry
/// token can leave the machine.
fn loopback_url(name: &str, value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|e| format!("{name}: invalid URL: {e}"))?;
    if url.scheme() != "http" {
        return Err(format!("{name}: only http:// is supported"));
    }
    match url.host_str() {
        Some("127.0.0.1" | "localhost" | "[::1]") => Ok(value.trim_end_matches('/').to_owned()),
        host => Err(format!(
            "{name}: host {} is not 127.0.0.1, localhost or ::1",
            host.unwrap_or("(none)")
        )),
    }
}

impl Config {
    /// `var` reads one setting. Nothing named `OPENAI_*` is ever asked for.
    fn load(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let required = |name: &str| var(name).ok_or_else(|| format!("{name} is not set"));
        let base_url = loopback_url(
            "COUNTDOWN_AGENT_BASE_URL",
            &var("COUNTDOWN_AGENT_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
        )?;
        let mcp_url = loopback_url(
            "COUNTDOWN_MCP_URL",
            &var("COUNTDOWN_MCP_URL").unwrap_or_else(|| DEFAULT_MCP_URL.into()),
        )?;
        let model = required("COUNTDOWN_AGENT_MODEL")?;
        let api_key = var("COUNTDOWN_AGENT_API_KEY").unwrap_or_else(|| PLACEHOLDER_API_KEY.into());
        if !api_key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("COUNTDOWN_AGENT_API_KEY must be visible ASCII".into());
        }
        let max_tool_calls = match var("COUNTDOWN_AGENT_MAX_TOOL_CALLS") {
            None => DEFAULT_MAX_TOOL_CALLS,
            Some(v) => v
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("COUNTDOWN_AGENT_MAX_TOOL_CALLS must be a positive integer")?,
        };
        let token_file = required("COUNTDOWN_AGENT_MCP_TOKEN_FILE")?;
        let mcp_token = read_token_file(Path::new(&token_file))
            .map_err(|e| format!("COUNTDOWN_AGENT_MCP_TOKEN_FILE {token_file}: {e}"))?;
        Ok(Self {
            base_url,
            api_key,
            model,
            mcp_url,
            mcp_token,
            max_tool_calls,
        })
    }

    /// Both address and key are set explicitly, and the organization and
    /// project headers are cleared, so no `OPENAI_*` value that
    /// `OpenAIConfig::new()` picked up from the environment is ever sent.
    fn gateway(&self) -> Client<OpenAIConfig> {
        let config = OpenAIConfig::new()
            .with_api_base(&self.base_url)
            .with_api_key(&self.api_key)
            .with_org_id("")
            .with_project_id("");
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(MODEL_TIMEOUT)
            .build()
            .expect("build HTTP client");
        Client::build(http, config)
    }
}

/// Startup failure, printed to stderr as one JSON line.
#[derive(Serialize)]
struct Alert {
    level: &'static str,
    svc: &'static str,
    event: &'static str,
    mcp_url: String,
    reason: String,
}

type Mcp = RunningService<RoleClient, ()>;

/// Connects to MCP and checks that both read-only tools are offered.
/// Returns the tool definitions to hand to the model.
async fn connect_mcp(cfg: &Config) -> Result<(Mcp, Vec<ChatCompletionTools>), Alert> {
    let alert = |event, reason| Alert {
        level: "alert",
        svc: "agent",
        event,
        mcp_url: cfg.mcp_url.clone(),
        reason,
    };
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build HTTP client");
    let transport = StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(cfg.mcp_url.as_str())
            .auth_header(cfg.mcp_token.as_str()),
    );
    let connected = tokio::time::timeout(STARTUP_TIMEOUT, async {
        let mcp = ().serve(transport).await.map_err(|e| e.to_string())?;
        let tools = mcp.list_all_tools().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((mcp, tools))
    })
    .await;
    let (mcp, tools) = match connected {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => return Err(alert("mcp_unavailable", e)),
        Err(_) => return Err(alert("mcp_unavailable", "timed out".into())),
    };
    let offered: Vec<ChatCompletionTools> = tools
        .iter()
        .filter(|t| TOOLS.contains(&t.name.as_ref()))
        .map(|t| {
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: t.name.to_string(),
                    description: t.description.as_ref().map(|d| d.to_string()),
                    parameters: Some(Value::Object((*t.input_schema).clone())),
                    strict: None,
                },
            })
        })
        .collect();
    let missing = missing_tools(tools.iter().map(|t| t.name.as_ref()));
    if !missing.is_empty() {
        return Err(alert(
            "mcp_tools_missing",
            format!("missing tools: {}", missing.join(", ")),
        ));
    }
    Ok((mcp, offered))
}

fn missing_tools<'a>(offered: impl Iterator<Item = &'a str> + Clone) -> Vec<&'static str> {
    TOOLS
        .into_iter()
        .filter(|want| !offered.clone().any(|name| name == *want))
        .collect()
}

/// Removes a reasoning model's thinking: every `<think>…</think>` block, an
/// unterminated `<think>` to the end, and text before a leading `</think>`
/// (some chat templates open the block in the prompt).
fn strip_thinking(text: &str) -> String {
    let mut rest = text;
    if let Some(end) = rest.find("</think>") {
        if !rest[..end].contains("<think>") {
            rest = &rest[end + "</think>".len()..];
        }
    }
    let mut out = String::new();
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

fn not_found(reason: &str) -> String {
    format!("查不到：{reason}。")
}

/// One printed line per tool call (spec 4.3). Carries no tool output and no
/// model text, only what the model asked for and how it went.
#[derive(Serialize)]
struct CallRecord<'a> {
    tool: &'a str,
    args: &'a Value,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    duration_ms: u64,
    request_id: Option<&'a str>,
}

/// Runs one tool call. `Err` is the reason category shown to the user; the
/// second value is MCP's `_meta.request_id`, when the call reached MCP.
async fn call_tool(
    mcp: &Mcp,
    call: &ChatCompletionMessageToolCall,
) -> (Result<String, String>, Option<String>) {
    let name = call.function.name.as_str();
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    let started = Instant::now();
    let (outcome, request_id) = match (&args, TOOLS.contains(&name)) {
        (_, false) => (Err("模型请求了只读清单以外的工具"), None),
        (Value::Object(obj), true) => {
            let params = CallToolRequestParams::new(name.to_owned()).with_arguments(obj.clone());
            match tokio::time::timeout(TOOL_TIMEOUT, mcp.call_tool(params)).await {
                Err(_) => (Err("工具调用超时"), None),
                Ok(Err(_)) => (Err("调用 MCP 失败（网络或鉴权错误）"), None),
                Ok(Ok(result)) => {
                    let request_id = result
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("request_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    (tool_outcome(&result), request_id)
                }
            }
        }
        _ => (Err("模型给的工具参数不是 JSON 对象"), None),
    };
    let record = CallRecord {
        tool: name,
        args: &args,
        ok: outcome.is_ok(),
        error: outcome.as_ref().err().copied(),
        duration_ms: started.elapsed().as_millis() as u64,
        request_id: request_id.as_deref(),
    };
    eprintln!(
        "调用记录 {}",
        serde_json::to_string(&record).expect("record is JSON")
    );
    (outcome.map_err(str::to_owned), request_id)
}

/// Text of a successful result, or the reason category of a failed one.
/// Branches on `structuredContent.error.code` only — never on message text.
fn tool_outcome(result: &CallToolResult) -> Result<String, &'static str> {
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    if result.is_error != Some(true) {
        return Ok(text);
    }
    let code = result
        .structured_content
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str());
    Err(match code {
        Some("not_found") => "没有这个倒计时",
        Some("unauthenticated") | Some("permission_denied") => "数据接口拒绝了访问（401/403）",
        Some("upstream_unavailable") => "数据接口连不上",
        Some("contract_mismatch") => "数据接口契约不匹配",
        _ => "工具返回错误",
    })
}

/// Everything one question needs; shared by the command line and the
/// AG-UI server.
struct Shared {
    gateway: Client<OpenAIConfig>,
    mcp: Mcp,
    tools: Vec<ChatCompletionTools>,
    model: String,
    max_tool_calls: usize,
}

/// Progress of one question, for callers that show tool calls.
enum Step<'a> {
    ToolStart {
        id: &'a str,
        name: &'a str,
        args: &'a str,
    },
    ToolResult {
        id: &'a str,
        outcome: Result<&'a str, &'a str>,
        request_id: Option<&'a str>,
    },
}

impl Shared {
    /// Answers the last user message in `messages` (system prompt and earlier
    /// turns first). `Err` means the model gateway itself failed.
    async fn answer(
        &self,
        mut messages: Vec<ChatCompletionRequestMessage>,
        step: &mut (dyn FnMut(Step<'_>) + Send),
    ) -> Result<String, String> {
        let mut calls_made = 0;
        let reply = loop {
            let request = CreateChatCompletionRequestArgs::default()
                .model(&self.model)
                .messages(messages.clone())
                .tools(self.tools.clone())
                .build()
                .map_err(|e| e.to_string())?;
            let response = self
                .gateway
                .chat()
                .create(request)
                .await
                .map_err(|e| e.to_string())?;
            let Some(choice) = response.choices.into_iter().next() else {
                return Err("model gateway returned no choices".into());
            };
            let calls: Vec<ChatCompletionMessageToolCall> = choice
                .message
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| match c {
                    ChatCompletionMessageToolCalls::Function(f) => Some(f),
                    ChatCompletionMessageToolCalls::Custom(_) => None,
                })
                .collect();
            if calls.is_empty() {
                // Ollama cannot force a tool call (no tool_choice): an answer
                // given without any tool result is not trusted.
                let answer = strip_thinking(choice.message.content.as_deref().unwrap_or(""));
                break if calls_made == 0 {
                    not_found("模型没有调用工具查询数据，不能把它的回答当作查到的结果")
                } else if answer.is_empty() {
                    not_found("模型没有给出回答")
                } else {
                    answer
                };
            }
            // The thinking text is not sent back: only the calls are.
            messages.push(
                ChatCompletionRequestAssistantMessage {
                    tool_calls: Some(
                        calls
                            .iter()
                            .cloned()
                            .map(ChatCompletionMessageToolCalls::Function)
                            .collect(),
                    ),
                    ..Default::default()
                }
                .into(),
            );
            let mut failed = None;
            for call in &calls {
                if calls_made == self.max_tool_calls {
                    failed = Some(format!(
                        "单个问题的工具调用超过上限 {} 次，已停止",
                        self.max_tool_calls
                    ));
                    break;
                }
                calls_made += 1;
                step(Step::ToolStart {
                    id: &call.id,
                    name: &call.function.name,
                    args: &call.function.arguments,
                });
                let (outcome, request_id) = call_tool(&self.mcp, call).await;
                step(Step::ToolResult {
                    id: &call.id,
                    outcome: outcome.as_deref().map_err(String::as_str),
                    request_id: request_id.as_deref(),
                });
                match outcome {
                    Ok(text) => messages.push(
                        ChatCompletionRequestToolMessage {
                            content: text.into(),
                            tool_call_id: call.id.clone(),
                        }
                        .into(),
                    ),
                    // The program answers; the model never sees the failure.
                    Err(reason) => {
                        failed = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = failed {
                break not_found(&reason);
            }
        };
        Ok(reply)
    }
}

struct Agent {
    shared: Shared,
    /// System prompt plus earlier questions and final replies only.
    history: Vec<ChatCompletionRequestMessage>,
}

impl Agent {
    /// Answers one question. `Err` means the model gateway itself failed.
    async fn ask(&mut self, question: &str) -> Result<String, String> {
        let mut messages = self.history.clone();
        messages.push(ChatCompletionRequestUserMessage::from(question).into());
        let reply = self.shared.answer(messages, &mut |_| {}).await?;
        self.history
            .push(ChatCompletionRequestUserMessage::from(question).into());
        self.history
            .push(ChatCompletionRequestAssistantMessage::from(reply.as_str()).into());
        Ok(reply)
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let question = args.join(" ");
    if question == "-h" || question == "--help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let serve = match args.first().map(String::as_str) {
        Some("--serve") => match args.get(1..) {
            Some([]) | None => Some(agui::DEFAULT_ADDR.to_owned()),
            Some([addr]) => Some(addr.clone()),
            Some(_) => {
                eprintln!("countdown_agent: --serve takes at most one address\n{USAGE}");
                return ExitCode::from(2);
            }
        },
        _ => None,
    };
    let listen = match serve.as_deref().map(agui::listen_addr) {
        None => None,
        Some(Ok(addr)) => Some(addr),
        Some(Err(e)) => {
            eprintln!("countdown_agent: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let cfg = match Config::load(env_var) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("countdown_agent: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (mcp, tools) = match connect_mcp(&cfg).await {
        Ok(found) => found,
        Err(alert) => {
            eprintln!("{}", serde_json::to_string(&alert).expect("alert is JSON"));
            return ExitCode::from(2);
        }
    };
    let shared = Shared {
        gateway: cfg.gateway(),
        mcp,
        tools,
        model: cfg.model,
        max_tool_calls: cfg.max_tool_calls,
    };
    if let Some(addr) = listen {
        let web_dir = agui::web_dir(
            env_var(agui::WEB_DIR_VAR),
            std::env::current_exe().ok().as_deref(),
        );
        return agui::serve(std::sync::Arc::new(shared), addr, web_dir).await;
    }
    let mut agent = Agent {
        shared,
        history: vec![ChatCompletionRequestSystemMessage::from(SYSTEM_PROMPT).into()],
    };
    if !question.is_empty() {
        return match agent.ask(&question).await {
            Ok(reply) => {
                println!("{reply}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("countdown_agent: model gateway error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let interactive = std::io::stdin().is_terminal();
    let mut lines = std::io::stdin().lock().lines();
    loop {
        if interactive {
            print!("> ");
            let _ = std::io::stdout().flush();
        }
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match agent.ask(line).await {
            Ok(reply) => println!("{reply}"),
            Err(e) => eprintln!("countdown_agent: model gateway error: {e}"),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::load(|name| vars.get(name).cloned())
    }

    #[test]
    fn thinking_is_removed_in_every_shape() {
        for (raw, want) in [
            ("<think>plan</think>答案", "答案"),
            ("前<think>a</think>中<think>b</think>后", "前中后"),
            ("plan</think>\n答案", "答案"),
            ("答案<think>truncated", "答案"),
            ("<think>only</think>", ""),
            ("没有思考", "没有思考"),
        ] {
            assert_eq!(strip_thinking(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn only_loopback_http_urls_are_accepted() {
        for ok in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1/",
            "http://[::1]:11434/v1",
        ] {
            assert!(loopback_url("X", ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://10.0.0.1:11434/v1",
            "http://0.0.0.0:11434/v1",
            "http://example.com/v1",
            "https://127.0.0.1/v1",
            "http://localhost.example.com/v1",
            "not a url",
        ] {
            assert!(loopback_url("X", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_settings_are_named() {
        let err = load(&[]).err().unwrap();
        assert!(err.contains("COUNTDOWN_AGENT_MODEL"), "{err}");
        let err = load(&[("COUNTDOWN_AGENT_MODEL", "m")]).err().unwrap();
        assert!(err.contains("COUNTDOWN_AGENT_MCP_TOKEN_FILE"), "{err}");
        let err = load(&[
            ("COUNTDOWN_AGENT_MODEL", "m"),
            ("COUNTDOWN_AGENT_MAX_TOOL_CALLS", "0"),
        ])
        .err()
        .unwrap();
        assert!(err.contains("COUNTDOWN_AGENT_MAX_TOOL_CALLS"), "{err}");
    }

    #[test]
    fn missing_tools_lists_what_is_absent() {
        assert_eq!(
            missing_tools(["get_countdown", "other"].into_iter()),
            vec!["list_countdowns", "get_next_countdown"]
        );
        assert!(missing_tools(TOOLS.into_iter()).is_empty());
    }
}
