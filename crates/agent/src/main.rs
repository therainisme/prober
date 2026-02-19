use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use chrono::{DateTime, Utc};
use clap::Parser;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::StreamExt;
use rand::Rng;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sysinfo::{Disk, Disks, Networks, System};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

const COMMAND_CACHE_LIMIT: usize = 1000;
const COMMAND_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Parser)]
#[command(name = "prober-agent", about = "Prober lightweight agent")]
struct Cli {
    #[arg(long, env = "PROBER_DASHBOARD_URL")]
    dashboard: String,
    #[arg(long, env = "PROBER_TOKEN")]
    token: String,
    #[arg(long, env = "PROBER_AGENT_ID")]
    id: String,
    #[arg(long, env = "PROBER_AGENT_NAME")]
    name: Option<String>,
    #[arg(long, env = "PROBER_REPORT_INTERVAL", default_value_t = 3)]
    interval_seconds: u64,
    #[arg(
        long,
        env = "PROBER_HEARTBEAT_INTERVAL_SECONDS",
        default_value_t = 3600
    )]
    heartbeat_interval_seconds: u64,
    #[arg(long, env = "PROBER_UPGRADE_PUBLIC_KEY")]
    upgrade_public_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterPayload {
    agent_id: String,
    display_name: Option<String>,
    agent_version: String,
}

