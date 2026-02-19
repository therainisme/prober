use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad_request",
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: "unauthorized",
            message: message.into(),
        }
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            code: "too_many_requests",
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "not_found",
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: "conflict",
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal_error",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code {
            "bad_request" => StatusCode::BAD_REQUEST,
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "too_many_requests" => StatusCode::TOO_MANY_REQUESTS,
            "not_found" => StatusCode::NOT_FOUND,
            "conflict" => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentRegisterRequest {
    pub agent_id: String,
    pub display_name: Option<String>,
    pub agent_version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentReportRequest {
    pub agent_id: String,
    pub boot_id: String,
    pub seq: u64,
    pub collected_at: DateTime<Utc>,
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct CommandAckRequest {
    pub agent_id: String,
    pub command_id: Uuid,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AdminLoginRequest {
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentListQuery {
    pub include_deleted: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AgentCommandQuery {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AdminUpgradeRequest {
    pub agent_ids: Option<Vec<String>>,
    pub package_url: String,
    pub sha256: String,
    pub signature: String,
    pub metadata_hash: Option<String>,
    pub version: Option<String>,
    pub expire_minutes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AdminBackupRequest {
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReloadConfigResponse {
    pub hot_config: HotConfigView,
    pub restart_required_fields: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct HotConfigView {
    pub offline_threshold_seconds: u64,
    pub alert_quiet_window_minutes: u64,
    pub log_level: String,
    pub realtime_interval_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct BackupResponse {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct UpgradeResponse {
    pub issued_count: usize,
    pub command_ids: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct MetricSnapshot {
    pub cpu_usage_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub disk_used_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AgentView {
    pub agent_id: String,
    pub display_name: Option<String>,
    pub agent_version: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub deleted_at: Option<String>,
    pub online: bool,
    pub metrics: Option<MetricSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct AgentsResponse {
    pub agents: Vec<AgentView>,
}

#[derive(Debug, Serialize)]
pub struct AgentHistoryPoint {
    pub timestamp: String,
    pub clock_skewed: bool,
    pub metrics: MetricSnapshot,
}

#[derive(Debug, Serialize)]
pub struct AgentHistoryResponse {
    pub agent_id: String,
    pub from: String,
    pub to: String,
    pub points: Vec<AgentHistoryPoint>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutboundCommand {
    pub command_id: Uuid,
    #[serde(rename = "type")]
    pub command_type: String,
    pub issued_at: DateTime<Utc>,
    pub expire_at: DateTime<Utc>,
    pub payload: Value,
}
