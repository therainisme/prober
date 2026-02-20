use std::convert::Infallible;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use async_stream::stream;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use rusqlite::params;
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::core::auth;
use crate::core::config;
use crate::core::models::{
    AdminBackupRequest, AdminLoginRequest, AdminUpgradeRequest, AgentCommandQuery,
    AgentHistoryResponse, AgentListQuery, AgentRegisterRequest, AgentReportRequest, AgentsResponse,
    ApiError, BackupResponse, CommandAckRequest, HistoryQuery, HotConfigView, ReloadConfigResponse,
    UpgradeResponse,
};
use crate::core::runtime;
use crate::core::state::{
    ADMIN_SESSION_COOKIE, ADMIN_SESSION_SECONDS, AdminSession, AgentCommandConnectionGuard,
    AppState, BrowserSubscriberGuard, ModeKind, TokenBucket,
};
use crate::core::store;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/favicon.ico", get(favicon))
        .route("/assets/app.js", get(asset_app_js))
        .route("/assets/styles.css", get(asset_styles_css))
        .route("/healthz", get(healthz))
        .route("/api/events", get(events_sse))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{agent_id}", get(get_agent_detail))
        .route("/api/agents/{agent_id}/history", get(get_agent_history))
        .route("/api/agent/commands", get(agent_commands_sse))
        .route("/api/agent/register", post(agent_register))
        .route("/api/agent/report", post(agent_report))
        .route("/api/agent/command-ack", post(agent_command_ack))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/logout", post(admin_logout))
        .route("/api/admin/session", get(admin_session_status))
        .route("/api/admin/reload-config", post(admin_reload_config))
        .route("/api/admin/upgrade", post(admin_issue_upgrade))
        .route("/api/admin/backup", post(admin_backup_db))
        .route(
            "/api/admin/agents/{agent_id}/delete",
            post(admin_delete_agent),
        )
        .route(
            "/api/admin/agents/{agent_id}/recover",
            post(admin_recover_agent),
        )
        .with_state(state)
}

async fn index_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        serve_static("static/index.html", include_str!("../static/index.html")),
    )
}

async fn asset_app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        serve_static("static/app.js", include_str!("../static/app.js")),
    )
}

async fn asset_styles_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        serve_static("static/styles.css", include_str!("../static/styles.css")),
    )
}

fn serve_static(relative_path: &str, embedded: &'static str) -> String {
    if cfg!(debug_assertions) {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join(relative_path);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content;
        }
    }
    embedded.to_string()
}

fn serve_static_bytes(relative_path: &str, embedded: &'static [u8]) -> Vec<u8> {
    if cfg!(debug_assertions) {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join(relative_path);
        if let Ok(content) = std::fs::read(&path) {
            return content;
        }
    }
    embedded.to_vec()
}

async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/x-icon")],
        serve_static_bytes("static/favicon.ico", include_bytes!("../static/favicon.ico")),
    )
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok", "service": "prober-dashboard"}))
}

