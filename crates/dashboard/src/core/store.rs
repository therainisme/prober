use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::core::config;
use crate::core::models::{AgentHistoryPoint, AgentView, MetricSnapshot};
use crate::core::state::AppState;

pub fn query_agents(
    conn: &Connection,
    include_deleted: bool,
    hot: &config::HotConfig,
) -> rusqlite::Result<Vec<AgentView>> {
    let mut sql = String::from(
        "SELECT a.agent_id, a.display_name, a.agent_version, a.first_seen, a.last_seen, a.deleted_at, \
        (SELECT payload_json FROM metric_reports m WHERE m.agent_id = a.agent_id ORDER BY m.received_at DESC LIMIT 1) as latest_payload \
        FROM agents a",
    );
    if !include_deleted {
        sql.push_str(" WHERE a.deleted_at IS NULL");
    }
    sql.push_str(" ORDER BY a.first_seen ASC");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let agent_id: String = row.get(0)?;
        let display_name: Option<String> = row.get(1)?;
        let agent_version: Option<String> = row.get(2)?;
        let first_seen: String = row.get(3)?;
        let last_seen: String = row.get(4)?;
        let deleted_at: Option<String> = row.get(5)?;
        let latest_payload: Option<String> = row.get(6)?;

        let online = deleted_at.is_none() && is_online(&last_seen, hot.offline_threshold_seconds);
        let metrics = latest_payload
            .as_deref()
            .and_then(parse_metrics_from_payload_str);

        Ok(AgentView {
            agent_id,
            display_name,
            agent_version,
            first_seen,
            last_seen,
            deleted_at,
            online,
            metrics,
        })
    })?;

    rows.collect()
}

pub fn query_agent_by_id(
    conn: &Connection,
    agent_id: &str,
    hot: &config::HotConfig,
) -> rusqlite::Result<Option<AgentView>> {
    let mut stmt = conn.prepare(
        "SELECT a.agent_id, a.display_name, a.agent_version, a.first_seen, a.last_seen, a.deleted_at, \
        (SELECT payload_json FROM metric_reports m WHERE m.agent_id = a.agent_id ORDER BY m.received_at DESC LIMIT 1) as latest_payload \
        FROM agents a WHERE a.agent_id = ?1",
    )?;

    stmt.query_row(params![agent_id], |row| {
        let agent_id: String = row.get(0)?;
        let display_name: Option<String> = row.get(1)?;
        let agent_version: Option<String> = row.get(2)?;
        let first_seen: String = row.get(3)?;
        let last_seen: String = row.get(4)?;
        let deleted_at: Option<String> = row.get(5)?;
        let latest_payload: Option<String> = row.get(6)?;

        let online = deleted_at.is_none() && is_online(&last_seen, hot.offline_threshold_seconds);
        let metrics = latest_payload
            .as_deref()
            .and_then(parse_metrics_from_payload_str);

        Ok(AgentView {
            agent_id,
            display_name,
            agent_version,
            first_seen,
            last_seen,
            deleted_at,
            online,
            metrics,
        })
    })
    .optional()
}

pub fn query_history_raw(
    conn: &Connection,
    agent_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: usize,
) -> rusqlite::Result<Vec<AgentHistoryPoint>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT collected_at, received_at, clock_skewed, payload_json
        FROM metric_reports
        WHERE agent_id = ?1 AND collected_at >= ?2 AND collected_at <= ?3
        ORDER BY collected_at ASC
        LIMIT ?4
        "#,
    )?;

    let rows = stmt.query_map(
        params![agent_id, from.to_rfc3339(), to.to_rfc3339(), limit as i64],
        |row| {
            let collected_at: String = row.get(0)?;
            let received_at: String = row.get(1)?;
            let clock_skewed: i64 = row.get(2)?;
            let payload_json: String = row.get(3)?;
            Ok((collected_at, received_at, clock_skewed != 0, payload_json))
        },
    )?;

    let mut points = Vec::new();
    for row in rows {
        let (collected_at, received_at, clock_skewed, payload_json) = row?;
        if let Some(metrics) = parse_metrics_from_payload_str(&payload_json) {
            let timestamp = if clock_skewed {
                received_at
            } else {
                collected_at
            };
            points.push(AgentHistoryPoint {
                timestamp,
                clock_skewed,
                metrics,
            });
        }
    }

    Ok(points)
}

