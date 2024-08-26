use std::{io::Read, sync::Arc};

use serde_derive::Deserialize;
use tokio::sync::Mutex;
pub trait HotReload {
    async fn reload(&mut self) -> Result<(), Box<dyn std::error::Error>>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Countdown {
    pub title: String,
    pub datetime: String,
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
                println!("Error: Cannot open file '{}'", config_filename);
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
