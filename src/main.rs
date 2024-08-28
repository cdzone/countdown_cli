use chrono::{Local, NaiveDateTime};
use clap::Parser;
use colored::*;
use config::{CountDownConfig, HotReload};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, ExecutableCommand};
use notify::osx_terminal_notifier;
use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::sleep;

mod command;
mod config;
mod notify;

pub fn get_styles() -> clap::builder::Styles {
    use clap::builder::styling::*;
    Styles::styled()
        .header(AnsiColor::Yellow.on_default())
        .usage(AnsiColor::Green.on_default())
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Green.on_default())
}

#[derive(Parser, Debug)]
#[command(author="dc38528<cdzone@yeah.net>", version, about="terminal cli countdown widget.", long_about = None, styles=get_styles())]
struct CliArgs {
    #[arg(
        short = 'c',
        long = "countdown_project_config",
        default_value = "config.toml"
    )]
    config_file: String,
}

pub async fn terminal_run(if_running: Arc<AtomicBool>, config: CountDownConfig) {
    let mut stdout = stdout();
    let mut last_line_count = 0;

    while if_running.load(Ordering::SeqCst) {
        let mut target_datetimes: Vec<(String, NaiveDateTime)> = config.get_config().await
        .countdown
        .into_iter()
        .filter_map(|countdown| {
            match NaiveDateTime::parse_from_str(&countdown.datetime, "%Y-%m-%d %H:%M:%S") {
                Ok(datetime) => Some((countdown.title, datetime)),
                Err(_) => {
                    println!(
                        "Error: Invalid datetime format for '{}'. Please use 'YYYY-MM-DD HH:MM:SS' format.",
                        countdown.title
                    );
                    None
                }
            }
        })
        .collect();

        target_datetimes.sort_by(|a, b| a.1.cmp(&b.1));

        // 清除之前的输出
        for _ in 0..last_line_count {
            let _ = stdout.execute(cursor::MoveUp(1));
            let _ = stdout.execute(Clear(ClearType::CurrentLine));
        }

        // 将光标移回开始位置
        let _ = stdout.execute(cursor::MoveToColumn(0));

        for (title, target_datetime) in target_datetimes.iter() {
            let now = Local::now().naive_local();
            let remaining = *target_datetime - now;
            let remaining_seconds = remaining.num_seconds();

            let message = match remaining_seconds {
                86401_i64..=i64::MAX => {
                    let days_left = remaining.num_days();
                    let hours_left = format!("{:02}", remaining.num_hours() % 24);
                    let minutes_left = format!("{:02}", remaining.num_minutes() % 60);
                    let secs_left = format!("{:02}", remaining.num_seconds() % 60);
                    format!(
                        "{}: There are {} days, {:02}:{:02}:{:02} secs left.",
                        title.bright_magenta(),
                        days_left.to_string().bright_yellow(),
                        hours_left.bright_yellow(),
                        minutes_left.bright_yellow(),
                        secs_left.bright_yellow()
                    )
                }
                1_i64..=86400 => {
                    let hours_left = format!("{:02}", remaining.num_hours() % 24);
                    let minutes_left = format!("{:02}", remaining.num_minutes() % 60);
                    let secs_left = format!("{:02}", remaining.num_seconds() % 60);
                    let mills_left = format!("{:03}", remaining.num_milliseconds() % 1000);
                    format!(
                        "{}: There are {:02}:{:02}:{:02}.{:03} secs left.",
                        title.bright_red(),
                        hours_left.bright_yellow(),
                        minutes_left.bright_yellow(),
                        secs_left.bright_yellow(),
                        mills_left.bright_yellow()
                    )
                }
                0 => {
                    /*  TODO: notify how many time need be controlled precision,not like this fixed sleep.
                    need fix it later.
                    not play any sound for now.*/
                    osx_terminal_notifier(title, "", None).await;
                    sleep(StdDuration::from_millis(500)).await;
                    format!("{}: Now is the time!", title)
                }
                i64::MIN..=-1_i64 => {
                    format!(
                        "{}: The datetime was {} seconds ago.",
                        title, -remaining_seconds
                    )
                }
            };

            // if run_count != 0 {
            //     // Move the cursor to the correct position
            //     stdout.execute(MoveTo(0, starting_row + i as u16)).unwrap();
            // } else {
            //     println!();
            // }

            // Print the message
            println!("{}", message);
            stdout.flush().unwrap();
        }

        // 更新行数
        last_line_count = target_datetimes.len();

        sleep(StdDuration::from_millis(50)).await;
    }
}

#[tokio::main]
async fn main() {
    let cli_args = CliArgs::parse();
    let file_path = cli_args.config_file;

    let config = CountDownConfig::try_new(file_path).unwrap();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let config_for_spawn = config.clone();
    let if_running_for_spawn = running.clone();

    let countdown_handle =
        tokio::spawn(async move { terminal_run(if_running_for_spawn, config_for_spawn).await });
    let mut config_for_reload = config.clone();
    let _reload_handle = tokio::spawn(async move {
        loop {
            sleep(StdDuration::from_secs(1)).await;
            let _ = config_for_reload.reload().await;
        }
    });
    while !countdown_handle.is_finished() {
        sleep(StdDuration::from_millis(10)).await;
    }
}
