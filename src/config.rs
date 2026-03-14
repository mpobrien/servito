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
    #[serde(default = "default_scan_concurrency")]
    pub scan_concurrency: usize,
}

fn default_scan_concurrency() -> usize {
    // CPU count is right for local storage; for NAS/Docker bind-mounts
    // you typically want much higher to hide I/O latency.
    std::thread::available_parallelism().map_or(8, |n| n.get()) * 4
}

pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("failed to parse config file: {}", path.display()))
}
