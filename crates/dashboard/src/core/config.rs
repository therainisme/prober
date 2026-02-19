use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
use clap::Parser;
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "./config/dashboard.toml";

#[derive(Debug, Clone, Parser)]
#[command(name = "prober-dashboard", about = "Prober dashboard service")]
pub struct Cli {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    #[arg(long)]
    pub token: Option<String>,
    #[arg(long)]
    pub admin_password: Option<String>,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub telegram_bot_token: Option<String>,
    #[arg(long)]
    pub telegram_chat_id: Option<String>,
    #[arg(long)]
    pub offline_threshold_seconds: Option<u64>,
    #[arg(long)]
    pub alert_quiet_window_minutes: Option<u64>,
    #[arg(long)]
    pub log_level: Option<String>,
    #[arg(long)]
    pub sqlite_busy_timeout_ms: Option<u64>,
    #[arg(long)]
    pub sqlite_wal_autocheckpoint: Option<u32>,
    #[arg(long)]
    pub realtime_interval_seconds: Option<u64>,
    #[arg(long)]
    pub heartbeat_interval_seconds: Option<u64>,
    #[arg(long)]
    pub command_expire_seconds: Option<u64>,
    #[arg(long)]
    pub soft_delete_retention_days: Option<u64>,
    #[arg(long)]
    pub raw_retention_days: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: PathBuf,
    pub listen: SocketAddr,
    pub agent_token: String,
    pub admin_password_hash: String,
    pub data_dir: PathBuf,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub hot: HotConfig,
    pub sqlite_busy_timeout_ms: u64,
    pub sqlite_wal_autocheckpoint: u32,
    pub heartbeat_interval_seconds: u64,
    pub command_expire_seconds: u64,
    pub soft_delete_retention_days: u64,
    pub raw_retention_days: u64,
}

#[derive(Debug, Clone)]
pub struct HotConfig {
    pub offline_threshold_seconds: u64,
    pub alert_quiet_window_minutes: u64,
    pub log_level: String,
    pub realtime_interval_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FileConfig {
    listen: Option<SocketAddr>,
    token: Option<String>,
    admin_password: Option<String>,
    data_dir: Option<PathBuf>,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
    offline_threshold_seconds: Option<u64>,
    alert_quiet_window_minutes: Option<u64>,
    log_level: Option<String>,
    sqlite_busy_timeout_ms: Option<u64>,
    sqlite_wal_autocheckpoint: Option<u32>,
    realtime_interval_seconds: Option<u64>,
    heartbeat_interval_seconds: Option<u64>,
    command_expire_seconds: Option<u64>,
    soft_delete_retention_days: Option<u64>,
    raw_retention_days: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            listen: "0.0.0.0:8080".parse().expect("default socket addr"),
            agent_token: "change-me-token".to_string(),
            admin_password_hash: hash_password("admin").expect("hash default password"),
            data_dir: PathBuf::from("./data"),
            telegram_bot_token: None,
            telegram_chat_id: None,
            hot: HotConfig {
                offline_threshold_seconds: 7200,
                alert_quiet_window_minutes: 0,
                log_level: "info".to_string(),
                realtime_interval_seconds: 3,
            },
            sqlite_busy_timeout_ms: 5000,
            sqlite_wal_autocheckpoint: 1000,
            heartbeat_interval_seconds: 3600,
            command_expire_seconds: 600,
            soft_delete_retention_days: 7,
            raw_retention_days: 30,
        }
    }
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self> {
        let mut config = Self::default();
        let mut should_persist_admin_hash = false;
        let config_path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        config.config_path = config_path.clone();

        if config_path.exists() {
            apply_file(&mut config, &config_path, &mut should_persist_admin_hash)?;
        }

        apply_env(&mut config, &mut should_persist_admin_hash)?;
        apply_cli(&mut config, cli, &mut should_persist_admin_hash)?;

        if config.agent_token.trim().is_empty() {
            bail!("token cannot be empty");
        }
        if config.hot.realtime_interval_seconds == 0 {
            bail!("realtime_interval_seconds must be >= 1");
        }
        if config.hot.realtime_interval_seconds > 60 {
            bail!("realtime_interval_seconds must be <= 60");
        }
        if config.heartbeat_interval_seconds == 0 {
            bail!("heartbeat_interval_seconds must be >= 1");
        }
        if config.command_expire_seconds == 0 {
            bail!("command_expire_seconds must be >= 1");
        }
        if config.soft_delete_retention_days == 0 {
            bail!("soft_delete_retention_days must be >= 1");
        }
        if config.raw_retention_days == 0 {
            bail!("raw_retention_days must be >= 1");
        }

        fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("create data dir {}", config.data_dir.display()))?;
        if should_persist_admin_hash {
            persist_hashed_password(&config)?;
        }

        Ok(config)
    }

    pub fn redact_summary(&self) -> String {
        format!(
            "listen={}, data_dir={}, token={}, admin_hash={}, log_level={}, offline_threshold_seconds={}, realtime_interval_seconds={}, heartbeat_interval_seconds={}, command_expire_seconds={}, soft_delete_retention_days={}, raw_retention_days={}, sqlite_busy_timeout_ms={}, sqlite_wal_autocheckpoint={}",
            self.listen,
            self.data_dir.display(),
            redact_secret(&self.agent_token),
            redact_secret(&self.admin_password_hash),
            self.hot.log_level,
            self.hot.offline_threshold_seconds,
            self.hot.realtime_interval_seconds,
            self.heartbeat_interval_seconds,
            self.command_expire_seconds,
            self.soft_delete_retention_days,
            self.raw_retention_days,
            self.sqlite_busy_timeout_ms,
            self.sqlite_wal_autocheckpoint,
        )
    }
}

