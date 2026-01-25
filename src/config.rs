use chrono::{Datelike, NaiveDateTime, Weekday};
use serde::{de, Deserializer};
use serde_derive::Deserialize;
use std::{fmt, io::Read, sync::Arc};
use tokio::sync::Mutex;

/// Repeat mode for recurring countdowns
#[derive(Debug, Clone, PartialEq)]
pub enum RepeatMode {
    /// Repeats every day
    Daily,
    /// Repeats every weekday (Monday to Friday)
    Weekday,
}

impl fmt::Display for RepeatMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepeatMode::Daily => write!(f, "daily"),
            RepeatMode::Weekday => write!(f, "weekday"),
        }
    }
}

impl<'de> de::Deserialize<'de> for RepeatMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "daily" => Ok(RepeatMode::Daily),
            "weekday" | "workday" => Ok(RepeatMode::Weekday),
            _ => Err(de::Error::custom(format!(
                "Invalid repeat mode: '{}'. Expected 'daily', 'weekday', or 'workday'.",
                s
            ))),
        }
    }
}

pub trait HotReload {
    async fn reload(&mut self) -> Result<(), Box<dyn std::error::Error>>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Countdown {
    pub title: String,
    pub datetime: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub repeat: Option<RepeatMode>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct CountDownData {
    pub countdown: Vec<Countdown>,
}

#[derive(Debug, Clone)]
pub struct CountDownConfig {
    pub data: Arc<Mutex<CountDownData>>,
    config_filename: String,
}

impl CountDownConfig {
    pub fn try_new(config_filename: String) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = match std::fs::File::open(config_filename.clone()) {
            Ok(file) => file,
            Err(err) => {
                println!("Error: Cannot open file '{config_filename}'");
                return Err(err.into());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let countdown_data: CountDownData = toml::from_str(&contents)?;
        Ok(Self {
            data: Arc::new(Mutex::new(countdown_data)),
            config_filename,
        })
    }

    pub async fn set_config(&mut self, data: CountDownData) {
        let mut data_config = self.data.lock().await;
        data_config.countdown = data.countdown;
    }

    pub async fn get_config(&self) -> CountDownData {
        let data_config = self.data.lock().await;
        CountDownData {
            countdown: data_config.countdown.clone(),
        }
    }
}

impl HotReload for CountDownConfig {
    async fn reload(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut file = std::fs::File::open(self.config_filename.clone())?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let countdown_data = toml::from_str(&contents)?;
        self.set_config(countdown_data).await;
        Ok(())
    }
}

/// Calculate the target datetime for a recurring countdown based on current time.
///
/// For repeat modes:
/// - `Daily`: Returns today's target time. At midnight (00:00), switches to today's target.
/// - `Weekday`: Returns the appropriate target time based on current day:
///   - On weekdays before target time: returns today's target
///   - On weekdays after target time: returns today's target (for elapsed display)
///   - On weekends: returns last Friday's target (for elapsed display)
///   - On Monday 00:00: switches to Monday's target
///
/// Returns `None` if the datetime string cannot be parsed.
pub fn calculate_target_time(
    datetime_str: &str,
    repeat: Option<&RepeatMode>,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let original_datetime =
        NaiveDateTime::parse_from_str(datetime_str, "%Y-%m-%d %H:%M:%S").ok()?;

    match repeat {
        None => Some(original_datetime),
        Some(RepeatMode::Daily) => {
            let target_time = original_datetime.time();
            Some(now.date().and_time(target_time))
        }
        Some(RepeatMode::Weekday) => {
            let target_time = original_datetime.time();
            let weekday = now.weekday();

            match weekday {
                Weekday::Sat => {
                    // Saturday: return Friday's target time
                    let friday = now.date() - chrono::Duration::days(1);
                    Some(friday.and_time(target_time))
                }
                Weekday::Sun => {
                    // Sunday: return Friday's target time
                    let friday = now.date() - chrono::Duration::days(2);
                    Some(friday.and_time(target_time))
                }
                _ => {
                    // Weekday: return today's target time
                    Some(now.date().and_time(target_time))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repeat_mode_deserialize_daily() {
        let toml_str = r#"
            [[countdown]]
            title = "test"
            datetime = "2024-01-01 18:00:00"
            repeat = "daily"
        "#;
        let data: CountDownData = toml::from_str(toml_str).unwrap();
        assert_eq!(data.countdown[0].repeat, Some(RepeatMode::Daily));
    }

    #[test]
    fn test_repeat_mode_deserialize_weekday() {
        let toml_str = r#"
            [[countdown]]
            title = "test"
            datetime = "2024-01-01 18:00:00"
            repeat = "weekday"
        "#;
        let data: CountDownData = toml::from_str(toml_str).unwrap();
        assert_eq!(data.countdown[0].repeat, Some(RepeatMode::Weekday));
    }

    #[test]
    fn test_repeat_mode_deserialize_workday_alias() {
        let toml_str = r#"
            [[countdown]]
            title = "test"
            datetime = "2024-01-01 18:00:00"
            repeat = "workday"
        "#;
        let data: CountDownData = toml::from_str(toml_str).unwrap();
        assert_eq!(data.countdown[0].repeat, Some(RepeatMode::Weekday));
    }

    #[test]
    fn test_repeat_mode_deserialize_none() {
        let toml_str = r#"
            [[countdown]]
            title = "test"
            datetime = "2024-01-01 18:00:00"
        "#;
        let data: CountDownData = toml::from_str(toml_str).unwrap();
        assert_eq!(data.countdown[0].repeat, None);
    }

    #[test]
    fn test_repeat_mode_display() {
        assert_eq!(format!("{}", RepeatMode::Daily), "daily");
        assert_eq!(format!("{}", RepeatMode::Weekday), "weekday");
    }

    #[test]
    fn test_calculate_target_time_no_repeat() {
        let datetime_str = "2024-08-30 18:00:00";
        let now =
            NaiveDateTime::parse_from_str("2025-01-15 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, None, now);
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2024-08-30 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_daily_mode_before_target() {
        let datetime_str = "2024-08-30 18:00:00";
        // Wednesday 10:00
        let now =
            NaiveDateTime::parse_from_str("2025-01-15 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Daily), now);
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-15 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_daily_mode_after_target() {
        let datetime_str = "2024-08-30 18:00:00";
        // Wednesday 19:00 (after target)
        let now =
            NaiveDateTime::parse_from_str("2025-01-15 19:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Daily), now);
        // Should return today's target for elapsed display
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-15 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_daily_mode_at_midnight() {
        let datetime_str = "2024-08-30 18:00:00";
        // Thursday 00:00 (midnight)
        let now =
            NaiveDateTime::parse_from_str("2025-01-16 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Daily), now);
        // Should return today's target
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-16 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_daily_mode_year_boundary() {
        let datetime_str = "2024-08-30 23:59:00";
        // Jan 1st 00:00 (after crossing year boundary)
        let now =
            NaiveDateTime::parse_from_str("2025-01-01 00:00:01", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Daily), now);
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-01 23:59:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_wednesday_before_target() {
        let datetime_str = "2024-08-30 18:00:00";
        // Wednesday 10:00 (2025-01-15 is Wednesday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-15 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-15 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_monday_after_target() {
        let datetime_str = "2024-08-30 18:00:00";
        // Monday 19:00 (2025-01-13 is Monday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-13 19:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        // Should return today's target for elapsed display
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-13 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_friday_after_target() {
        let datetime_str = "2024-08-30 18:00:00";
        // Friday 19:00 (2025-01-17 is Friday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-17 19:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        // Should return Friday's target for elapsed display
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-17 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_saturday() {
        let datetime_str = "2024-08-30 18:00:00";
        // Saturday 10:00 (2025-01-18 is Saturday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-18 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        // Should return Friday's target for elapsed display
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-17 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_sunday() {
        let datetime_str = "2024-08-30 18:00:00";
        // Sunday 10:00 (2025-01-19 is Sunday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-19 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        // Should return Friday's target for elapsed display
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-17 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_weekday_mode_monday_midnight() {
        let datetime_str = "2024-08-30 18:00:00";
        // Monday 00:00 (2025-01-20 is Monday)
        let now =
            NaiveDateTime::parse_from_str("2025-01-20 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Weekday), now);
        // Should return Monday's target
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-20 18:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }

    #[test]
    fn test_daily_mode_midnight_target() {
        let datetime_str = "2024-08-30 00:00:00";
        // 23:59:50 - before midnight target
        let now =
            NaiveDateTime::parse_from_str("2025-01-15 23:59:50", "%Y-%m-%d %H:%M:%S").unwrap();
        let result = calculate_target_time(datetime_str, Some(&RepeatMode::Daily), now);
        // Should return today's midnight (which is in the past for today)
        assert_eq!(
            result,
            Some(
                NaiveDateTime::parse_from_str("2025-01-15 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
            )
        );
    }
}