pub fn query_history_rollups(
    conn: &Connection,
    agent_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: usize,
    bucket_seconds: i64,
) -> rusqlite::Result<Vec<AgentHistoryPoint>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT bucket_start, cpu_avg, memory_used_avg, memory_total_avg,
               disk_used_avg, disk_total_avg, rx_bps_avg, tx_bps_avg
        FROM metric_rollups
        WHERE agent_id = ?1 AND bucket_seconds = ?2
          AND bucket_start >= ?3 AND bucket_start <= ?4
        ORDER BY bucket_start ASC
        LIMIT ?5
        "#,
    )?;

    let rows = stmt.query_map(
        params![
            agent_id,
            bucket_seconds,
            from.to_rfc3339(),
            to.to_rfc3339(),
            limit as i64
        ],
        |row| {
            Ok(AgentHistoryPoint {
                timestamp: row.get::<_, String>(0)?,
                clock_skewed: false,
                metrics: MetricSnapshot {
                    cpu_usage_percent: row.get::<_, Option<f64>>(1)?,
                    memory_used_bytes: row.get::<_, Option<f64>>(2)?.map(|v| v.round() as u64),
                    memory_total_bytes: row.get::<_, Option<f64>>(3)?.map(|v| v.round() as u64),
                    disk_used_bytes: row.get::<_, Option<f64>>(4)?.map(|v| v.round() as u64),
                    disk_total_bytes: row.get::<_, Option<f64>>(5)?.map(|v| v.round() as u64),
                    rx_bps: row.get::<_, Option<f64>>(6)?,
                    tx_bps: row.get::<_, Option<f64>>(7)?,
                },
            })
        },
    )?;

    rows.collect()
}

pub fn parse_metrics_from_payload_str(payload_json: &str) -> Option<MetricSnapshot> {
    let payload: Value = serde_json::from_str(payload_json).ok()?;
    Some(MetricSnapshot {
        cpu_usage_percent: get_f64(&payload, &["cpu", "usage_percent"]),
        memory_used_bytes: get_u64(&payload, &["memory", "used_bytes"]),
        memory_total_bytes: get_u64(&payload, &["memory", "total_bytes"]),
        disk_used_bytes: get_u64(&payload, &["disk", "used_bytes"]),
        disk_total_bytes: get_u64(&payload, &["disk", "total_bytes"]),
        rx_bps: get_f64(&payload, &["network", "rx_bps"]),
        tx_bps: get_f64(&payload, &["network", "tx_bps"]),
    })
}

pub fn parse_rfc3339_to_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|v| v.with_timezone(&Utc))
}

pub fn is_online(last_seen: &str, threshold_seconds: u64) -> bool {
    let parsed = DateTime::parse_from_rfc3339(last_seen);
    if let Ok(seen) = parsed {
        let seen_utc = seen.with_timezone(&Utc);
        let elapsed = Utc::now() - seen_utc;
        elapsed.num_seconds() >= 0 && elapsed.num_seconds() as u64 <= threshold_seconds
    } else {
        false
    }
}

pub fn broadcast_agent_update(state: &AppState, payload: &Value) {
    let _ = state.events_tx.send(payload.to_string());
}

fn get_f64(value: &Value, path: &[&str]) -> Option<f64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_f64()
}

fn get_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

#[cfg(test)]
mod tests {
    use super::{is_online, parse_metrics_from_payload_str};

    #[test]
    fn parse_metrics_handles_core_fields() {
        let raw = r#"{
            "cpu": {"usage_percent": 42.5},
            "memory": {"used_bytes": 100, "total_bytes": 200},
            "disk": {"used_bytes": 300, "total_bytes": 500},
            "network": {"rx_bps": 10.5, "tx_bps": 20.5}
        }"#;

        let metrics = parse_metrics_from_payload_str(raw).expect("parse metrics");
        assert_eq!(metrics.cpu_usage_percent, Some(42.5));
        assert_eq!(metrics.memory_total_bytes, Some(200));
        assert_eq!(metrics.tx_bps, Some(20.5));
    }

    #[test]
    fn online_check_respects_threshold() {
        let now = chrono::Utc::now();
        assert!(is_online(&now.to_rfc3339(), 5));
        let old = now - chrono::Duration::seconds(10);
        assert!(!is_online(&old.to_rfc3339(), 5));
    }
}