#[derive(Debug, Serialize)]
struct ReportPayload {
    agent_id: String,
    boot_id: String,
    seq: u64,
    collected_at: DateTime<Utc>,
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CommandEnvelope {
    command_id: Uuid,
    #[serde(rename = "type")]
    command_type: String,
    issued_at: DateTime<Utc>,
    expire_at: DateTime<Utc>,
    payload: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct CommandAckPayload {
    agent_id: String,
    command_id: Uuid,
    status: String,
    detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentMode {
    Heartbeat,
    Realtime,
}

#[derive(Debug, Clone)]
struct ModeState {
    mode: AgentMode,
    realtime_interval_seconds: u64,
    heartbeat_interval_seconds: u64,
}

#[derive(Debug)]
struct CommandCache {
    order: VecDeque<(Uuid, DateTime<Utc>)>,
    seen: HashMap<Uuid, DateTime<Utc>>,
}

impl CommandCache {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            seen: HashMap::new(),
        }
    }

    fn contains(&self, id: &Uuid) -> bool {
        self.seen.contains_key(id)
    }

    fn insert(&mut self, id: Uuid) {
        let now = Utc::now();
        self.seen.insert(id, now);
        self.order.push_back((id, now));
        self.prune();
    }

    fn prune(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::seconds(COMMAND_CACHE_TTL_SECONDS);
        while let Some((id, ts)) = self.order.front().cloned() {
            let stale = ts < cutoff || self.order.len() > COMMAND_CACHE_LIMIT;
            if !stale {
                break;
            }
            self.order.pop_front();
            if let Some(current_ts) = self.seen.get(&id)
                && *current_ts == ts
            {
                self.seen.remove(&id);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    if cli.interval_seconds == 0 || cli.interval_seconds > 60 {
        anyhow::bail!("interval_seconds must be in [1, 60]");
    }
    if cli.heartbeat_interval_seconds == 0 {
        anyhow::bail!("heartbeat_interval_seconds must be >= 1");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build http client")?;

    let base = cli.dashboard.trim_end_matches('/').to_string();
    let register_url = format!("{base}/api/agent/register");
    let report_url = format!("{base}/api/agent/report");
    let commands_url = format!(
        "{base}/api/agent/commands?agent_id={}",
        urlencoding::encode(&cli.id)
    );
    let command_ack_url = format!("{base}/api/agent/command-ack");

    register(&client, &register_url, &cli).await?;

    let boot_id = Uuid::new_v4().to_string();
    let mode_state = std::sync::Arc::new(RwLock::new(ModeState {
        mode: AgentMode::Heartbeat,
        realtime_interval_seconds: cli.interval_seconds,
        heartbeat_interval_seconds: cli.heartbeat_interval_seconds,
    }));
    let command_cache = std::sync::Arc::new(tokio::sync::Mutex::new(CommandCache::new()));

    let command_task = {
        let client = client.clone();
        let cli = cli.clone();
        let mode_state = mode_state.clone();
        let cache = command_cache.clone();
        tokio::spawn(async move {
            command_loop(
                &client,
                &cli,
                &commands_url,
                &command_ack_url,
                mode_state,
                cache,
            )
            .await
        })
    };

    let report_task = {
        let client = client.clone();
        let cli = cli.clone();
        let mode_state = mode_state.clone();
        tokio::spawn(
            async move { report_loop(&client, &cli, &report_url, &boot_id, mode_state).await },
        )
    };

    let (report_result, command_result) = tokio::join!(report_task, command_task);

    if let Err(err) = report_result {
        return Err(anyhow::anyhow!("report task join failed: {err}"));
    }
    if let Err(err) = command_result {
        return Err(anyhow::anyhow!("command task join failed: {err}"));
    }

    Ok(())
}

async fn report_loop(
    client: &reqwest::Client,
    cli: &Cli,
    report_url: &str,
    boot_id: &str,
    mode_state: std::sync::Arc<RwLock<ModeState>>,
) -> Result<()> {
    let mut seq = 1_u64;
    let mut system = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    let mut disks = Disks::new_with_refreshed_list();
    let mut last_net_total: Option<(u64, u64, Instant)> = None;

    info!(agent_id = cli.id, "agent report loop started");

    loop {
        let report = collect_report(
            cli,
            boot_id,
            seq,
            &mut system,
            &mut networks,
            &mut disks,
            &mut last_net_total,
        );

        if let Err(err) = post_report(client, report_url, &cli.token, &report).await {
            warn!(error = %err, "post report failed");
        }

        seq = seq.saturating_add(1);

        let state = mode_state.read().await.clone();
        let interval = match state.mode {
            AgentMode::Heartbeat => state.heartbeat_interval_seconds,
            AgentMode::Realtime => state.realtime_interval_seconds,
        };
        tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
    }
}

async fn command_loop(
    client: &reqwest::Client,
    cli: &Cli,
    commands_url: &str,
    ack_url: &str,
    mode_state: std::sync::Arc<RwLock<ModeState>>,
    cache: std::sync::Arc<tokio::sync::Mutex<CommandCache>>,
) -> Result<()> {
    let mut backoff_secs = 1_u64;

    loop {
        let req = client
            .get(commands_url)
            .bearer_auth(&cli.token)
            .timeout(Duration::from_secs(0));

        let response = match req.send().await {
            Ok(resp) => resp,
            Err(err) => {
                warn!(error = %err, "command stream connect failed");
                let jitter = rand::rng().random_range(0..=500);
                tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter)).await;
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
        };

        if response.status() != StatusCode::OK {
            let code = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            warn!(status = %code, body, "command stream rejected");
            let jitter = rand::rng().random_range(0..=500);
            tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter)).await;
            backoff_secs = (backoff_secs * 2).min(60);
            continue;
        }

        info!("command stream connected");
        backoff_secs = 1;

        if let Err(err) =
            consume_sse_stream(response, client, cli, ack_url, &mode_state, &cache).await
        {
            warn!(error = %err, "command stream disconnected");
        }

        let jitter = rand::rng().random_range(0..=500);
        tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

async fn consume_sse_stream(
    response: reqwest::Response,
    client: &reqwest::Client,
    cli: &Cli,
    ack_url: &str,
    mode_state: &std::sync::Arc<RwLock<ModeState>>,
    cache: &std::sync::Arc<tokio::sync::Mutex<CommandCache>>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read command stream chunk")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        loop {
            let normalized = buffer.replace("\r\n", "\n");
            if let Some(pos) = normalized.find("\n\n") {
                let frame = normalized[..pos].to_string();
                let remaining = normalized[pos + 2..].to_string();
                buffer = remaining;
                handle_sse_frame(&frame, client, cli, ack_url, mode_state, cache).await?;
            } else {
                break;
            }
        }
    }

    anyhow::bail!("command stream closed by server")
}

async fn handle_sse_frame(
    frame: &str,
    client: &reqwest::Client,
    cli: &Cli,
    ack_url: &str,
    mode_state: &std::sync::Arc<RwLock<ModeState>>,
    cache: &std::sync::Arc<tokio::sync::Mutex<CommandCache>>,
) -> Result<()> {
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim());
        }
    }

    if data_lines.is_empty() {
        return Ok(());
    }

    let raw = data_lines.join("\n");
    let command: CommandEnvelope = match serde_json::from_str(&raw) {
        Ok(cmd) => cmd,
        Err(err) => {
            warn!(error = %err, payload = raw, "invalid command payload");
            return Ok(());
        }
    };

    let now = Utc::now();
    if command.issued_at > now + chrono::Duration::minutes(5) {
        post_command_ack(
            client,
            ack_url,
            &cli.token,
            &CommandAckPayload {
                agent_id: cli.id.clone(),
                command_id: command.command_id,
                status: "rejected".to_string(),
                detail: Some("issued_at is too far in the future".to_string()),
            },
        )
        .await?;
        return Ok(());
    }
    if now > command.expire_at {
        post_command_ack(
            client,
            ack_url,
            &cli.token,
            &CommandAckPayload {
                agent_id: cli.id.clone(),
                command_id: command.command_id,
                status: "expired".to_string(),
                detail: Some("command expired before execution".to_string()),
            },
        )
        .await?;
        return Ok(());
    }

    {
        let mut dedupe = cache.lock().await;
        dedupe.prune();
        if dedupe.contains(&command.command_id) {
            post_command_ack(
                client,
                ack_url,
                &cli.token,
                &CommandAckPayload {
                    agent_id: cli.id.clone(),
                    command_id: command.command_id,
                    status: "duplicated".to_string(),
                    detail: Some("command already processed".to_string()),
                },
            )
            .await?;
            return Ok(());
        }
        dedupe.insert(command.command_id);
    }

    let ack = execute_command(client, cli, mode_state, &command).await;
    post_command_ack(client, ack_url, &cli.token, &ack).await?;

    if ack.status == "ok" && command.command_type == "upgrade" {
        std::process::exit(0);
    }

    Ok(())
}

