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

pub(crate) trait HotReload {
    async fn reload(&mut self) -> Result<(), Box<dyn std::error::Error>>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Countdown {
    #[serde(default)]
    pub id: Option<String>,
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
    last_mtime: Option<std::time::SystemTime>,
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
        let last_mtime = std::fs::metadata(&config_filename)
            .and_then(|m| m.modified())
            .ok();
        Ok(Self {
            data: Arc::new(Mutex::new(countdown_data)),
            config_filename,
            last_mtime,
        })
    }

    /// Reload only when the config file mtime changed (stat, not full re-read every tick).
    pub async fn reload_if_changed(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        let modified = std::fs::metadata(&self.config_filename)?.modified()?;
        if self.last_mtime == Some(modified) {
            return Ok(false);
        }
        self.reload().await?;
        self.last_mtime = Some(modified);
        Ok(true)
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::sync::{RwLock, RwLockReadGuard};

/// 统一的数据与mtime存储（供终端/热加载/API共享）
#[derive(Debug)]
pub struct CountdownStore {
    file_path: PathBuf,
    data: RwLock<CountDownData>,
    last_mtime: RwLock<Option<SystemTime>>,
    api_mode_enabled: bool,
    write_lock: tokio::sync::Mutex<()>,
}

impl CountdownStore {
    pub async fn new<P: Into<PathBuf>>(
        file_path: P,
        api_mode_enabled: bool,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let file_path = file_path.into();
        let (data, mtime) = Self::load_from_file(&file_path)?;
        let store = Arc::new(Self {
            file_path,
            data: RwLock::new(data),
            last_mtime: RwLock::new(mtime),
            api_mode_enabled,
            write_lock: tokio::sync::Mutex::new(()),
        });
        if api_mode_enabled {
            // 首次启动：补id并落盘（只加id行，保留注释排版）
            let _guard = store.write_lock.lock().await;
            store.ensure_ids_and_persist_if_needed_locked().await?;
        }
        Ok(store)
    }

    fn load_from_file(
        path: &Path,
    ) -> Result<(CountDownData, Option<SystemTime>), Box<dyn std::error::Error>> {
        let mut file = std::fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let countdown_data: CountDownData = toml::from_str(&contents)?;
        let last_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok((countdown_data, last_mtime))
    }

    pub async fn read_data(&self) -> RwLockReadGuard<'_, CountDownData> {
        self.data.read().await
    }

    pub async fn reload_if_changed(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let modified = std::fs::metadata(&self.file_path)?.modified()?;
        {
            let last = self.last_mtime.read().await;
            if *last == Some(modified) {
                return Ok(false);
            }
        }
        self.reload().await?;
        {
            let mut last = self.last_mtime.write().await;
            *last = Some(modified);
        }
        Ok(true)
    }

    /// 重新加载（加写锁），并在 API 模式下补 id 与落盘
    pub async fn reload(&self) -> Result<(), Box<dyn std::error::Error>> {
        let _guard = self.write_lock.lock().await;
        self.reload_locked().await
    }

    async fn reload_locked(&self) -> Result<(), Box<dyn std::error::Error>> {
        let (new_data, _) = Self::load_from_file(&self.file_path)?;
        {
            let mut data_lock = self.data.write().await;
            *data_lock = new_data;
        }
        if self.api_mode_enabled {
            self.ensure_ids_and_persist_if_needed_locked().await?;
        }
        Ok(())
    }

    /// 给文件里缺 id 的条目补 id：直接在 toml_edit 文档上补，
    /// 再从补好的文档解析内存数据，内存和文件不需要再做配对。
    async fn ensure_ids_and_persist_if_needed_locked(
        &self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let original = std::fs::read_to_string(&self.file_path)?;
        let mut doc = original.parse::<toml_edit::DocumentMut>()?;
        let mut updated = false;
        if let Some(arr) = doc
            .get_mut("countdown")
            .and_then(|it| it.as_array_of_tables_mut())
        {
            for table in arr.iter_mut() {
                if !table.contains_key("id") {
                    table.insert("id", toml_edit::value(uuid::Uuid::new_v4().to_string()));
                    updated = true;
                }
            }
        }
        if !updated {
            return Ok(());
        }
        let contents = doc.to_string();
        let new_data: CountDownData = toml::from_str(&contents)?;
        std::fs::write(self.file_path.with_extension("toml.bak"), &original)?;
        let tmp_path = self.unique_tmp_path();
        std::fs::write(&tmp_path, contents)?;
        std::fs::rename(&tmp_path, &self.file_path)?;
        *self.data.write().await = new_data;
        *self.last_mtime.write().await = std::fs::metadata(&self.file_path)?.modified().ok();
        Ok(())
    }

    fn unique_tmp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.file_path
            .with_extension(format!("toml.tmp.{}.{}", pid, seq))
    }