async fn events_sse(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let snapshot = {
        let hot = state.hot_config.read().await.clone();
        let conn = state.db.lock().await;
        let agents = store::query_agents(&conn, false, &hot).map_err(|e| {
            error!(error = %e, "query snapshot agents failed");
            ApiError::internal("load snapshot failed")
        })?;
        serde_json::to_string(&json!({"agents": agents})).map_err(|e| {
            error!(error = %e, "serialize snapshot failed");
            ApiError::internal("serialize snapshot failed")
        })?
    };

    let prev = state
        .browser_subscribers
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if prev == 0 {
        runtime::set_desired_mode(&state, ModeKind::Realtime)
            .await
            .map_err(|e| {
                error!(error = %e, "set realtime mode on subscriber connect failed");
                ApiError::internal("switch realtime mode failed")
            })?;
    }

    let guard_state = state.clone();
    let mut rx = state.events_tx.subscribe();
    let stream = stream! {
        let _guard = BrowserSubscriberGuard::new(guard_state);
        yield Ok::<Event, Infallible>(Event::default().event("snapshot").data(snapshot));
        loop {
            match rx.recv().await {
                Ok(payload) => yield Ok(Event::default().event("agent_update").data(payload)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let warning = json!({"type": "lagged", "skipped": skipped}).to_string();
                    yield Ok(Event::default().event("warning").data(warning));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(
        HeaderNameConst::X_ACCEL_BUFFERING,
        HeaderValue::from_static("no"),
    );

    Ok((
        headers,
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    ))
}

struct HeaderNameConst;

impl HeaderNameConst {
    const X_ACCEL_BUFFERING: header::HeaderName =
        header::HeaderName::from_static("x-accel-buffering");
}

async fn list_agents(
    State(state): State<AppState>,
    Query(query): Query<AgentListQuery>,
) -> Result<Json<AgentsResponse>, ApiError> {
    let include_deleted = query.include_deleted.unwrap_or(false);
    let hot = state.hot_config.read().await.clone();
    let conn = state.db.lock().await;
    let agents = store::query_agents(&conn, include_deleted, &hot).map_err(|e| {
        error!(error = %e, "query agents failed");
        ApiError::internal("query agents failed")
    })?;

    Ok(Json(AgentsResponse { agents }))
}

async fn get_agent_detail(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<crate::core::models::AgentView>, ApiError> {
    let hot = state.hot_config.read().await.clone();
    let conn = state.db.lock().await;
    let result = store::query_agent_by_id(&conn, &agent_id, &hot).map_err(|e| {
        error!(error = %e, "query agent detail failed");
        ApiError::internal("query agent failed")
    })?;
    result
        .map(Json)
        .ok_or_else(|| ApiError::not_found("agent not found"))
}

async fn get_agent_history(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<AgentHistoryResponse>, ApiError> {
    let to = query.to.unwrap_or_else(Utc::now);
    let from = query
        .from
        .unwrap_or_else(|| to - chrono::Duration::hours(24));
    if from >= to {
        return Err(ApiError::bad_request("from must be earlier than to"));
    }

    let limit = query.limit.unwrap_or(5000).clamp(1, 20000);
    let range_seconds = (to - from).num_seconds().max(0) as u64;

    let conn = state.db.lock().await;
    let points = if range_seconds > 7 * 24 * 3600 {
        store::query_history_rollups(&conn, &agent_id, from, to, limit, 3600).map_err(|e| {
            error!(error = %e, "query 1h history failed");
            ApiError::internal("query history failed")
        })?
    } else if range_seconds > 24 * 3600 {
        store::query_history_rollups(&conn, &agent_id, from, to, limit, 300).map_err(|e| {
            error!(error = %e, "query 5m history failed");
            ApiError::internal("query history failed")
        })?
    } else {
        store::query_history_raw(&conn, &agent_id, from, to, limit).map_err(|e| {
            error!(error = %e, "query raw history failed");
            ApiError::internal("query history failed")
        })?
    };

    Ok(Json(AgentHistoryResponse {
        agent_id,
        from: from.to_rfc3339(),
        to: to.to_rfc3339(),
        points,
    }))
}

async fn agent_commands_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AgentCommandQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth::ensure_agent_auth(&state, &headers)?;
    if query.agent_id.trim().is_empty() {
        return Err(ApiError::bad_request("agent_id is required"));
    }

    runtime::ensure_current_mode_command_for_agent(&state, &query.agent_id)
        .await
        .map_err(|e| {
            error!(error = %e, agent_id = query.agent_id, "ensure mode command failed");
            ApiError::internal("prepare mode command failed")
        })?;

    let bootstraps = runtime::load_pending_command_payloads(&state, &query.agent_id)
        .await
        .map_err(|e| {
            error!(error = %e, agent_id = query.agent_id, "load pending commands failed");
            ApiError::internal("load pending commands failed")
        })?;

    let sender = runtime::get_or_create_command_sender(&state, &query.agent_id).await;
    let mut rx = sender.subscribe();
    let connection_guard =
        AgentCommandConnectionGuard::new(state.clone(), query.agent_id.clone()).await;

    let stream = stream! {
        let _connection_guard = connection_guard;
        for payload in bootstraps {
            yield Ok::<Event, Infallible>(Event::default().event("command").data(payload));
        }
        loop {
            match rx.recv().await {
                Ok(payload) => yield Ok(Event::default().event("command").data(payload)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let mut out_headers = HeaderMap::new();
    out_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    out_headers.insert(
        HeaderNameConst::X_ACCEL_BUFFERING,
        HeaderValue::from_static("no"),
    );

    Ok((
        out_headers,
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    ))
}

async fn agent_register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AgentRegisterRequest>,
) -> Result<Json<Value>, ApiError> {
    auth::ensure_agent_auth(&state, &headers)?;

    if req.agent_id.trim().is_empty() {
        return Err(ApiError::bad_request("agent_id cannot be empty"));
    }

    let command_connections = state.agent_command_connections.lock().await;
    if command_connections.get(&req.agent_id).copied().unwrap_or(0) > 0 {
        return Err(ApiError::conflict("agent_id already online"));
    }
    drop(command_connections);

    let now = Utc::now().to_rfc3339();
    let conn = state.db.lock().await;

    conn.execute(
        r#"
        INSERT INTO agents (agent_id, display_name, agent_version, first_seen, last_seen, deleted_at)
        VALUES (?1, ?2, ?3, ?4, ?5, NULL)
        ON CONFLICT(agent_id) DO UPDATE SET
          display_name = excluded.display_name,
          agent_version = excluded.agent_version,
          last_seen = excluded.last_seen,
          deleted_at = NULL
        "#,
        params![
            req.agent_id,
            req.display_name,
            req.agent_version,
            now,
            now,
        ],
    )
    .map_err(|e| {
        error!(error = %e, "register agent failed");
        ApiError::internal("register failed")
    })?;
    drop(conn);

    store::broadcast_agent_update(&state, &json!({"type": "agent_register"}));

    let current_mode = state.desired_mode.read().await.clone();
    runtime::issue_set_mode_command(&state, &req.agent_id, &current_mode)
        .await
        .map_err(|e| {
            error!(error = %e, agent_id = req.agent_id, "issue initial mode command failed");
            ApiError::internal("register failed")
        })?;

    Ok(Json(json!({"ok": true})))
}

async fn agent_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AgentReportRequest>,
) -> Result<Json<Value>, ApiError> {
    auth::ensure_agent_auth(&state, &headers)?;

    if req.agent_id.trim().is_empty() || req.boot_id.trim().is_empty() {
        return Err(ApiError::bad_request("agent_id and boot_id are required"));
    }

    {
        let mut limiter = state.report_limiter.lock().await;
        let bucket = limiter
            .entry(req.agent_id.clone())
            .or_insert_with(|| TokenBucket::new(3.0));
        if !bucket.allow(1.0, 3.0) {
            return Err(ApiError::too_many_requests(
                "single agent reporting rate exceeds 1 req/s",
            ));
        }
    }

    let received_at = Utc::now();
    let skew = (received_at - req.collected_at)
        .num_seconds()
        .unsigned_abs();
    let clock_skewed = skew > 300;
    if clock_skewed {
        warn!(
            agent_id = req.agent_id,
            skew_seconds = skew,
            "clock skew detected"
        );
    }

    let conn = state.db.lock().await;

    conn.execute(
        r#"
        INSERT INTO agents (agent_id, display_name, agent_version, first_seen, last_seen, deleted_at)
        VALUES (?1, NULL, NULL, ?2, ?2, NULL)
        ON CONFLICT(agent_id) DO UPDATE SET
          last_seen = excluded.last_seen,
          deleted_at = NULL
        "#,
        params![req.agent_id, received_at.to_rfc3339()],
    )
    .map_err(|e| {
        error!(error = %e, "upsert agent before report failed");
        ApiError::internal("store report failed")
    })?;

    let inserted = conn
        .execute(
            r#"
            INSERT OR IGNORE INTO metric_reports
            (agent_id, boot_id, seq, collected_at, received_at, clock_skewed, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                req.agent_id,
                req.boot_id,
                req.seq,
                req.collected_at.to_rfc3339(),
                received_at.to_rfc3339(),
                i32::from(clock_skewed),
                req.payload.to_string(),
            ],
        )
        .map_err(|e| {
            error!(error = %e, "insert report failed");
            ApiError::internal("store report failed")
        })?;

    store::broadcast_agent_update(
        &state,
        &json!({"type": "agent_report", "agent_id": req.agent_id}),
    );

    Ok(Json(
        json!({"ok": true, "clock_skewed": clock_skewed, "duplicated": inserted == 0}),
    ))
}

async fn agent_command_ack(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CommandAckRequest>,
) -> Result<Json<Value>, ApiError> {
    auth::ensure_agent_auth(&state, &headers)?;

    if req.agent_id.trim().is_empty() {
        return Err(ApiError::bad_request("agent_id cannot be empty"));
    }

    let received_at = Utc::now().to_rfc3339();
    let conn = state.db.lock().await;
    conn.execute(
        r#"
        INSERT INTO command_acks (command_id, agent_id, status, detail, received_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(command_id, agent_id) DO UPDATE SET
          status = excluded.status,
          detail = excluded.detail,
          received_at = excluded.received_at
        "#,
        params![
            req.command_id.to_string(),
            req.agent_id,
            req.status,
            req.detail,
            received_at,
        ],
    )
    .map_err(|e| {
        error!(error = %e, "insert command ack failed");
        ApiError::internal("store command ack failed")
    })?;

    conn.execute(
        r#"
        UPDATE agent_commands
        SET status = 'acked', ack_status = ?3, ack_detail = ?4, acked_at = ?5
        WHERE command_id = ?1 AND agent_id = ?2
        "#,
        params![
            req.command_id.to_string(),
            req.agent_id,
            req.status,
            req.detail,
            Utc::now().to_rfc3339(),
        ],
    )
    .map_err(|e| {
        error!(error = %e, "update command status with ack failed");
        ApiError::internal("store command ack failed")
    })?;

    Ok(Json(json!({"ok": true})))
}

async fn admin_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdminLoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let client_ip = auth::parse_client_ip(&headers);
    {
        let mut limiter = state.login_limiter.lock().await;
        let bucket = limiter
            .entry(client_ip)
            .or_insert_with(|| TokenBucket::new(5.0));
        if !bucket.allow(5.0 / 60.0, 5.0) {
            return Err(ApiError::too_many_requests(
                "too many login attempts from this ip",
            ));
        }
    }

    let parsed_hash = PasswordHash::new(&state.static_config.admin_password_hash)
        .map_err(|_| ApiError::internal("stored admin hash is invalid"))?;

    Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed_hash)
        .map_err(|_| ApiError::unauthorized("invalid admin password"))?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let expires_at = Utc::now() + chrono::Duration::seconds(ADMIN_SESSION_SECONDS);
    {
        let mut sessions = state.sessions.lock().await;
        auth::prune_expired_sessions(&mut sessions);
        sessions.insert(session_id.clone(), AdminSession { expires_at });
    }

    let cookie = auth::build_session_cookie(&session_id, ADMIN_SESSION_SECONDS);
    let mut out_headers = HeaderMap::new();
    out_headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|_| ApiError::internal("failed to set session cookie"))?,
    );

    Ok((
        out_headers,
        Json(json!({
            "ok": true,
            "expires_at": expires_at.to_rfc3339(),
        })),
    ))
}

async fn admin_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(session_id) = auth::read_cookie(&headers, ADMIN_SESSION_COOKIE) {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(&session_id);
    }

    let mut out_headers = HeaderMap::new();
    out_headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::clear_session_cookie())
            .map_err(|_| ApiError::internal("failed to clear session cookie"))?,
    );
    Ok((out_headers, Json(json!({"ok": true}))))
}

async fn admin_session_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let active = auth::ensure_admin_session(&state, &headers).await.is_ok();
    Ok(Json(json!({"active": active})))
}

