use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Config {
    pub db: PathBuf,
    pub stream: StreamConfig,
    pub library: LibraryConfig,
}

#[derive(Deserialize)]
pub struct StreamConfig {
    pub port: u16,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: u64,
}

fn default_log_interval() -> u64 { 10 }

#[derive(Deserialize)]
pub struct LibraryConfig {
    pub paths: Vec<String>,
}

pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("failed to parse config file: {}", path.display()))
}
