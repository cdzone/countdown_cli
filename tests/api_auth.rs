//! The API's token gate and audit log, checked against the real binaries.

mod common;

use std::{ffi::OsStr, path::Path, process::Command, time::Duration};

use common::{
    caller_auth, token_file, token_table, CliProcess, McpClient, McpProcess, CALLER_ID,
    CALLER_TOKEN, SERVICE_TOKEN,
};
use serde_json::Value;

const CONFIG: &str = r#"# keep me
[[countdown]]
title = "A"
datetime = "2030-01-01 00:00:00"
"#;
const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";

fn tables() -> String {
    token_table(&[
        ("mcp-svc", SERVICE_TOKEN, "countdowns:read"),
        ("admin", ADMIN_TOKEN, "countdowns:write"),
    ])
}

/// Runs `countdown_cli --api` to completion; it must refuse to start.
fn refused(dir: &Path, extra: &[&OsStr]) -> std::process::Output {
    let config = dir.join("c.toml");
    common::output_with_deadline(
        Command::new(env!("CARGO_BIN_EXE_countdown_cli"))
            .env_remove("COUNTDOWN_API_TOKENS")
            .arg("-c")
            .arg(&config)
            .args(["--api", "127.0.0.1:0"])
            .args(extra),
    )
}

#[test]
fn refused_startups_leave_config_untouched_and_create_no_audit_file() {
    for (case, table) in [
        ("no table", None),
        ("empty", Some("")),
        ("corrupt", Some("[[token]\n")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.toml"), CONFIG).unwrap();
        let audit = dir.path().join("audit.jsonl");
        let mut args: Vec<&OsStr> = vec!["--audit-log".as_ref(), audit.as_os_str()];
        let table_path = dir.path().join("tokens.toml");
        if let Some(text) = table {
            std::fs::write(&table_path, text).unwrap();
            args.extend(["--api-tokens".as_ref(), table_path.as_os_str()]);
        }
        let out = refused(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{case}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.toml")).unwrap(),
            CONFIG
        );
        assert!(!dir.path().join("c.toml.bak").exists(), "{case}");
        assert!(!audit.exists(), "{case}: audit file created");
    }
}

#[test]
fn unwritable_audit_log_refuses_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("c.toml"), CONFIG).unwrap();
    let table = dir.path().join("tokens.toml");
    std::fs::write(&table, tables()).unwrap();
    let audit = dir.path().join("no-such-dir/audit.jsonl");
    let out = refused(
        dir.path(),
        &[
            "--api-tokens".as_ref(),
            table.as_os_str(),
            "--audit-log".as_ref(),
            audit.as_os_str(),
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("c.toml")).unwrap(),
        CONFIG
    );
    assert!(!dir.path().join("c.toml.bak").exists());
}

/// Starts the real API and waits until it answers.
async fn start_api(dir: &Path, audit: &Path) -> (CliProcess, String) {
    start_api_with(dir, audit, |_| {}).await
}

/// `start_api` with `extra` applied to the command before it starts.
#[cfg_attr(target_os = "macos", allow(dead_code))]
async fn start_api_with(
    dir: &Path,
    audit: &Path,
    extra: impl FnOnce(&mut Command),
) -> (CliProcess, String) {
    let config = dir.join("c.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let table = dir.join("tokens.toml");
    std::fs::write(&table, tables()).unwrap();
    let addr = format!("127.0.0.1:{}", common::free_port());
    let proc = CliProcess::spawn_with(
        &[
            "-c".as_ref(),
            config.as_os_str(),
            "--api".as_ref(),
            addr.as_ref(),
            "--api-tokens".as_ref(),
            table.as_os_str(),
            "--audit-log".as_ref(),
            audit.as_os_str(),
        ],
        extra,
    );
    let base = format!("http://{addr}");
    let http = client();
    // The port is bound before the server runs, so a probe can connect and
    // then wait forever: bound each probe and the whole wait.
    let deadline = std::time::Instant::now() + common::PROCESS_TIMEOUT;
    while std::time::Instant::now() < deadline {
        let probe = http
            .get(format!("{base}/countdowns"))
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        if probe.is_ok() {
            return (proc, base);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "API at {base} did not answer within {:?}; stderr: {}",
        common::PROCESS_TIMEOUT,
        proc.stderr()
    );
}

fn client() -> reqwest::Client {
    common::http_client()
}

fn audit_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("each audit line is JSON"))
        .collect()
}

