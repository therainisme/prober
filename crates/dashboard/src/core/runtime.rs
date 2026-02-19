use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{error, warn};
use uuid::Uuid;

use crate::core::models::OutboundCommand;
use crate::core::state::{AppState, DesiredMode, ModeKind};
use crate::core::store::{broadcast_agent_update, is_online, parse_rfc3339_to_utc};

pub fn spawn_background_worker(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        let mut rounds: u64 = 0;
        loop {
            ticker.tick().await;
            rounds = rounds.saturating_add(1);

            if let Err(err) = resend_pending_commands(&state).await {
                error!(error = %err, "resend pending commands failed");
            }
            if let Err(err) = scan_and_emit_alerts(&state).await {
                error!(error = %err, "alert scan failed");
            }
            if rounds % 10 == 0 {
                if let Err(err) = run_retention_and_rollup_jobs(&state).await {
                    error!(error = %err, "retention jobs failed");
                }
                if let Err(err) = hard_delete_expired_agents(&state).await {
                    error!(error = %err, "hard-delete job failed");
                }
            }
            if rounds % 120 == 0
                && let Err(err) = sqlite_maintenance(&state).await
            {
                warn!(error = %err, "sqlite maintenance failed");
            }
        }
    });
}

pub async fn sqlite_maintenance(state: &AppState) -> Result<()> {
    let conn = state.db.lock().await;
    conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;")
        .context("sqlite maintenance pragmas")?;
    Ok(())
}

pub async fn set_desired_mode(state: &AppState, mode: ModeKind) -> Result<()> {
    let current = state.desired_mode.read().await.clone();
    if current.kind == mode {
        return Ok(());
    }

    {
        let mut desired = state.desired_mode.write().await;
        desired.kind = mode;
    }

    let desired = state.desired_mode.read().await.clone();
    let agent_ids = list_active_agent_ids(state).await?;
    for agent_id in agent_ids {
        issue_set_mode_command(state, &agent_id, &desired).await?;
    }

    broadcast_agent_update(
        state,
        &json!({"type": "mode_changed", "mode": desired.kind.as_str()}),
    );
    Ok(())
}

pub async fn ensure_current_mode_command_for_agent(state: &AppState, agent_id: &str) -> Result<()> {
    let desired = state.desired_mode.read().await.clone();
    issue_set_mode_command(state, agent_id, &desired).await
}

pub async fn issue_set_mode_command(
    state: &AppState,
    agent_id: &str,
    desired: &DesiredMode,
) -> Result<()> {
    let payload = json!({
        "mode": desired.kind.as_str(),
        "recommended_realtime_interval_seconds": desired.realtime_interval_seconds,
        "heartbeat_interval_seconds": desired.heartbeat_interval_seconds,
        "offline_threshold_seconds": state.hot_config.read().await.offline_threshold_seconds,
    });

    create_and_dispatch_command(
        state,
        agent_id,
        "set_mode",
        payload,
        state.static_config.command_expire_seconds,
    )
    .await?;
    Ok(())
}