fn apply_file(
    config: &mut Config,
    path: &Path,
    should_persist_admin_hash: &mut bool,
) -> Result<()> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read config file {}", path.display()))?;
    let parsed: FileConfig =
        toml::from_str(&raw).with_context(|| format!("parse config file {}", path.display()))?;
    apply_file_like(config, parsed, should_persist_admin_hash)
}

fn apply_env(config: &mut Config, should_persist_admin_hash: &mut bool) -> Result<()> {
    let parsed = FileConfig {
        listen: get_env("PROBER_DASHBOARD_LISTEN").and_then(|v| v.parse().ok()),
        token: get_env("PROBER_DASHBOARD_TOKEN"),
        admin_password: get_env("PROBER_DASHBOARD_ADMIN_PASSWORD"),
        data_dir: get_env("PROBER_DASHBOARD_DATA_DIR").map(PathBuf::from),
        telegram_bot_token: get_env("PROBER_DASHBOARD_TELEGRAM_BOT_TOKEN"),
        telegram_chat_id: get_env("PROBER_DASHBOARD_TELEGRAM_CHAT_ID"),
        offline_threshold_seconds: get_env("PROBER_DASHBOARD_OFFLINE_THRESHOLD_SECONDS")
            .and_then(|v| v.parse().ok()),
        alert_quiet_window_minutes: get_env("PROBER_DASHBOARD_ALERT_QUIET_WINDOW_MINUTES")
            .and_then(|v| v.parse().ok()),
        log_level: get_env("PROBER_DASHBOARD_LOG_LEVEL"),
        sqlite_busy_timeout_ms: get_env("PROBER_DASHBOARD_SQLITE_BUSY_TIMEOUT_MS")
            .and_then(|v| v.parse().ok()),
        sqlite_wal_autocheckpoint: get_env("PROBER_DASHBOARD_SQLITE_WAL_AUTOCHECKPOINT")
            .and_then(|v| v.parse().ok()),
        realtime_interval_seconds: get_env("PROBER_DASHBOARD_REALTIME_INTERVAL_SECONDS")
            .and_then(|v| v.parse().ok()),
        heartbeat_interval_seconds: get_env("PROBER_DASHBOARD_HEARTBEAT_INTERVAL_SECONDS")
            .and_then(|v| v.parse().ok()),
        command_expire_seconds: get_env("PROBER_DASHBOARD_COMMAND_EXPIRE_SECONDS")
            .and_then(|v| v.parse().ok()),
        soft_delete_retention_days: get_env("PROBER_DASHBOARD_SOFT_DELETE_RETENTION_DAYS")
            .and_then(|v| v.parse().ok()),
        raw_retention_days: get_env("PROBER_DASHBOARD_RAW_RETENTION_DAYS")
            .and_then(|v| v.parse().ok()),
    };

    apply_file_like(config, parsed, should_persist_admin_hash)
}

fn apply_cli(config: &mut Config, cli: &Cli, should_persist_admin_hash: &mut bool) -> Result<()> {
    let parsed = FileConfig {
        listen: cli.listen,
        token: cli.token.clone(),
        admin_password: cli.admin_password.clone(),
        data_dir: cli.data_dir.clone(),
        telegram_bot_token: cli.telegram_bot_token.clone(),
        telegram_chat_id: cli.telegram_chat_id.clone(),
        offline_threshold_seconds: cli.offline_threshold_seconds,
        alert_quiet_window_minutes: cli.alert_quiet_window_minutes,
        log_level: cli.log_level.clone(),
        sqlite_busy_timeout_ms: cli.sqlite_busy_timeout_ms,
        sqlite_wal_autocheckpoint: cli.sqlite_wal_autocheckpoint,
        realtime_interval_seconds: cli.realtime_interval_seconds,
        heartbeat_interval_seconds: cli.heartbeat_interval_seconds,
        command_expire_seconds: cli.command_expire_seconds,
        soft_delete_retention_days: cli.soft_delete_retention_days,
        raw_retention_days: cli.raw_retention_days,
    };

    apply_file_like(config, parsed, should_persist_admin_hash)
}