async fn execute_command(
    client: &reqwest::Client,
    cli: &Cli,
    mode_state: &std::sync::Arc<RwLock<ModeState>>,
    command: &CommandEnvelope,
) -> CommandAckPayload {
    match command.command_type.as_str() {
        "set_mode" => {
            let mode_raw = command
                .payload
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("heartbeat");
            let rt = command
                .payload
                .get("recommended_realtime_interval_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .clamp(1, 60);
            let hb = command
                .payload
                .get("heartbeat_interval_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(3600)
                .max(1);

            let mode = if mode_raw.eq_ignore_ascii_case("realtime") {
                AgentMode::Realtime
            } else {
                AgentMode::Heartbeat
            };

            {
                let mut guard = mode_state.write().await;
                guard.mode = mode;
                guard.realtime_interval_seconds = rt;
                guard.heartbeat_interval_seconds = hb;
            }

            CommandAckPayload {
                agent_id: cli.id.clone(),
                command_id: command.command_id,
                status: "ok".to_string(),
                detail: Some(format!(
                    "mode updated to {mode_raw}, realtime={rt}s heartbeat={hb}s"
                )),
            }
        }
        "upgrade" => match execute_upgrade(client, cli, command).await {
            Ok(detail) => CommandAckPayload {
                agent_id: cli.id.clone(),
                command_id: command.command_id,
                status: "ok".to_string(),
                detail: Some(detail),
            },
            Err(err) => CommandAckPayload {
                agent_id: cli.id.clone(),
                command_id: command.command_id,
                status: "failed".to_string(),
                detail: Some(err.to_string()),
            },
        },
        other => CommandAckPayload {
            agent_id: cli.id.clone(),
            command_id: command.command_id,
            status: "rejected".to_string(),
            detail: Some(format!("unsupported command type: {other}")),
        },
    }
}

async fn execute_upgrade(
    client: &reqwest::Client,
    cli: &Cli,
    command: &CommandEnvelope,
) -> Result<String> {
    let package_url = command
        .payload
        .get("package_url")
        .and_then(|v| v.as_str())
        .context("missing package_url")?;
    let expected_sha256 = command
        .payload
        .get("sha256")
        .and_then(|v| v.as_str())
        .context("missing sha256")?
        .to_lowercase();
    let signature_b64 = command
        .payload
        .get("signature")
        .and_then(|v| v.as_str())
        .context("missing signature")?;

    let metadata_hash = command
        .payload
        .get("metadata_hash")
        .and_then(|v| v.as_str())
        .map(|v| v.to_lowercase());
    let version = command
        .payload
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if let Some(expect_meta) = metadata_hash {
        let mut hasher = Sha256::new();
        hasher.update(
            format!("{package_url}|{expected_sha256}|{signature_b64}|{version}").as_bytes(),
        );
        let actual_meta = hex::encode(hasher.finalize());
        if actual_meta != expect_meta {
            anyhow::bail!("metadata hash mismatch");
        }
    }

    let response = client
        .get(package_url)
        .send()
        .await
        .context("download upgrade package failed")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "download upgrade package failed with status {}",
            response.status()
        );
    }

    let bytes = response
        .bytes()
        .await
        .context("read upgrade package bytes failed")?;

    let actual_sha = hex::encode(Sha256::digest(&bytes));
    if actual_sha != expected_sha256 {
        anyhow::bail!("sha256 mismatch");
    }

    let pubkey_raw = cli
        .upgrade_public_key
        .as_deref()
        .context("upgrade public key not configured")?;
    let key_bytes = BASE64_STD
        .decode(pubkey_raw)
        .context("decode upgrade public key failed")?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .context("upgrade public key must be 32 bytes")?;
    let verifying_key =
        VerifyingKey::from_bytes(&key_array).context("parse upgrade public key failed")?;

    let sig_raw = BASE64_STD
        .decode(signature_b64)
        .context("decode signature failed")?;
    let sig_array: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .context("signature must be 64 bytes")?;
    let signature = Signature::from_bytes(&sig_array);

    verifying_key
        .verify(&bytes, &signature)
        .context("signature verification failed")?;

    install_upgrade_binary(bytes.to_vec())?;
    Ok("upgrade applied, restarting agent".to_string())
}

