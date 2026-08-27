use std::process::Stdio;

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

    let mut cmd = std::process::Command::new("terminal-notifier");
    cmd.args(["-message", message, "-title", title, "-group", title])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if custom_sound.is_none() {
        cmd.args(["-sound", "default"]);
    }

    let mut notify_window = cmd.spawn()?;

    if let Some(sound_path) = custom_sound {
        let mut sound_process = std::process::Command::new("ffplay")
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
