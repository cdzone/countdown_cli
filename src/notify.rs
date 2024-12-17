use std::process::Stdio;

fn check_path_exist(path: &str) -> bool {
    let path_obj = std::path::Path::new(path);
    if path_obj.exists() {
        true
    } else {
        println!("{path} not exist!");
        false
    }
}

pub async fn osx_terminal_notifier(
    title: &str,
    content: &str,
    sound: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(sound_path) = sound {
        if check_path_exist(&sound_path) {
            let mut notify_window = std::process::Command::new("terminal-notifier")
                .args(["-message", content, "-title", title])
                .spawn()?;
            let mut sound_process = std::process::Command::new("ffplay")
                .args(["-i", &sound_path, "-autoexit", "-nodisp"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let _ = notify_window.wait();
            let _ = sound_process.wait();
            return Ok(());
        }
    }
    let mut notify_window = std::process::Command::new("terminal-notifier")
        .args(["-message", content, "-title", title, "-sound", "default"])
        .spawn()
        .unwrap();
    let _ = notify_window.wait();
    Ok(())
}
