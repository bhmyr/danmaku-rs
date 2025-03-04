use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use std::fs;
use crate::storage::StorageConfig;
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    #[serde(rename = "ControlPort")]
    pub control_port: String,
    #[serde(rename = "LogLevel")]
    pub log_level: String,
    #[serde(rename = "LiverFile")]
    pub liver_file: String,
    #[serde(rename = "AuthFile")]
    pub auth_file: String,
    #[serde(
        rename = "LiveInterval",
        deserialize_with = "deserialize_duration_minutes"
    )]
    pub live_interval: Duration,
    #[serde(rename = "StorageConfig")]
    pub storage_config: StorageConfig,

    #[serde(rename = "OfflineMsg")]
    pub offline_msg: HashSet<String>,

    #[serde(rename = "msg")]
    pub message_configs: HashMap<String, HashMap<String, MessageTypeConfig>>,
}

#[derive(Debug, Deserialize)]
pub struct MessageTypeConfig {
    #[serde(default)]
    pub name: Option<String>,
    pub keys: Vec<String>,
    pub values: Vec<String>,
}

fn deserialize_duration_minutes<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let minutes = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(minutes * 60))
}

pub static CONFIG: OnceLock<AppConfig> = OnceLock::new();

pub fn init() -> &'static AppConfig {
    CONFIG.get_or_init(|| {
        // 读取配置文件
        let config_str = fs::read_to_string("config.toml")
            .unwrap_or_else(|e| panic!("无法读取 config.toml: {}", e));

        toml::from_str(&config_str).unwrap_or_else(|e| panic!("TOML 解析错误: {}", e))
    })
}