async fn admin_reload_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ReloadConfigResponse>, ApiError> {
    auth::ensure_admin_session(&state, &headers).await?;

    let reloaded = config::Config::load(&state.cli).map_err(|e| {
        error!(error = %e, "reload config failed");
        ApiError::internal("reload config failed")
    })?;

    let mut restart_required = Vec::new();
    if reloaded.listen != state.static_config.listen {
        restart_required.push("listen");
    }
    if reloaded.data_dir != state.static_config.data_dir {
        restart_required.push("data_dir");
    }
    if reloaded.agent_token != state.static_config.agent_token {
        restart_required.push("token");
    }
    if reloaded.admin_password_hash != state.static_config.admin_password_hash {
        restart_required.push("admin_password_hash");
    }
    if reloaded.sqlite_busy_timeout_ms != state.static_config.sqlite_busy_timeout_ms {
        restart_required.push("sqlite_busy_timeout_ms");
    }
    if reloaded.sqlite_wal_autocheckpoint != state.static_config.sqlite_wal_autocheckpoint {
        restart_required.push("sqlite_wal_autocheckpoint");
    }
    if reloaded.heartbeat_interval_seconds != state.static_config.heartbeat_interval_seconds {
        restart_required.push("heartbeat_interval_seconds");
    }
    if reloaded.command_expire_seconds != state.static_config.command_expire_seconds {
        restart_required.push("command_expire_seconds");
    }
    if reloaded.soft_delete_retention_days != state.static_config.soft_delete_retention_days {
        restart_required.push("soft_delete_retention_days");
    }
    if reloaded.raw_retention_days != state.static_config.raw_retention_days {
        restart_required.push("raw_retention_days");
    }

    {
        let mut hot = state.hot_config.write().await;
        *hot = reloaded.hot.clone();
    }

    {
        let mut desired = state.desired_mode.write().await;
        desired.realtime_interval_seconds = reloaded.hot.realtime_interval_seconds;
    }

    if state
        .browser_subscribers
        .load(std::sync::atomic::Ordering::SeqCst)
        > 0
    {
        runtime::set_desired_mode(&state, ModeKind::Realtime)
            .await
            .map_err(|e| {
                error!(error = %e, "re-apply realtime mode after reload failed");
                ApiError::internal("reload config failed")
            })?;
    }

    let hot = state.hot_config.read().await;
    let body = ReloadConfigResponse {
        hot_config: HotConfigView {
            offline_threshold_seconds: hot.offline_threshold_seconds,
            alert_quiet_window_minutes: hot.alert_quiet_window_minutes,
            log_level: hot.log_level.clone(),
            realtime_interval_seconds: hot.realtime_interval_seconds,
        },
        restart_required_fields: restart_required,
    };

    Ok(Json(body))
}

