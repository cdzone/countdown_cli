use chrono::{Local, NaiveDateTime};
use clap::Parser;
use colored::*;
use config::{CountDownConfig, HotReload};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, ExecutableCommand};
use notify::osx_terminal_notifier;
use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration as StdDuration, Duration, Instant};
use tokio::sync::Mutex;
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

#[derive(Debug, PartialEq)]
enum PomodoroState {
    Idle,
    Work,
    ShortBreak,
    LongBreak,
}

struct PomodoroTimer {
    start_time: Option<Instant>,
    work_duration: Duration,
    short_break_duration: Duration,
    long_break_duration: Duration,
    state: PomodoroState,
    completed_work_sessions: u32,
    long_break_interval: u32,
}

impl PomodoroTimer {
    fn new() -> Self {
        PomodoroTimer {
            start_time: None,
            work_duration: Duration::from_secs(25 * 60),
            short_break_duration: Duration::from_secs(5 * 60),
            long_break_duration: Duration::from_secs(15 * 60),
            state: PomodoroState::Idle,
            completed_work_sessions: 0,
            long_break_interval: 4,
        }
    }

    fn start(&mut self) {
        self.start_time = Some(Instant::now());
        if self.state == PomodoroState::Idle {
            self.state = PomodoroState::Work;
        }
    }

    fn stop(&mut self) {
        self.start_time = None;
        self.state = PomodoroState::Idle;
    }

    fn remaining_time(&self) -> Option<Duration> {
        self.start_time.map(|start| {
            let elapsed = start.elapsed();
            let duration = match self.state {
                PomodoroState::Work => self.work_duration,
                PomodoroState::ShortBreak => self.short_break_duration,
                PomodoroState::LongBreak => self.long_break_duration,
                PomodoroState::Idle => return Duration::from_secs(0),
            };
            if elapsed >= duration {
                Duration::from_secs(0)
            } else {
                duration - elapsed
            }
        })
    }

    fn next_state(&mut self) {
        match self.state {
            PomodoroState::Work => {
                self.completed_work_sessions += 1;
                if self.completed_work_sessions % self.long_break_interval == 0 {
                    self.state = PomodoroState::LongBreak;
                } else {
                    self.state = PomodoroState::ShortBreak;
                }
            }
            PomodoroState::ShortBreak | PomodoroState::LongBreak => {
                self.state = PomodoroState::Work;
            }
            PomodoroState::Idle => {}
        }
        self.start_time = Some(Instant::now());
    }

    fn set_work_duration(&mut self, minutes: u64) {
        self.work_duration = Duration::from_secs(minutes * 60);
    }

    fn set_short_break_duration(&mut self, minutes: u64) {
        self.short_break_duration = Duration::from_secs(minutes * 60);
    }

    fn set_long_break_duration(&mut self, minutes: u64) {
        self.long_break_duration = Duration::from_secs(minutes * 60);
    }

    fn set_long_break_interval(&mut self, interval: u32) {
        self.long_break_interval = interval;
    }
}

pub async fn terminal_run(if_running: Arc<AtomicBool>, config: CountDownConfig) {
    let mut stdout = stdout();
    let mut last_line_count = 0;
    let pomodoro = Arc::new(Mutex::new(PomodoroTimer::new()));

    let (tx, rx) = std_mpsc::channel();

    let if_running_clone = if_running.clone();
    thread::spawn(move || {
        handle_user_input(tx, if_running_clone);
    });

    let mut paused = false;

    while if_running.load(Ordering::SeqCst) {
        if let Ok(command) = rx.try_recv() {
            let mut pomodoro_lock = pomodoro.lock().await;
            match command
                .as_str()
                .split_whitespace()
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["start"] => pomodoro_lock.start(),
                ["stop"] => pomodoro_lock.stop(),
                ["next"] => pomodoro_lock.next_state(),
                ["work", duration] => {
                    if let Ok(minutes) = duration.parse() {
                        pomodoro_lock.set_work_duration(minutes);
                    }
                }
                ["short", duration] => {
                    if let Ok(minutes) = duration.parse() {
                        pomodoro_lock.set_short_break_duration(minutes);
                    }
                }
                ["long", duration] => {
                    if let Ok(minutes) = duration.parse() {
                        pomodoro_lock.set_long_break_duration(minutes);
                    }
                }
                ["interval", count] => {
                    if let Ok(interval) = count.parse() {
                        pomodoro_lock.set_long_break_interval(interval);
                    }
                }
                ["pause"] => paused = true,
                ["resume"] => paused = false,
                _ => println!("未知命令: {}", command),
            }
            drop(pomodoro_lock);
        }

        if paused {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

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

        let _ = stdout.execute(cursor::MoveToColumn(0));

        // 显示番茄钟状态
        let pomodoro_lock = pomodoro.lock().await;
        match pomodoro_lock.state {
            PomodoroState::Idle => println!("番茄钟未启动"),
            _ => {
                if let Some(remaining) = pomodoro_lock.remaining_time() {
                    println!(
                        "番茄钟状态: {:?}, 剩余时间: {:02}:{:02}",
                        pomodoro_lock.state,
                        remaining.as_secs() / 60,
                        remaining.as_secs() % 60
                    );
                    if remaining.as_secs() == 0 {
                        println!("当前阶段结束！");
                        drop(pomodoro_lock);
                        pomodoro.lock().await.next_state();
                        let pomodoro_lock = pomodoro.lock().await;
                        println!(
                            "已完成的工作周期: {}",
                            pomodoro_lock.completed_work_sessions
                        );
                        continue;
                    }
                }
            }
        }
        println!(
            "已完成的工作周期: {}",
            pomodoro_lock.completed_work_sessions
        );
        drop(pomodoro_lock);

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

            println!("{}", message);
            stdout.flush().unwrap();
        }

        last_line_count = target_datetimes.len() + 2; // +2 for the pomodoro timer lines

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn handle_user_input(tx: std_mpsc::Sender<String>, if_running: Arc<AtomicBool>) {
    println!("输入 'help' 查看可用命令");

    while if_running.load(Ordering::SeqCst) {
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let input = input.trim().to_string();
            if input == "help" {
                tx.send("pause".to_string()).unwrap();
                print_help();
                println!("按回车键继续...");
                let _ = std::io::stdin().read_line(&mut String::new());
                tx.send("resume".to_string()).unwrap();
            } else {
                tx.send(input).unwrap();
            }
        }
    }
}

fn print_help() {
    println!("可用命令：");
    println!("start - 启动番茄钟");
    println!("stop - 停止番茄钟");
    println!("next - 手动切换到下一个状态");
    println!("work <分钟> - 设置工作时间");
    println!("short <分钟> - 设置短休息时间");
    println!("long <分钟> - 设置长休息时间");
    println!("interval <次数> - 设置长休息间隔（工作周期次数）");
    println!("help - 显示此帮助信息");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = config_for_reload.reload().await;
        }
    });

    countdown_handle.await?;

    Ok(())
}
