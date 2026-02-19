use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::Client;
use rusqlite::Connection;
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::error;

use crate::core::config;

pub const ADMIN_SESSION_COOKIE: &str = "prober_session";
pub const ADMIN_SESSION_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone)]
pub struct AppState {
    pub cli: config::Cli,
    pub static_config: Arc<config::Config>,
    pub hot_config: Arc<RwLock<config::HotConfig>>,
    pub db: Arc<Mutex<Connection>>,
    pub report_limiter: Arc<Mutex<HashMap<String, TokenBucket>>>,
    pub login_limiter: Arc<Mutex<HashMap<String, TokenBucket>>>,
    pub sessions: Arc<Mutex<HashMap<String, AdminSession>>>,
    pub events_tx: broadcast::Sender<String>,
    pub command_channels: Arc<Mutex<HashMap<String, broadcast::Sender<String>>>>,
    pub agent_command_connections: Arc<Mutex<HashMap<String, usize>>>,
    pub browser_subscribers: Arc<AtomicUsize>,
    pub desired_mode: Arc<RwLock<DesiredMode>>,
    pub alert_state: Arc<Mutex<HashMap<String, AgentAlertState>>>,
    pub last_global_alert_sent_at: Arc<Mutex<Option<DateTime<Utc>>>>,
    pub http_client: Client,
}

impl AppState {
    pub fn new(
        cli: config::Cli,
        loaded: config::Config,
        conn: Connection,
        events_tx: broadcast::Sender<String>,
        http_client: Client,
    ) -> Self {
        Self {
            cli,
            hot_config: Arc::new(RwLock::new(loaded.hot.clone())),
            static_config: Arc::new(loaded.clone()),
            db: Arc::new(Mutex::new(conn)),
            report_limiter: Arc::new(Mutex::new(HashMap::new())),
            login_limiter: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            command_channels: Arc::new(Mutex::new(HashMap::new())),
            agent_command_connections: Arc::new(Mutex::new(HashMap::new())),
            browser_subscribers: Arc::new(AtomicUsize::new(0)),
            desired_mode: Arc::new(RwLock::new(DesiredMode {
                kind: ModeKind::Heartbeat,
                realtime_interval_seconds: loaded.hot.realtime_interval_seconds,
                heartbeat_interval_seconds: loaded.heartbeat_interval_seconds,
            })),
            alert_state: Arc::new(Mutex::new(HashMap::new())),
            last_global_alert_sent_at: Arc::new(Mutex::new(None)),
            http_client,
        }
    }
}

#[derive(Debug)]
pub struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(initial_tokens: f64) -> Self {
        Self {
            tokens: initial_tokens,
            last_refill: Instant::now(),
        }
    }

    pub fn allow(&mut self, refill_rate_per_sec: f64, burst: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * refill_rate_per_sec).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdminSession {
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeKind {
    Heartbeat,
    Realtime,
}

impl ModeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Heartbeat => "heartbeat",
            Self::Realtime => "realtime",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DesiredMode {
    pub kind: ModeKind,
    pub realtime_interval_seconds: u64,
    pub heartbeat_interval_seconds: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AgentAlertState {
    pub was_offline: bool,
    pub offline_alert_sent: bool,
    pub last_offline_alert_at: Option<DateTime<Utc>>,
}

pub struct BrowserSubscriberGuard {
    state: AppState,
}

impl BrowserSubscriberGuard {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl Drop for BrowserSubscriberGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let prev = state.browser_subscribers.fetch_sub(1, Ordering::SeqCst);
            if prev <= 1 {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if state.browser_subscribers.load(Ordering::SeqCst) == 0
                    && let Err(err) =
                        crate::core::runtime::set_desired_mode(&state, ModeKind::Heartbeat).await
                {
                    error!(error = %err, "switch to heartbeat mode failed");
                }
            }
        });
    }
}

pub struct AgentCommandConnectionGuard {
    state: AppState,
    agent_id: String,
}

impl AgentCommandConnectionGuard {
    pub async fn new(state: AppState, agent_id: String) -> Self {
        let mut map = state.agent_command_connections.lock().await;
        let entry = map.entry(agent_id.clone()).or_insert(0);
        *entry = entry.saturating_add(1);
        drop(map);
        Self { state, agent_id }
    }
}

impl Drop for AgentCommandConnectionGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let mut map = state.agent_command_connections.lock().await;
            if let Some(count) = map.get_mut(&agent_id) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    map.remove(&agent_id);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::TokenBucket;

    #[test]
    fn token_bucket_respects_burst_limit() {
        let mut bucket = TokenBucket::new(3.0);
        assert!(bucket.allow(1.0, 3.0));
        assert!(bucket.allow(1.0, 3.0));
        assert!(bucket.allow(1.0, 3.0));
        assert!(!bucket.allow(1.0, 3.0));
    }
}