/// Real API + real MCP: a tool call must succeed and match a direct GET.
#[tokio::test]
async fn mcp_tool_call_through_the_real_api_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let (api, base) = start_api(dir.path(), &audit).await;
    let svc_file = token_file(dir.path(), "svc.token", SERVICE_TOKEN, 0o600);
    let mcp = McpProcess::spawn(&base, Some(&svc_file), |_| {});
    let mcp_client = McpClient::connect(mcp.url(), Some(&caller_auth())).await;

    let result = mcp_client
        .call_tool("list_countdowns", serde_json::json!({}))
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let via_mcp: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    let direct: Value = client()
        .get(format!("{base}/countdowns"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids = |v: &Value| -> Vec<Value> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].clone())
            .collect()
    };
    assert_eq!(ids(&via_mcp), ids(&direct));
    assert_eq!(ids(&direct).len(), 1);

    // A failing call (API 404) must be audited on both sides as well.
    let missing = mcp_client
        .call_tool(
            "get_countdown",
            serde_json::json!({"id": "00000000-0000-4000-8000-000000000000"}),
        )
        .await;
    assert_eq!(missing["isError"], true, "{missing}");

    // Acceptance 10: one MCP line per tool call, joined to the API line by
    // request_id; the API records the MCP caller as on_behalf_of.
    let mcp_lines = mcp.audit_lines();
    let api_lines = audit_lines(&audit);
    let tool_lines: Vec<&Value> = mcp_lines
        .iter()
        .filter(|l| l["action"].as_str().unwrap().starts_with("tool:"))
        .collect();
    assert_eq!(tool_lines.len(), 2, "{mcp_lines:?}");
    for (line, result, outcome, api_outcome) in [
        (tool_lines[0], &result, "ok", "ok"),
        (tool_lines[1], &missing, "not_found", "not_found"),
    ] {
        let id = line["request_id"].as_str().unwrap();
        assert_eq!(result["_meta"]["request_id"], id, "{result}");
        assert_eq!(line["svc"], "mcp");
        assert_eq!(line["caller"], CALLER_ID);
        assert_eq!(line["outcome"], outcome);
        for field in [
            "ts",
            "on_behalf_of",
            "target",
            "params",
            "is_error",
            "duration_ms",
        ] {
            assert!(line.get(field).is_some(), "{field} missing in {line}");
        }
        let api_line = api_lines
            .iter()
            .find(|l| l["request_id"] == id)
            .unwrap_or_else(|| panic!("no API line for {id}: {api_lines:?}"));
        assert_eq!(api_line["caller"], "mcp-svc");
        assert_eq!(api_line["on_behalf_of"], CALLER_ID);
        assert_eq!(api_line["outcome"], api_outcome);
    }

    // Acceptance 11: no token or digest on either process's stderr or in
    // either audit file.
    let everything = format!(
        "{}{}{}{}",
        api.stderr(),
        mcp.stderr(),
        std::fs::read_to_string(&audit).unwrap(),
        std::fs::read_to_string(&mcp.audit).unwrap()
    );
    for secret in [SERVICE_TOKEN, ADMIN_TOKEN, CALLER_TOKEN] {
        assert!(!everything.contains(secret), "token leaked");
        assert!(
            !everything.contains(&common::sha256_hex(secret)),
            "digest leaked"
        );
    }

    let from_mcp: Vec<Value> = audit_lines(&audit)
        .into_iter()
        .filter(|l| l["caller"] == "mcp-svc" && l["action"] == "GET /countdowns")
        .collect();
    // startup probe + the tool call
    assert!(from_mcp.len() >= 2, "{from_mcp:?}");
    for line in &from_mcp {
        assert_eq!(line["outcome"], "ok");
        assert_eq!(line["action"], "GET /countdowns");
        assert!(uuid::Uuid::parse_str(line["request_id"].as_str().unwrap()).is_ok());
    }
    drop(api);
}

