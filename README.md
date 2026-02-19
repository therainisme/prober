# Prober

Lightweight server probe system with Rust `dashboard` + `agent` binaries.

## Workspace

- `crates/dashboard`: central dashboard service (Axum + SQLite)
- `crates/agent`: lightweight host agent that reports metrics to dashboard

## Prerequisites

- Rust >= 1.85 (edition 2024)

## Quick start

1. Build

```bash
cargo build --workspace
```

2. Start dashboard

```bash
cargo run -p dashboard -- \
  --listen 0.0.0.0:8080 \
  --token change-me-token \
  --admin-password change-me-admin
```

Or with a config file:

```bash
cp config/dashboard.toml.example config/dashboard.toml
# edit config/dashboard.toml
cargo run -p dashboard -- --config config/dashboard.toml
```

3. Start agent

```bash
cargo run -p agent -- \
  --dashboard http://127.0.0.1:8080 \
  --token change-me-token \
  --id local-dev-1 \
  --name "Local Dev 1"
```

Or via environment variables:

```bash
PROBER_DASHBOARD_URL=http://127.0.0.1:8080 \
PROBER_TOKEN=change-me-token \
PROBER_AGENT_ID=local-dev-1 \
PROBER_AGENT_NAME="Local Dev 1" \
cargo run -p agent
```

4. Health check

```bash
curl --noproxy "*" http://127.0.0.1:8080/healthz
```

5. Open dashboard UI

Open `http://127.0.0.1:8080/` in browser.

## Config precedence

Dashboard config precedence:

`CLI args > ENV vars > config file > defaults`

Template config: `config/dashboard.toml.example`.

## Implemented scope

- Dashboard HTTP + SSE APIs
- SQLite WAL + runtime pragmas + background maintenance
- Agent token auth (`Authorization: Bearer <token>`)
- Admin login/session via HttpOnly cookie + IP rate limit (5/min)
- Agent APIs: register/report/command-ack/commands(SSE)
- Command model: `command_id + issued_at + expire_at`, retry + ACK state tracking
- Agent command dedupe cache (1000 items, >=24h)
- Realtime/heartbeat mode switch driven by dashboard subscribers
- Single-agent report rate limit (1 req/s, burst 3)
- Clock-skew tagging (`clock_skewed`)
- Soft delete + recover + scheduled hard delete
- Retention/downsampling jobs (raw/5m/1h tiers)
- Telegram offline/recovery alerts + debounce + quiet window
- SQLite hot backup API (`POST /api/admin/backup`)
- Embedded frontend with realtime list + 24h area charts + light/dark theme + responsive layout
- Agent upgrade execution path with SHA256 + Ed25519 verification + rollback-aware binary swap

## Upgrade public key

Agent upgrade verification requires:

- `PROBER_UPGRADE_PUBLIC_KEY` (base64 Ed25519 public key)

Without this key, upgrade commands are rejected by agent.

## API quick reference

- `GET /healthz`
- `GET /api/events` (SSE)
- `GET /api/agents`
- `GET /api/agents?include_deleted=true`
- `GET /api/agents/{agent_id}`
- `GET /api/agents/{agent_id}/history`
- `GET /api/agent/commands?agent_id={id}` (SSE)
- `POST /api/agent/register`
- `POST /api/agent/report`
- `POST /api/agent/command-ack`
- `POST /api/admin/login`
- `POST /api/admin/logout`
- `GET /api/admin/session`
- `POST /api/admin/reload-config`
- `POST /api/admin/upgrade`
- `POST /api/admin/backup`
- `POST /api/admin/agents/{agent_id}/delete`
- `POST /api/admin/agents/{agent_id}/recover`
