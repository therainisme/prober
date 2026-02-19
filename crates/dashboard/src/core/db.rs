use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::core::config::Config;

pub fn open_and_init(config: &Config) -> Result<Connection> {
    let db_path = config.data_dir.join("prober.db");
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open sqlite {}", db_path.display()))?;

    apply_pragmas(&conn, config)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection, config: &Config) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )
    .context("apply sqlite baseline pragmas")?;

    conn.pragma_update(None, "busy_timeout", config.sqlite_busy_timeout_ms)
        .context("set busy_timeout")?;
    conn.pragma_update(None, "wal_autocheckpoint", config.sqlite_wal_autocheckpoint)
        .context("set wal_autocheckpoint")?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .context("set auto_vacuum")?;

    Ok(())
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS agents (
            agent_id TEXT PRIMARY KEY,
            display_name TEXT,
            agent_version TEXT,
            first_seen TEXT NOT NULL,
            last_seen TEXT NOT NULL,
            deleted_at TEXT
        );

        CREATE TABLE IF NOT EXISTS metric_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            boot_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            collected_at TEXT NOT NULL,
            received_at TEXT NOT NULL,
            clock_skewed INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            UNIQUE (agent_id, boot_id, seq),
            FOREIGN KEY(agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS command_acks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            status TEXT NOT NULL,
            detail TEXT,
            received_at TEXT NOT NULL,
            UNIQUE(command_id, agent_id),
            FOREIGN KEY(agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS agent_commands (
            command_id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            command_type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            issued_at TEXT NOT NULL,
            expire_at TEXT NOT NULL,
            status TEXT NOT NULL,
            last_sent_at TEXT,
            ack_status TEXT,
            ack_detail TEXT,
            acked_at TEXT,
            FOREIGN KEY(agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS alert_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            message TEXT NOT NULL,
            sent_at TEXT NOT NULL,
            FOREIGN KEY(agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS metric_rollups (
            agent_id TEXT NOT NULL,
            bucket_start TEXT NOT NULL,
            bucket_seconds INTEGER NOT NULL,
            cpu_avg REAL,
            cpu_max REAL,
            cpu_min REAL,
            memory_used_avg REAL,
            memory_total_avg REAL,
            disk_used_avg REAL,
            disk_total_avg REAL,
            rx_bps_avg REAL,
            tx_bps_avg REAL,
            sample_count INTEGER NOT NULL,
            PRIMARY KEY(agent_id, bucket_start, bucket_seconds),
            FOREIGN KEY(agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_metric_reports_agent_collected_at
            ON metric_reports(agent_id, collected_at);
        CREATE INDEX IF NOT EXISTS idx_agents_deleted_at
            ON agents(deleted_at);
        CREATE INDEX IF NOT EXISTS idx_agent_commands_agent_status_expire
            ON agent_commands(agent_id, status, expire_at);
        CREATE INDEX IF NOT EXISTS idx_metric_rollups_bucket
            ON metric_rollups(agent_id, bucket_seconds, bucket_start);
        "#,
    )
    .context("init schema")?;

    Ok(())
}

pub fn db_file_path(data_dir: &Path) -> String {
    data_dir.join("prober.db").display().to_string()
}
