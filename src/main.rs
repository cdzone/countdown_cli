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
    #[arg(short = 's', long = "notify_sound", default_value = "")]
    notify_sound: Option<String>,
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
    last_completed_time: Option<Instant>,
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
            last_completed_time: None,
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
                self.last_completed_time = Some(Instant::now());
            }
            PomodoroState::ShortBreak | PomodoroState::LongBreak => {
                self.last_completed_time = Some(Instant::now());
            }
            PomodoroState::Idle => {}
        }
        self.state = PomodoroState::Idle;
        self.start_time = None;
    }

    fn set_state(&mut self, new_state: PomodoroState) {
        self.state = new_state;
        self.start_time = Some(Instant::now());
        self.last_completed_time = None; // 清除上次完成时间
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

    fn time_since_last_completion(&self) -> Option<Duration> {
        self.last_completed_time.map(|time| time.elapsed())
    }
}

#[allow(unused_assignments)]
pub async fn terminal_run(
    if_running: Arc<AtomicBool>,
    config: CountDownConfig,
    notify_sound: Option<String>,
) {
    let mut stdout = stdout();
    let mut last_line_count = 0;
    let pomodoro = Arc::new(Mutex::new(PomodoroTimer::new()));

    let (tx, rx) = std_mpsc::channel();

    let if_running_clone = if_running.clone();
    thread::spawn(move || {
        handle_user_input(tx, if_running_clone);
    });

    let mut paused = false;
    let mut clean_without_output = false;

    while if_running.load(Ordering::SeqCst) {
        if let Ok(command) = rx.try_recv() {
            let mut pomodoro_lock = pomodoro.lock().await;
            match command
                .as_str()
                .split_whitespace()
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["start"] => pomodoro_lock.set_state(PomodoroState::Work),
                ["stop"] => pomodoro_lock.stop(),
                ["short"] => pomodoro_lock.set_state(PomodoroState::ShortBreak),
                ["long"] => pomodoro_lock.set_state(PomodoroState::LongBreak),
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

        let mut target_datetimes: Vec<(String, NaiveDateTime)> = config
            .get_config()
            .await
            .countdown
            .into_iter()
            .filter(|countdown| countdown.enabled)
            .filter_map(|countdown| {
                match NaiveDateTime::parse_from_str(&countdown.datetime, "%Y-%m-%d %H:%M:%S") {
                    Ok(datetime) => Some((countdown.title.clone(), datetime)),
                    Err(_) => {
                        println!(
                            "错误：'{}' 的日期时间格式无效。请使用 'YYYY-MM-DD HH:MM:SS' 格式。",
                            countdown.title
                        );
                        None
                    }
                }
            })
            .collect();

        target_datetimes.sort_by(|a, b| a.1.cmp(&b.1));

        // 清除之前的输出
        if !clean_without_output {
            for _ in 0..last_line_count {
                let _ = stdout.execute(cursor::MoveUp(1));
                let _ = stdout.execute(Clear(ClearType::CurrentLine));
            }
        }

        let _ = stdout.execute(cursor::MoveToColumn(0));

        let mut current_line_count = 0;

        // 显示番茄钟状态
        let pomodoro_lock = pomodoro.lock().await;
        match pomodoro_lock.state {
            PomodoroState::Idle => {
                if let Some(time_since_completion) = pomodoro_lock.time_since_last_completion() {
                    println!(
                        "番茄钟未启动，上次完成后已经过去: {:02}:{:02}",
                        time_since_completion.as_secs() / 60,
                        time_since_completion.as_secs() % 60
                    );
                    current_line_count += 1;
                } else {
                    println!("番茄钟未启动");
                    current_line_count += 1;
                }
            }
            _ => {
                if let Some(remaining) = pomodoro_lock.remaining_time() {
                    println!(
                        "番茄钟状态: {:?}, 剩余时间: {:02}:{:02}",
                        pomodoro_lock.state,
                        remaining.as_secs() / 60,
                        remaining.as_secs() % 60
                    );
                    current_line_count += 1;
                    if remaining.as_secs() == 0 {
                        println!("当前阶段结束！");
                        drop(pomodoro_lock);
                        pomodoro.lock().await.next_state();
                        osx_terminal_notifier("番茄钟：当前阶段结束！", "", notify_sound.clone())
                            .await;
                        let pomodoro_lock = pomodoro.lock().await;
                        println!(
                            "已完成的工作周期: {}",
                            pomodoro_lock.completed_work_sessions
                        );
                        current_line_count += 1;
                        println!("请输入下一个命令（start/short/long）来开始新的阶段");
                        current_line_count += 1;
                        clean_without_output = true;
                        continue;
                    }
                }
            }
        }
        println!(
            "已完成的工作周期: {}",
            pomodoro_lock.completed_work_sessions
        );
        current_line_count += 1;
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
                    osx_terminal_notifier(title, "", notify_sound.clone()).await;
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
            current_line_count += 1;
            stdout.flush().unwrap();
        }

        clean_without_output = false;
        last_line_count = current_line_count;

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
    println!("start - 开始工作阶段");
    println!("short - 开始短休息阶段");
    println!("long - 开始长休息阶段");
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
    let notify_sound = cli_args.notify_sound;

    let config = CountDownConfig::try_new(file_path).unwrap();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let config_for_spawn = config.clone();
    let if_running_for_spawn = running.clone();
    let notify_sound_for_spawn = notify_sound.clone();
    let countdown_handle = tokio::spawn(async move {
        terminal_run(
            if_running_for_spawn,
            config_for_spawn,
            notify_sound_for_spawn,
        )
        .await
    });
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