    // --- API-side mutation helpers ---
    pub async fn api_create(
        &self,
        title: String,
        datetime: String,
        enabled: bool,
        repeat: Option<RepeatMode>,
    ) -> Result<Countdown, Box<dyn std::error::Error>> {
        let _guard = self.write_lock.lock().await;
        self.sync_from_disk_if_stale_locked().await?;
        let id = uuid::Uuid::new_v4().to_string();
        // 编辑 toml 文档并落盘（保留注释）
        let original = std::fs::read_to_string(&self.file_path)?;
        let mut doc = original.parse::<toml_edit::DocumentMut>()?;
        // 构建新表
        let mut new_table = toml_edit::Table::new();
        new_table.insert("id", toml_edit::value(id.clone()));
        new_table.insert("title", toml_edit::value(title.clone()));
        new_table.insert("datetime", toml_edit::value(datetime.clone()));
        new_table.insert("enabled", toml_edit::value(enabled));
        if let Some(r) = &repeat {
            let s = match r {
                RepeatMode::Daily => "daily",
                RepeatMode::Weekday => "weekday",
            };
            new_table.insert("repeat", toml_edit::value(s));
        }
        new_table.set_implicit(true);
        if !doc.contains_key("countdown") {
            doc["countdown"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        doc["countdown"]
            .as_array_of_tables_mut()
            .unwrap()
            .push(new_table);
        let tmp = self.unique_tmp_path();
        std::fs::write(&tmp, doc.to_string())?;
        std::fs::rename(&tmp, &self.file_path)?;
        let created = Countdown {
            id: Some(id),
            title,
            datetime,
            enabled,
            repeat,
        };
        {
            let mut data = self.data.write().await;
            data.countdown.push(created.clone());
        }
        let mut last = self.last_mtime.write().await;
        *last = std::fs::metadata(&self.file_path)
            .and_then(|m| m.modified())
            .ok();
        Ok(created)
    }

    pub async fn api_update(
        &self,
        id: &str,
        title: String,
        datetime: String,
        enabled: bool,
        repeat: Option<RepeatMode>,
    ) -> Result<Countdown, CountdownUpdateError> {
        let _guard = self.write_lock.lock().await;
        self.sync_from_disk_if_stale_locked()
            .await
            .map_err(|e| CountdownUpdateError::Other(e.to_string()))?;
        let original = std::fs::read_to_string(&self.file_path)
            .map_err(|e| CountdownUpdateError::Other(e.to_string()))?;
        let mut doc = original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| CountdownUpdateError::Other(e.to_string()))?;
        // 找到带id的表并替换字段
        let mut found = false;
        if let Some(arr) = doc
            .get_mut("countdown")
            .and_then(|it| it.as_array_of_tables_mut())
        {
            for table in arr.iter_mut() {
                if table
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s == id)
                    .unwrap_or(false)
                {
                    found = true;
                    table.insert("title", toml_edit::value(title.clone()));
                    table.insert("datetime", toml_edit::value(datetime.clone()));
                    table.insert("enabled", toml_edit::value(enabled));
                    match &repeat {
                        None => {
                            table.remove("repeat");
                        }
                        Some(r) => {
                            let s = match r {
                                RepeatMode::Daily => "daily",
                                RepeatMode::Weekday => "weekday",
                            };
                            table.insert("repeat", toml_edit::value(s));
                        }
                    }
                    break;
                }
            }
        }
        if !found {
            return Err(CountdownUpdateError::NotFound);
        }
        let tmp = self.unique_tmp_path();
        std::fs::write(&tmp, doc.to_string())
            .map_err(|e| CountdownUpdateError::Other(e.to_string()))?;
        std::fs::rename(&tmp, &self.file_path)
            .map_err(|e| CountdownUpdateError::Other(e.to_string()))?;
        {
            let mut data = self.data.write().await;
            if let Some(existing) = data
                .countdown
                .iter_mut()
                .find(|c| c.id.as_deref() == Some(id))
            {
                existing.title = title.clone();
                existing.datetime = datetime.clone();
                existing.enabled = enabled;
                existing.repeat = repeat.clone();
            }
        }
        let mut last = self.last_mtime.write().await;
        *last = std::fs::metadata(&self.file_path)
            .and_then(|m| m.modified())
            .ok();
        let updated = {
            let data = self.data.read().await;
            data.countdown
                .iter()
                .find(|c| c.id.as_deref() == Some(id))
                .cloned()
                .unwrap()
        };
        Ok(updated)
    }