fn apply_file_like(
    config: &mut Config,
    parsed: FileConfig,
    should_persist_admin_hash: &mut bool,
) -> Result<()> {
    if let Some(v) = parsed.listen {
        config.listen = v;
    }
    if let Some(v) = parsed.token {
        config.agent_token = v;
    }
    if let Some(v) = parsed.admin_password {
        let (hash, from_plain) = normalize_admin_password(&v)?;
        config.admin_password_hash = hash;
        if from_plain {
            *should_persist_admin_hash = true;
        }
    }
    if let Some(v) = parsed.data_dir {
        config.data_dir = v;
    }
    if let Some(v) = parsed.telegram_bot_token {
        config.telegram_bot_token = Some(v);
    }
    if let Some(v) = parsed.telegram_chat_id {
        config.telegram_chat_id = Some(v);
    }
    if let Some(v) = parsed.offline_threshold_seconds {
        config.hot.offline_threshold_seconds = v;
    }
    if let Some(v) = parsed.alert_quiet_window_minutes {
        config.hot.alert_quiet_window_minutes = v;
    }
    if let Some(v) = parsed.log_level {
        config.hot.log_level = v;
    }
    if let Some(v) = parsed.realtime_interval_seconds {
        config.hot.realtime_interval_seconds = v;
    }
    if let Some(v) = parsed.sqlite_busy_timeout_ms {
        config.sqlite_busy_timeout_ms = v;
    }
    if let Some(v) = parsed.sqlite_wal_autocheckpoint {
        config.sqlite_wal_autocheckpoint = v;
    }
    if let Some(v) = parsed.heartbeat_interval_seconds {
        config.heartbeat_interval_seconds = v;
    }
    if let Some(v) = parsed.command_expire_seconds {
        config.command_expire_seconds = v;
    }
    if let Some(v) = parsed.soft_delete_retention_days {
        config.soft_delete_retention_days = v;
    }
    if let Some(v) = parsed.raw_retention_days {
        config.raw_retention_days = v;
    }

    Ok(())
}

fn get_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn normalize_admin_password(raw: &str) -> Result<(String, bool)> {
    let trimmed = raw.trim();
    if trimmed.starts_with("$argon2id$") {
        return Ok((trimmed.to_string(), false));
    }
    if trimmed.starts_with("argon2id$") {
        return Ok((format!("${trimmed}"), false));
    }
    Ok((hash_password(trimmed)?, true))
}

fn hash_password(plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .context("hash admin password")?
        .to_string();
    Ok(hash)
}

fn redact_secret(raw: &str) -> String {
    if raw.is_empty() {
        return "<empty>".to_string();
    }
    let len = raw.chars().count();
    if len <= 8 {
        return "***".to_string();
    }
    let head: String = raw.chars().take(4).collect();
    let tail: String = raw
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}***{tail}")
}

fn persist_hashed_password(config: &Config) -> Result<()> {
    let path = &config.config_path;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config parent dir {}", parent.display()))?;
    }

    let mut table = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("read config file {}", path.display()))?;
        content
            .parse::<toml::Value>()
            .ok()
            .and_then(|v| v.as_table().cloned())
            .unwrap_or_default()
    } else {
        toml::map::Map::new()
    };

    table.insert(
        "admin_password".to_string(),
        toml::Value::String(config.admin_password_hash.clone()),
    );
    let output = toml::to_string_pretty(&toml::Value::Table(table))
        .context("serialize config with hashed admin password")?;
    fs::write(path, format!("{output}\n"))
        .with_context(|| format!("write config file {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_password_becomes_hash() {
        let (hash, plain) = normalize_admin_password("123456").expect("hash plain password");
        assert!(hash.starts_with("$argon2id$"));
        assert!(plain);
    }

    #[test]
    fn legacy_prefix_is_supported() {
        let (hash, plain) = normalize_admin_password("argon2id$v=19$m=19456,t=2,p=1$abc$def")
            .expect("normalize legacy prefix hash");
        assert!(hash.starts_with("$argon2id$"));
        assert!(!plain);
    }
}
