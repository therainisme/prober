mod core;
mod handlers;

use anyhow::{Context, Result};
use clap::Parser;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::info;

use crate::core::config;
use crate::core::db;
use crate::core::runtime;
use crate::core::state::{AppState, ModeKind};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = config::Cli::parse();
    let loaded = config::Config::load(&cli)?;

    init_tracing(&loaded.hot.log_level);

    info!("effective config: {}", loaded.redact_summary());
    info!("sqlite db file: {}", db::db_file_path(&loaded.data_dir));

    let conn = db::open_and_init(&loaded)?;
    let (events_tx, _) = broadcast::channel(2048);
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build dashboard http client")?;

    let state = AppState::new(cli, loaded.clone(), conn, events_tx, http_client);

    runtime::spawn_background_worker(state.clone());

    let app = handlers::build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(loaded.listen)
        .await
        .with_context(|| format!("bind {}", loaded.listen))?;
    info!("dashboard listening on {}", loaded.listen);

    runtime::set_desired_mode(&state, ModeKind::Heartbeat)
        .await
        .context("set initial mode")?;

    axum::serve(listener, app)
        .await
        .context("run http server")?;
    Ok(())
}

fn init_tracing(log_level: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level.to_string())),
        )
        .with_target(false)
        .compact()
        .init();
}
