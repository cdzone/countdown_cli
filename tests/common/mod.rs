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

/// Upper bound for one HTTP request in tests: a server that accepts the
/// connection but never answers must fail the test, not hang CI.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound for waiting on a child process or its stderr reader.
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);

/// A reqwest client for tests: no proxy, every request bounded by `HTTP_TIMEOUT`.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(HTTP_TIMEOUT)
        .build()
        .unwrap()
}

/// Joins a stderr reader thread, panicking instead of blocking forever when
/// the pipe stays open (e.g. inherited by a grandchild).
fn join_reader(reader: JoinHandle<()>, stderr: &Arc<Mutex<String>>) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            panic!(
                "stderr reader still open after {PROCESS_TIMEOUT:?}; stderr so far: {}",
                stderr.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = reader.join();
}

/// Runs `cmd` to completion with stdin closed; kills it and panics if it is
/// still running after `PROCESS_TIMEOUT`.
pub fn output_with_deadline(cmd: &mut Command) -> std::process::Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "process did not exit within {PROCESS_TIMEOUT:?}; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Describes whether `child` is still running and, on Linux, what each of its
/// threads is blocked in, so a timed-out test shows where the server stopped.
fn child_state(child: &mut Child) -> String {
    match child.try_wait() {
        Ok(Some(status)) => format!("exited: {status}"),
        Ok(None) => {
            let pid = child.id();
            let mut out = format!("still running (pid {pid})");
            if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
                for task in tasks.flatten() {
                    let dir = task.path();
                    let read = |f: &str| {
                        std::fs::read_to_string(dir.join(f))
                            .unwrap_or_default()
                            .trim()
                            .to_owned()
                    };
                    out.push_str(&format!(
                        "\n  thread {} {}: wchan={}",
                        task.file_name().to_string_lossy(),
                        read("comm"),
                        read("wchan")
                    ));
                }
            }
            out
        }
        Err(e) => format!("state unknown: {e}"),
    }
}

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
        let deadline = Instant::now() + PROCESS_TIMEOUT;
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
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                if let Some(reader) = self.reader.take() {
                    join_reader(reader, &self.stderr);
                }
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
        if let Some(reader) = self.reader.take() {
            join_reader(reader, &self.stderr);
        }
        self.stderr()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "countdown_mcp {}\nstderr:\n{}",
                child_state(&mut self.child),
                self.stderr()
            );
        }
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
            http: http_client(),
            session: None,
            authorization: authorization.map(str::to_owned),
        };
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}});
        let mut resp = None;
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            let attempt = tokio::time::timeout(Duration::from_secs(1), client.post(init.clone()));
            match attempt.await {
                Ok(Ok(r)) => {
                    resp = Some(r);
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let resp = resp.unwrap_or_else(|| panic!("countdown_mcp did not start at {}", client.url));
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

pub fn sha256_hex(s: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A token table with one entry per `(id, token, scope)`.
pub fn token_table(entries: &[(&str, &str, &str)]) -> String {
    entries
        .iter()
        .map(|(id, token, scope)| {
            format!(
                "[[token]]\nid = \"{id}\"\nsha256 = \"{}\"\nscopes = [\"{scope}\"]\n",
                sha256_hex(token)
            )
        })
        .collect()
}

/// `countdown_cli` with the given args, stdin held open so its terminal loop
/// blocks instead of spinning on EOF; killed on drop.
pub struct CliProcess {
    child: Child,
    stderr: Arc<Mutex<String>>,
    _stdin: std::process::ChildStdin,
}

impl CliProcess {
    pub fn spawn(args: &[&std::ffi::OsStr]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_countdown_cli"))
            .args(args)
            .env_remove("COUNTDOWN_API_TOKENS")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
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
        Self {
            child,
            stderr,
            _stdin: stdin,
        }
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        // A timed-out request panics; show what the API said so a red CI run
        // explains itself.
        if std::thread::panicking() {
            eprintln!(
                "countdown_cli {}\nstderr:\n{}",
                child_state(&mut self.child),
                self.stderr()
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
