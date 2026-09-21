use std::io::ErrorKind;
use std::process::{Command, Stdio};

/// Probe whether `terminal-notifier` is on PATH (macOS notification backend).
///
/// Returns `Ok(())` if the binary can be spawned. Missing binary is `Err` with
/// an install hint; other spawn failures are also `Err`.
pub fn probe_terminal_notifier() -> Result<(), String> {
    match Command::new("terminal-notifier")
        .arg("-help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Err(
            "未找到 terminal-notifier，到期桌面通知不可用。安装: brew install terminal-notifier"
                .to_string(),
        ),
        Err(err) => Err(format!("无法启动 terminal-notifier: {err}")),
    }
}

pub async fn osx_terminal_notifier(
    title: &str,
    content: &str,
    sound: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // terminal-notifier 3.0 treats empty -message as missing and dumps Usage to stdout.
    let message = if content.trim().is_empty() {
        title
    } else {
        content
    };

    let custom_sound = sound
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty() && std::path::Path::new(path).exists());

    let mut cmd = Command::new("terminal-notifier");
    cmd.args(["-message", message, "-title", title, "-group", title])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if custom_sound.is_none() {
        cmd.args(["-sound", "default"]);
    }

    let mut notify_window = cmd.spawn()?;

    if let Some(sound_path) = custom_sound {
        let mut sound_process = Command::new("ffplay")
            .args(["-i", sound_path, "-autoexit", "-nodisp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let _ = notify_window.wait();
        let _ = sound_process.wait();
        return Ok(());
    }

    let _ = notify_window.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_installed_or_missing_clearly() {
        // On CI (Linux) this is usually Err; on the author's Mac it is Ok.
        // Either way the function must not panic and Err must be non-empty.
        match probe_terminal_notifier() {
            Ok(()) => {}
            Err(msg) => assert!(!msg.is_empty(), "missing-tool message must be non-empty"),
        }
    }
}
