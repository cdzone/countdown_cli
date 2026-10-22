mod common;

use std::process::Command;

const CONFIG: &str = r#"# keep me
[[countdown]]
title = "A"
datetime = "2030-01-01 00:00:00"
"#;

#[test]
fn rejected_api_addr_leaves_config_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("c.toml");
    std::fs::write(&config, CONFIG).unwrap();

    let output = common::output_with_deadline(
        Command::new(env!("CARGO_BIN_EXE_countdown_cli"))
            .arg("-c")
            .arg(&config)
            .arg("--api")
            .arg("0.0.0.0:0"),
    );

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
    assert!(!config.with_extension("toml.bak").exists());
}
