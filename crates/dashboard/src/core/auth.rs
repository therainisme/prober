use std::collections::HashMap;

use axum::http::{HeaderMap, header};
use chrono::Utc;

use crate::core::models::ApiError;
use crate::core::state::{ADMIN_SESSION_COOKIE, AdminSession, AppState};

pub fn ensure_agent_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(token) = parse_bearer_token(headers) else {
        return Err(ApiError::unauthorized("missing bearer token"));
    };
    if token != state.static_config.agent_token {
        return Err(ApiError::unauthorized("invalid token"));
    }
    Ok(())
}

pub async fn ensure_admin_session(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(session_id) = read_cookie(headers, ADMIN_SESSION_COOKIE) else {
        return Err(ApiError::unauthorized("missing admin session"));
    };

    let mut sessions = state.sessions.lock().await;
    prune_expired_sessions(&mut sessions);
    if sessions.contains_key(&session_id) {
        Ok(())
    } else {
        Err(ApiError::unauthorized("admin session expired"))
    }
}

pub fn prune_expired_sessions(sessions: &mut HashMap<String, AdminSession>) {
    let now = Utc::now();
    sessions.retain(|_, session| session.expires_at > now);
}

pub fn build_session_cookie(session_id: &str, max_age_seconds: i64) -> String {
    format!(
        "{ADMIN_SESSION_COOKIE}={session_id}; HttpOnly; Path=/; SameSite=Lax; Max-Age={max_age_seconds}"
    )
}

pub fn clear_session_cookie() -> String {
    format!("{ADMIN_SESSION_COOKIE}=deleted; HttpOnly; Path=/; SameSite=Lax; Max-Age=0")
}

pub fn read_cookie(headers: &HeaderMap, key: &str) -> Option<String> {
    let all = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in all.split(';') {
        let (name, value) = part.trim().split_once('=')?;
        if name == key {
            return Some(value.to_string());
        }
    }
    None
}

pub fn parse_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

pub fn parse_client_ip(headers: &HeaderMap) -> String {
    if let Some(value) = headers.get("x-forwarded-for")
        && let Ok(raw) = value.to_str()
        && let Some(first) = raw.split(',').next()
    {
        let ip = first.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    if let Some(value) = headers.get("x-real-ip")
        && let Ok(ip) = value.to_str()
    {
        let ip = ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{parse_client_ip, read_cookie};

    #[test]
    fn parse_ip_prefers_x_forwarded_for_first_entry() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.1, 10.0.0.2"),
        );
        assert_eq!(parse_client_ip(&headers), "203.0.113.1");
    }

    #[test]
    fn read_cookie_extracts_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; prober_session=abc123; b=2"),
        );
        assert_eq!(
            read_cookie(&headers, "prober_session"),
            Some("abc123".to_string())
        );
    }
}
