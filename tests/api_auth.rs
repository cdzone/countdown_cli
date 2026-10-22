//! The API's token gate and audit log, checked against the real binaries.

mod common;

use std::{ffi::OsStr, path::Path, process::Command, time::Duration};

use common::{token_file, token_table, CliProcess, McpClient, McpProcess, SERVICE_TOKEN};
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
    let config = dir.join("c.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let table = dir.join("tokens.toml");
    std::fs::write(&table, tables()).unwrap();
    let addr = format!("127.0.0.1:{}", common::free_port());
    let proc = CliProcess::spawn(&[
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
    let mcp_client = McpClient::connect(mcp.url(), None).await;

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

    let from_mcp: Vec<Value> = audit_lines(&audit)
        .into_iter()
        .filter(|l| l["caller"] == "mcp-svc")
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
