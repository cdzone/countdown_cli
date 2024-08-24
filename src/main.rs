use chrono::{Local, NaiveDateTime};
use clap::Parser;
use colored::*;
use crossterm::cursor::MoveTo;
use crossterm::terminal::size;
use crossterm::ExecutableCommand;
use serde_derive::Deserialize;
use std::fs::File;
use std::io::Read;
use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::sleep;

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

#[derive(Debug, Deserialize)]
struct Countdown {
    title: String,
    datetime: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    countdown: Vec<Countdown>,
}

#[tokio::main]
async fn main() {
    let cli_args = CliArgs::parse();
    let file_path = cli_args.config_file;

    let mut file = match File::open(file_path.clone()) {
        Ok(file) => file,
        Err(_) => {
            println!("Error: Cannot open file '{}'", file_path);
            return;
        }
    };

    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();

    let config: Config = match toml::from_str(&contents) {
        Ok(config) => config,
        Err(_) => {
            println!("Error: Invalid TOML file format");
            return;
        }
    };

    let mut target_datetimes: Vec<(String, NaiveDateTime)> = config
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

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut stdout = stdout();
    let mut run_count: usize = 0;

    while running.load(Ordering::SeqCst) {
        let _messages = String::new();

        // Get the terminal size
        let (_terminal_width, terminal_height) = size().unwrap();

        // Calculate the starting row for the messages
        let starting_row = if terminal_height > target_datetimes.len() as u16 {
            terminal_height - target_datetimes.len() as u16
        } else {
            0
        };

        for (i, (title, target_datetime)) in target_datetimes.iter().enumerate() {
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
                0 => format!("{}: Now is the time!", title),
                i64::MIN..=-1_i64 => {
                    format!(
                        "{}: The datetime was {} seconds ago.",
                        title, -remaining_seconds
                    )
                }
            };

            if run_count != 0 {
                // Move the cursor to the correct position
                stdout.execute(MoveTo(0, starting_row + i as u16)).unwrap();
            } else {
                println!();
            }

            // Print the message
            print!("{}", message);
            stdout.flush().unwrap();
        }

        run_count += 1;

        sleep(StdDuration::from_millis(50)).await;
    }
}