    pub async fn api_delete(&self, id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let _guard = self.write_lock.lock().await;
        self.sync_from_disk_if_stale_locked().await?;
        let original = std::fs::read_to_string(&self.file_path)?;
        let mut doc = original.parse::<toml_edit::DocumentMut>()?;
        let mut deleted = false;
        if let Some(arr) = doc
            .get_mut("countdown")
            .and_then(|it| it.as_array_of_tables_mut())
        {
            let len_before = arr.len();
            arr.retain(|table| {
                let is_target = table
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s == id)
                    .unwrap_or(false);
                if is_target {
                    deleted = true;
                    false
                } else {
                    true
                }
            });
            let _ = len_before;
        }
        if !deleted {
            return Ok(false);
        }
        let tmp = self.unique_tmp_path();
        std::fs::write(&tmp, doc.to_string())?;
        std::fs::rename(&tmp, &self.file_path)?;
        {
            let mut data = self.data.write().await;
            data.countdown.retain(|c| c.id.as_deref() != Some(id));
        }
        let mut last = self.last_mtime.write().await;
        *last = std::fs::metadata(&self.file_path)
            .and_then(|m| m.modified())
            .ok();
        Ok(true)
    }

    async fn sync_from_disk_if_stale_locked(&self) -> Result<(), Box<dyn std::error::Error>> {
        let disk_mtime = std::fs::metadata(&self.file_path)?.modified()?;
        let last = self.last_mtime.read().await;
        if *last != Some(disk_mtime) {
            drop(last);
            // 已在写锁内调用
            self.reload_locked().await?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CountdownUpdateError {
    NotFound,
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn id_injection_matches_api_ids_for_duplicate_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(
            &path,
            r#"[[countdown]]
id = "x1"
title = "A"
datetime = "2030-01-01 00:00:00"

[[countdown]]
title = "A"
datetime = "2030-01-01 00:00:00"
"#,
        )
        .unwrap();

        let store = CountdownStore::new(&path, true).await.unwrap();
        let in_memory: Vec<String> = store
            .read_data()
            .await
            .countdown
            .iter()
            .map(|c| c.id.clone().unwrap())
            .collect();
        let on_disk: CountDownData =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let on_disk: Vec<String> = on_disk
            .countdown
            .iter()
            .map(|c| c.id.clone().unwrap())
            .collect();

        assert_eq!(in_memory, on_disk);
        assert_eq!(on_disk[0], "x1");
        assert_ne!(on_disk[0], on_disk[1]);
    }

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