pub async fn create_and_dispatch_command(
    state: &AppState,
    agent_id: &str,
    command_type: &str,
    payload: Value,
    expire_seconds: u64,
) -> Result<Uuid> {
    let now = Utc::now();
    let command = OutboundCommand {
        command_id: Uuid::new_v4(),
        command_type: command_type.to_string(),
        issued_at: now,
        expire_at: now + chrono::Duration::seconds(expire_seconds as i64),
        payload,
    };
    let raw = serde_json::to_string(&command).context("serialize command")?;

    {
        let conn = state.db.lock().await;
        conn.execute(
            r#"
            INSERT INTO agent_commands
            (command_id, agent_id, command_type, payload_json, issued_at, expire_at, status, last_sent_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', NULL)
            "#,
            params![
                command.command_id.to_string(),
                agent_id,
                command.command_type,
                command.payload.to_string(),
                command.issued_at.to_rfc3339(),
                command.expire_at.to_rfc3339(),
            ],
        )
        .context("insert command")?;
    }

    let sender = get_or_create_command_sender(state, agent_id).await;
    let _ = sender.send(raw);

    {
        let conn = state.db.lock().await;
        conn.execute(
            "UPDATE agent_commands SET last_sent_at = ?2 WHERE command_id = ?1",
            params![command.command_id.to_string(), Utc::now().to_rfc3339()],
        )
        .context("update command last_sent_at")?;
    }

    Ok(command.command_id)
}

pub async fn get_or_create_command_sender(
    state: &AppState,
    agent_id: &str,
) -> broadcast::Sender<String> {
    let mut map = state.command_channels.lock().await;
    map.entry(agent_id.to_string())
        .or_insert_with(|| {
            let (tx, _) = broadcast::channel(256);
            tx
        })
        .clone()
}

pub async fn load_pending_command_payloads(
    state: &AppState,
    agent_id: &str,
) -> Result<Vec<String>> {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare(
            r#"
            SELECT command_id, command_type, payload_json, issued_at, expire_at
            FROM agent_commands
            WHERE agent_id = ?1 AND status = 'pending' AND expire_at > ?2
            ORDER BY issued_at ASC
            "#,
        )
        .context("prepare query pending commands")?;

    let now = Utc::now().to_rfc3339();
    let rows = stmt
        .query_map(params![agent_id, now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .context("query pending commands")?;

    let mut out = Vec::new();
    for row in rows {
        let (command_id, command_type, payload_json, issued_at, expire_at) = row?;
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({}));
        let cmd = OutboundCommand {
            command_id: Uuid::parse_str(&command_id).unwrap_or_else(|_| Uuid::new_v4()),
            command_type,
            issued_at: parse_rfc3339_to_utc(&issued_at).unwrap_or_else(Utc::now),
            expire_at: parse_rfc3339_to_utc(&expire_at).unwrap_or_else(Utc::now),
            payload,
        };
        out.push(serde_json::to_string(&cmd).context("serialize pending command")?);
    }

    Ok(out)
}

pub async fn resend_pending_commands(state: &AppState) -> Result<()> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    let candidates: Vec<(String, String, String, String, String, String)> = {
        let conn = state.db.lock().await;
        conn.execute(
            "UPDATE agent_commands SET status = 'expired' WHERE status = 'pending' AND expire_at <= ?1",
            params![now_str.clone()],
        )
        .context("expire pending commands")?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT command_id, agent_id, command_type, payload_json, issued_at, expire_at
                FROM agent_commands
                WHERE status = 'pending'
                  AND expire_at > ?1
                  AND (last_sent_at IS NULL OR last_sent_at <= ?2)
                ORDER BY issued_at ASC
                LIMIT 256
                "#,
            )
            .context("prepare resend query")?;

        let resend_threshold = (now - chrono::Duration::seconds(20)).to_rfc3339();
        let rows = stmt
            .query_map(params![now_str.clone(), resend_threshold], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .context("query resend candidates")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (command_id, agent_id, command_type, payload_json, issued_at, expire_at) in candidates {
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({}));
        let cmd = OutboundCommand {
            command_id: Uuid::parse_str(&command_id).unwrap_or_else(|_| Uuid::new_v4()),
            command_type,
            issued_at: parse_rfc3339_to_utc(&issued_at).unwrap_or_else(Utc::now),
            expire_at: parse_rfc3339_to_utc(&expire_at).unwrap_or_else(Utc::now),
            payload,
        };
        let raw = serde_json::to_string(&cmd).context("serialize resend command")?;

        let sender = get_or_create_command_sender(state, &agent_id).await;
        let _ = sender.send(raw);

        let conn = state.db.lock().await;
        conn.execute(
            "UPDATE agent_commands SET last_sent_at = ?2 WHERE command_id = ?1",
            params![command_id, Utc::now().to_rfc3339()],
        )
        .context("mark command resent")?;
    }

    Ok(())
}

pub async fn list_active_agent_ids(state: &AppState) -> Result<Vec<String>> {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare("SELECT agent_id FROM agents WHERE deleted_at IS NULL")
        .context("prepare list active agent ids")?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query active agent ids")?;

    let ids = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect active agent ids")?;
    Ok(ids)
}

pub async fn hard_delete_expired_agents(state: &AppState) -> Result<()> {
    let cutoff = (Utc::now()
        - chrono::Duration::days(state.static_config.soft_delete_retention_days as i64))
    .to_rfc3339();

    let conn = state.db.lock().await;
    let deleted = conn
        .execute(
            "DELETE FROM agents WHERE deleted_at IS NOT NULL AND deleted_at <= ?1",
            params![cutoff],
        )
        .context("hard delete expired soft-deleted agents")?;

    if deleted > 0 {
        broadcast_agent_update(state, &json!({"type": "hard_deleted", "count": deleted}));
    }
    Ok(())
}

pub async fn run_retention_and_rollup_jobs(state: &AppState) -> Result<()> {
    let retention_days = state.static_config.raw_retention_days as i64;

    let conn = state.db.lock().await;

    conn.execute_batch(
        r#"
        INSERT OR REPLACE INTO metric_rollups
        (agent_id, bucket_start, bucket_seconds, cpu_avg, cpu_max, cpu_min,
         memory_used_avg, memory_total_avg, disk_used_avg, disk_total_avg,
         rx_bps_avg, tx_bps_avg, sample_count)
        SELECT
            agent_id,
            datetime((CAST(strftime('%s', collected_at) AS INTEGER) / 300) * 300, 'unixepoch') AS bucket_start,
            300,
            AVG(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            MAX(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            MIN(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.memory.used_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.memory.total_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.disk.used_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.disk.total_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.network.rx_bps') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.network.tx_bps') AS REAL)),
            COUNT(*)
        FROM metric_reports
        WHERE CAST(strftime('%s', collected_at) AS INTEGER) >= CAST(strftime('%s', 'now', '-7 day') AS INTEGER)
          AND CAST(strftime('%s', collected_at) AS INTEGER) < CAST(strftime('%s', 'now', '-1 day') AS INTEGER)
        GROUP BY agent_id, bucket_start;

        INSERT OR REPLACE INTO metric_rollups
        (agent_id, bucket_start, bucket_seconds, cpu_avg, cpu_max, cpu_min,
         memory_used_avg, memory_total_avg, disk_used_avg, disk_total_avg,
         rx_bps_avg, tx_bps_avg, sample_count)
        SELECT
            agent_id,
            datetime((CAST(strftime('%s', collected_at) AS INTEGER) / 3600) * 3600, 'unixepoch') AS bucket_start,
            3600,
            AVG(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            MAX(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            MIN(CAST(json_extract(payload_json, '$.cpu.usage_percent') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.memory.used_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.memory.total_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.disk.used_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.disk.total_bytes') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.network.rx_bps') AS REAL)),
            AVG(CAST(json_extract(payload_json, '$.network.tx_bps') AS REAL)),
            COUNT(*)
        FROM metric_reports
        WHERE CAST(strftime('%s', collected_at) AS INTEGER) >= CAST(strftime('%s', 'now', '-30 day') AS INTEGER)
          AND CAST(strftime('%s', collected_at) AS INTEGER) < CAST(strftime('%s', 'now', '-7 day') AS INTEGER)
        GROUP BY agent_id, bucket_start;

        DELETE FROM metric_reports
        WHERE CAST(strftime('%s', collected_at) AS INTEGER) < CAST(strftime('%s', 'now', '-1 day') AS INTEGER);

        DELETE FROM metric_rollups
        WHERE bucket_seconds = 300
          AND CAST(strftime('%s', bucket_start) AS INTEGER) < CAST(strftime('%s', 'now', '-7 day') AS INTEGER);
        "#,
    )
    .context("run rollup and compaction")?;

    let retention_cutoff = (Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();

    conn.execute(
        "DELETE FROM metric_rollups WHERE bucket_start < ?1",
        params![retention_cutoff.clone()],
    )
    .context("delete old rollups")?;

    conn.execute(
        "DELETE FROM command_acks WHERE received_at < ?1",
        params![retention_cutoff.clone()],
    )
    .context("delete old command acks")?;

    conn.execute(
        "DELETE FROM alert_events WHERE sent_at < ?1",
        params![retention_cutoff],
    )
    .context("delete old alert events")?;

    Ok(())
}

pub async fn scan_and_emit_alerts(state: &AppState) -> Result<()> {
    let hot = state.hot_config.read().await.clone();

    let agents: Vec<(String, Option<String>, String)> = {
        let conn = state.db.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, display_name, last_seen FROM agents WHERE deleted_at IS NULL",
            )
            .context("prepare query agents for alerts")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("query agents for alerts")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let now = Utc::now();
    let mut alert_state = state.alert_state.lock().await;

    for (agent_id, display_name, last_seen) in agents {
        let online = is_online(&last_seen, hot.offline_threshold_seconds);
        let name = display_name.unwrap_or_else(|| agent_id.clone());

        let item = alert_state.entry(agent_id.clone()).or_default();
        if online {
            if item.was_offline && item.offline_alert_sent {
                let msg = format!(
                    "[Prober] Agent recovered: {name} ({agent_id}) at {}",
                    now.to_rfc3339()
                );
                if should_emit_alert(state, now, hot.alert_quiet_window_minutes).await {
                    emit_alert(state, &agent_id, "recovered", &msg).await?;
                }
            }
            item.was_offline = false;
            item.offline_alert_sent = false;
            continue;
        }

        let in_debounce = item
            .last_offline_alert_at
            .map(|t| now - t < chrono::Duration::minutes(30))
            .unwrap_or(false);

        if !in_debounce && !item.offline_alert_sent {
            let msg = format!(
                "[Prober] Agent offline: {name} ({agent_id}), last_seen={last_seen}, threshold={}s",
                hot.offline_threshold_seconds
            );

            if should_emit_alert(state, now, hot.alert_quiet_window_minutes).await {
                emit_alert(state, &agent_id, "offline", &msg).await?;
                item.offline_alert_sent = true;
                item.last_offline_alert_at = Some(now);
            }
        }

        item.was_offline = true;
    }

    Ok(())
}

pub async fn should_emit_alert(
    state: &AppState,
    now: DateTime<Utc>,
    quiet_window_minutes: u64,
) -> bool {
    let mut last = state.last_global_alert_sent_at.lock().await;
    let (allow, next_last) = should_emit_alert_with_last(*last, now, quiet_window_minutes);
    *last = next_last;
    allow
}

fn should_emit_alert_with_last(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    quiet_window_minutes: u64,
) -> (bool, Option<DateTime<Utc>>) {
    if quiet_window_minutes == 0 {
        return (true, Some(now));
    }

    let allow = match last {
        Some(prev) => now - prev >= chrono::Duration::minutes(quiet_window_minutes as i64),
        None => true,
    };
    if allow {
        (true, Some(now))
    } else {
        (false, last)
    }
}

pub async fn emit_alert(state: &AppState, agent_id: &str, kind: &str, message: &str) -> Result<()> {
    {
        let conn = state.db.lock().await;
        conn.execute(
            "INSERT INTO alert_events (agent_id, kind, message, sent_at) VALUES (?1, ?2, ?3, ?4)",
            params![agent_id, kind, message, Utc::now().to_rfc3339()],
        )
        .context("insert alert event")?;
    }

    if let (Some(bot_token), Some(chat_id)) = (
        state.static_config.telegram_bot_token.as_deref(),
        state.static_config.telegram_chat_id.as_deref(),
    ) {
        let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
        let payload = json!({
            "chat_id": chat_id,
            "text": message,
            "disable_web_page_preview": true,
        });

        let resp = state
            .http_client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("send telegram alert")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            warn!(status = %status, body, "telegram alert failed");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::should_emit_alert_with_last;

    #[test]
    fn quiet_window_blocks_repeated_alerts() {
        let now = Utc::now();
        let (allow_first, last) = should_emit_alert_with_last(None, now, 30);
        assert!(allow_first);

        let (allow_second, _) = should_emit_alert_with_last(last, now + Duration::minutes(10), 30);
        assert!(!allow_second);

        let (allow_third, _) = should_emit_alert_with_last(last, now + Duration::minutes(31), 30);
        assert!(allow_third);
    }
}