async fn admin_issue_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdminUpgradeRequest>,
) -> Result<Json<UpgradeResponse>, ApiError> {
    auth::ensure_admin_session(&state, &headers).await?;

    if req.package_url.trim().is_empty()
        || req.sha256.trim().is_empty()
        || req.signature.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "package_url/sha256/signature are required",
        ));
    }

    let expire_seconds = req.expire_minutes.unwrap_or(10).clamp(1, 60) * 60;

    let target_ids = if let Some(ids) = req.agent_ids {
        let mut out = Vec::new();
        for id in ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        out
    } else {
        runtime::list_active_agent_ids(&state).await.map_err(|e| {
            error!(error = %e, "load active agent ids failed");
            ApiError::internal("issue upgrade failed")
        })?
    };

    if target_ids.is_empty() {
        return Err(ApiError::bad_request("no target agents"));
    }

    let mut command_ids = Vec::with_capacity(target_ids.len());
    for agent_id in target_ids {
        let payload = json!({
            "package_url": req.package_url,
            "sha256": req.sha256,
            "signature": req.signature,
            "metadata_hash": req.metadata_hash,
            "version": req.version,
        });
        let cmd_id = runtime::create_and_dispatch_command(
            &state,
            &agent_id,
            "upgrade",
            payload,
            expire_seconds,
        )
        .await
        .map_err(|e| {
            error!(error = %e, agent_id, "issue upgrade command failed");
            ApiError::internal("issue upgrade failed")
        })?;
        command_ids.push(cmd_id.to_string());
    }

    Ok(Json(UpgradeResponse {
        issued_count: command_ids.len(),
        command_ids,
    }))
}