#[tokio::test]
async fn audit_failure_blocks_writes_but_not_reads_and_stays_off_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let (api, base) = start_api(dir.path(), &audit).await;
    let http = client();
    let list: Value = http
        .get(format!("{base}/countdowns"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let item = format!("{base}/countdowns/{}", list[0]["id"].as_str().unwrap());
    let config = dir.path().join("c.toml");
    let before = std::fs::read(&config).unwrap();
    let stderr_before = api.stderr();

    std::fs::remove_file(&audit).unwrap();
    std::fs::create_dir(&audit).unwrap();

    let res = http
        .delete(&item)
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 500);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "audit_unavailable");
    assert_eq!(std::fs::read(&config).unwrap(), before);

    let res = http
        .get(&item)
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-audit-status"], "unavailable");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(api.stderr(), stderr_before, "API wrote to stderr");
}

/// Kills every fake `terminal-notifier` that recorded its pid, so a red run
/// does not leave `sleep 600` behind.
#[cfg(all(unix, not(target_os = "macos")))]
struct FakeNotifierPids(std::path::PathBuf);

#[cfg(all(unix, not(target_os = "macos")))]
impl FakeNotifierPids {
    fn pids(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Drop for FakeNotifierPids {
    fn drop(&mut self) {
        for pid in self.pids() {
            let _ = Command::new("kill")
                .args(["-9", &pid])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// A `terminal-notifier` that never exits must not keep the API from serving;
/// off macOS it is never spawned at all.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn hanging_terminal_notifier_does_not_block_the_api() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let pid_file = dir.path().join("notifier.pids");
    let guard = FakeNotifierPids(pid_file.clone());
    let script = bin.join("terminal-notifier");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ >> '{}'\nexec sleep 600\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();

    let audit = dir.path().join("audit.jsonl");
    let (api, base) = start_api_with(dir.path(), &audit, |cmd| {
        cmd.env("PATH", &path);
    })
    .await;
    let res = client()
        .get(format!("{base}/countdowns"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Off macOS the notifier is never spawned and nothing is printed about it.
    assert!(
        guard.pids().is_empty(),
        "terminal-notifier spawned on non-macOS"
    );
    assert!(
        !api.stderr().contains("terminal-notifier"),
        "stderr: {}",
        api.stderr()
    );
    drop(api);
}

/// Bodies over 2 MiB are refused with 413 before the write runs.
#[tokio::test]
async fn oversized_write_body_is_413_and_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let (_api, base) = start_api(dir.path(), &audit).await;
    let config = dir.path().join("c.toml");
    let before = std::fs::read(&config).unwrap();
    let before_lines = audit_lines(&audit).len();

    let title = "x".repeat(3 * 1024 * 1024);
    let res = client()
        .post(format!("{base}/countdowns"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&serde_json::json!({"title": title, "datetime": "2030-01-01 00:00:00"}))
        .send()
        .await;
    match res {
        Ok(res) => {
            assert_eq!(res.status(), 413);
            let body: Value = res.json().await.unwrap();
            assert_eq!(body["error"]["code"], "payload_too_large");
        }
        // The server may reply 413 and close while we still upload.
        Err(e) => assert!(!e.is_timeout(), "unexpected error: {e:?}"),
    }
    assert_eq!(std::fs::read(&config).unwrap(), before, "write executed");

    // The audit line may land after the client sees the reset.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut new: Vec<Value> = audit_lines(&audit).split_off(before_lines);
    while !new.iter().any(|l| l["status"] == 413) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        new = audit_lines(&audit).split_off(before_lines);
    }
    assert!(new.iter().all(|l| l["phase"] != "attempt"), "{new:?}");
    let result: Vec<&Value> = new.iter().filter(|l| l["status"] == 413).collect();
    assert_eq!(result.len(), 1, "{new:?}");
    assert_eq!(result[0]["outcome"], "invalid");
    assert_eq!(result[0]["caller"], "admin");
    assert!(!std::fs::read_to_string(&audit)
        .unwrap()
        .contains(&"x".repeat(64)));
}