fn install_upgrade_binary(new_binary: Vec<u8>) -> Result<()> {
    let exe_path = std::env::current_exe().context("resolve current executable path failed")?;
    let new_path = exe_path.with_extension("new");
    let backup_path = exe_path.with_extension("bak");

    std::fs::write(&new_path, &new_binary)
        .with_context(|| format!("write new binary failed: {}", new_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&new_path)
            .with_context(|| format!("read metadata failed: {}", new_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_path, perms)
            .with_context(|| format!("chmod new binary failed: {}", new_path.display()))?;
    }

    if backup_path.exists() {
        std::fs::remove_file(&backup_path)
            .with_context(|| format!("remove old backup failed: {}", backup_path.display()))?;
    }

    std::fs::rename(&exe_path, &backup_path).with_context(|| {
        format!(
            "rename current binary to backup failed: {}",
            backup_path.display()
        )
    })?;

    if let Err(err) = std::fs::rename(&new_path, &exe_path) {
        let _ = std::fs::rename(&backup_path, &exe_path);
        anyhow::bail!("activate upgraded binary failed: {err}");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = std::process::Command::new(&exe_path).args(args).spawn() {
        let _ = std::fs::rename(&exe_path, &new_path);
        let _ = std::fs::rename(&backup_path, &exe_path);
        anyhow::bail!("restart upgraded binary failed: {err}");
    }

    Ok(())
}

async fn post_command_ack(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    payload: &CommandAckPayload,
) -> Result<()> {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .context("post command ack failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        warn!(status = %status, body, "command ack rejected");
    }

    Ok(())
}

async fn register(client: &reqwest::Client, url: &str, cli: &Cli) -> Result<()> {
    let payload = RegisterPayload {
        agent_id: cli.id.clone(),
        display_name: cli.name.clone(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let response = client
        .post(url)
        .bearer_auth(&cli.token)
        .json(&payload)
        .send()
        .await
        .context("register request failed")?;

    let status = response.status();
    if status != StatusCode::OK {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        anyhow::bail!("register failed: status={} body={}", status, body);
    }

    info!(agent_id = cli.id, "agent registered");
    Ok(())
}

async fn post_report(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    payload: &ReportPayload,
) -> Result<()> {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .context("report request failed")?;

    match response.status() {
        StatusCode::OK => Ok(()),
        StatusCode::TOO_MANY_REQUESTS => {
            warn!("dashboard rate limited report upload");
            Ok(())
        }
        code => {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            warn!(status = %code, body, "report rejected");
            Ok(())
        }
    }
}

fn collect_report(
    cli: &Cli,
    boot_id: &str,
    seq: u64,
    system: &mut System,
    networks: &mut Networks,
    disks: &mut Disks,
    last_net_total: &mut Option<(u64, u64, Instant)>,
) -> ReportPayload {
    system.refresh_all();
    networks.refresh(true);
    disks.refresh(true);

    let cpu_usage = system.global_cpu_usage();
    let cpu_cores = system.cpus().len();

    let memory_total = system.total_memory();
    let memory_used = system.used_memory();
    let swap_total = system.total_swap();
    let swap_used = system.used_swap();

    let (disk_total, disk_used) = aggregate_disk_usage(disks);

    let mut total_rx = 0_u64;
    let mut total_tx = 0_u64;
    for (_name, data) in &*networks {
        total_rx = total_rx.saturating_add(data.total_received());
        total_tx = total_tx.saturating_add(data.total_transmitted());
    }

    let now = Instant::now();
    let (rx_rate, tx_rate) = if let Some((last_rx, last_tx, last_at)) = last_net_total {
        let elapsed = now.duration_since(*last_at).as_secs_f64().max(0.001);
        let rx = total_rx.saturating_sub(*last_rx) as f64 / elapsed;
        let tx = total_tx.saturating_sub(*last_tx) as f64 / elapsed;
        (rx, tx)
    } else {
        (0.0, 0.0)
    };
    *last_net_total = Some((total_rx, total_tx, now));

    let load_avg = System::load_average();

    let payload = json!({
        "system": {
            "agent_id": cli.id,
            "agent_version": env!("CARGO_PKG_VERSION"),
            "host_name": System::host_name(),
            "os": System::name(),
            "kernel_version": System::kernel_version(),
            "uptime_seconds": System::uptime(),
        },
        "cpu": {
            "usage_percent": cpu_usage,
            "cores": cpu_cores,
        },
        "memory": {
            "total_bytes": memory_total,
            "used_bytes": memory_used,
            "swap_total_bytes": swap_total,
            "swap_used_bytes": swap_used,
        },
        "disk": {
            "total_bytes": disk_total,
            "used_bytes": disk_used,
        },
        "network": {
            "rx_bps": rx_rate,
            "tx_bps": tx_rate,
            "rx_total_bytes": total_rx,
            "tx_total_bytes": total_tx,
        },
        "load": {
            "load1": load_avg.one,
            "load5": load_avg.five,
            "load15": load_avg.fifteen,
        }
    });

    ReportPayload {
        agent_id: cli.id.clone(),
        boot_id: boot_id.to_string(),
        seq,
        collected_at: Utc::now(),
        payload,
    }
}

fn aggregate_disk_usage(disks: &Disks) -> (u64, u64) {
    let mut disk_total = 0_u64;
    let mut disk_used = 0_u64;
    let mut seen = HashSet::new();

    for disk in disks.list() {
        if !should_count_disk(disk) {
            continue;
        }
        let fs = disk.file_system().to_string_lossy().to_ascii_lowercase();
        let name = disk.name().to_string_lossy();
        let total = disk.total_space();
        let dedup_key = format!("{name}|{fs}|{total}");
        if !seen.insert(dedup_key) {
            continue;
        }
        let used = total.saturating_sub(disk.available_space());
        disk_total = disk_total.saturating_add(total);
        disk_used = disk_used.saturating_add(used);
    }

    if disk_total > 0 {
        return (disk_total, disk_used);
    }

    for disk in disks.list() {
        let total = disk.total_space();
        let used = total.saturating_sub(disk.available_space());
        disk_total = disk_total.saturating_add(total);
        disk_used = disk_used.saturating_add(used);
    }
    (disk_total, disk_used)
}

fn should_count_disk(disk: &Disk) -> bool {
    let total = disk.total_space();
    if total == 0 {
        return false;
    }

    let fs = disk.file_system().to_string_lossy().to_ascii_lowercase();
    let mount = disk.mount_point().to_string_lossy();
    let ignored_fs = [
        "tmpfs",
        "devtmpfs",
        "overlay",
        "aufs",
        "squashfs",
        "proc",
        "sysfs",
        "cgroup",
        "cgroup2",
        "mqueue",
        "rpc_pipefs",
        "devpts",
        "autofs",
        "tracefs",
        "nsfs",
        "fusectl",
        "pstore",
        "securityfs",
        "debugfs",
        "configfs",
        "ramfs",
        "selinuxfs",
        "hugetlbfs",
    ];

    if ignored_fs.contains(&fs.as_str()) {
        return false;
    }
    if mount.starts_with("/proc") || mount.starts_with("/sys") {
        return false;
    }

    true
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();
}