async fn admin_backup_db(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AdminBackupRequest>,
) -> Result<Json<BackupResponse>, ApiError> {
    auth::ensure_admin_session(&state, &headers).await?;

    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let path = req.path.unwrap_or_else(|| {
        state
            .static_config
            .data_dir
            .join(format!("backup-{ts}.db"))
            .display()
            .to_string()
    });

    let conn = state.db.lock().await;
    conn.execute("VACUUM INTO ?1", params![path.clone()])
        .map_err(|e| {
            error!(error = %e, backup_path = %path, "backup sqlite failed");
            ApiError::internal("backup failed")
        })?;

    Ok(Json(BackupResponse { path }))
}

async fn admin_delete_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth::ensure_admin_session(&state, &headers).await?;

    let conn = state.db.lock().await;
    conn.execute(
        "UPDATE agents SET deleted_at = ?2 WHERE agent_id = ?1",
        params![agent_id, Utc::now().to_rfc3339()],
    )
    .map_err(|e| {
        error!(error = %e, "soft delete agent failed");
        ApiError::internal("soft delete failed")
    })?;

    store::broadcast_agent_update(
        &state,
        &json!({"type": "agent_deleted", "agent_id": agent_id}),
    );
    Ok(Json(json!({"ok": true})))
}

async fn admin_recover_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth::ensure_admin_session(&state, &headers).await?;

    let conn = state.db.lock().await;
    conn.execute(
        "UPDATE agents SET deleted_at = NULL WHERE agent_id = ?1",
        params![agent_id],
    )
    .map_err(|e| {
        error!(error = %e, "recover agent failed");
        ApiError::internal("recover failed")
    })?;

    store::broadcast_agent_update(
        &state,
        &json!({"type": "agent_recovered", "agent_id": agent_id}),
    );
    Ok(Json(json!({"ok": true})))
}
