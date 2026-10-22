//! Helpers for tests that run the real `countdown_mcp` binary.
// Each test crate uses a different subset.
#![allow(dead_code)]

use std::{
    io::Read,
    net::TcpListener as StdListener,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

pub const SERVICE_TOKEN: &str = "svc-token-0123456789abcdef0123456789abcdef";

pub fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Writes `content` to `dir/name` with the given Unix mode.
pub fn token_file(dir: &Path, name: &str, content: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = mode;
    path
}

/// A running `countdown_mcp`; stderr is collected, the process is killed on drop.
pub struct McpProcess {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
    pub port: u16,
}

impl McpProcess {
    /// Starts `countdown_mcp` against `api_url`; `extra` adds args or env.
    pub fn spawn(
        api_url: &str,
        token_file: Option<&Path>,
        extra: impl FnOnce(&mut Command),
    ) -> Self {
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_countdown_mcp"));
        cmd.args(["--api-url", api_url])
            .args(["--addr", &format!("127.0.0.1:{port}")])
            .env_remove("COUNTDOWN_API_TOKEN_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(path) = token_file {
            cmd.arg("--api-token-file").arg(path);
        }
        extra(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self {
            child,
            stderr,
            reader: Some(reader),
            port,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Waits until stderr contains `needle`.
    pub fn wait_for_stderr(&self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.stderr().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Waits for the process to exit on its own; `None` if it keeps running.
    pub fn wait_exit(&mut self) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reader.take().map(JoinHandle::join);
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Stops the process and returns everything it wrote to stderr.
    pub fn finish(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reader.take().map(JoinHandle::join);
        self.stderr()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns the JSON-RPC message carried in a JSON or SSE response body.
pub fn rpc_body(body: &str) -> Value {
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

/// Minimal MCP client over streamable HTTP; sends `authorization` on every
/// request when given.
pub struct McpClient {
    url: String,
    http: reqwest::Client,
    session: Option<String>,
    authorization: Option<String>,
}

impl McpClient {
    /// Initializes a session, retrying while the server starts up.
    pub async fn connect(url: String, authorization: Option<&str>) -> Self {
        let mut client = Self {
            url,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            session: None,
            authorization: authorization.map(str::to_owned),
        };
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}});
        let mut resp = None;
        for _ in 0..50 {
            match client.post(init.clone()).await {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let resp = resp.expect("countdown_mcp did not start");
        client.session = resp
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().unwrap().to_string());
        let _ = resp.text().await.unwrap();
        client
            .post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await
            .unwrap();
        client
    }

    async fn post(&self, body: Value) -> reqwest::Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        if let Some(a) = &self.authorization {
            req = req.header("authorization", a);
        }
        req.send().await
    }

    /// Calls a tool and returns the JSON-RPC `result`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}});
        let body = self.post(call).await.unwrap().text().await.unwrap();
        rpc_body(&body)["result"].clone()
    }
}
