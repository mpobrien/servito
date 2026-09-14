use anyhow::Context;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Config {
    pub db: PathBuf,
    pub stream: StreamConfig,
    #[serde(default = "default_scan_concurrency")]
    pub scan_concurrency: usize,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Deserialize)]
pub struct StreamConfig {
    pub port: u16,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: u64,
}

fn default_log_interval() -> u64 { 10 }

/// A channel is a named, independently-scheduled stream backed by its own
/// subset of the library. `paths` uses the same file/dir/glob syntax as the
/// old top-level `library.paths` did; the same file may be listed by more
/// than one channel.
#[derive(Deserialize, Clone)]
pub struct ChannelConfig {
    pub name: String,
    pub paths: Vec<String>,
}

fn default_scan_concurrency() -> usize {
    // CPU count is right for local storage; for NAS/Docker bind-mounts
    // you typically want much higher to hide I/O latency.
    std::thread::available_parallelism().map_or(8, |n| n.get()) * 4
}

pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text)
        .with_context(|| format!("failed to parse config file: {}", path.display()))?;

    // A relative `db` is relative to the config file's own directory, not
    // the process's current directory — otherwise the same config resolves
    // to a different (likely empty) database depending on where you happen
    // to run `servito` from.
    if cfg.db.is_relative() {
        let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        cfg.db = base.join(&cfg.db);
    }

    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> anyhow::Result<()> {
    if cfg.channels.is_empty() {
        anyhow::bail!("config must define at least one [[channels]] entry");
    }
    let mut seen = HashSet::new();
    for ch in &cfg.channels {
        if ch.name.is_empty()
            || !ch.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!(
                "invalid channel name '{}': use only letters, digits, '-' and '_' (it appears in URLs)",
                ch.name
            );
        }
        if !seen.insert(ch.name.clone()) {
            anyhow::bail!("duplicate channel name '{}'", ch.name);
        }
        if ch.paths.is_empty() {
            anyhow::bail!("channel '{}' has no paths configured", ch.name);
        }
    }
    Ok(())
}
