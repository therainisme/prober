# Prober

Lightweight server monitoring system with Rust `dashboard` + `agent` binaries.

## Workspace

- `crates/dashboard`: central dashboard service (Axum + SQLite)
- `crates/agent`: lightweight host agent that reports metrics to dashboard

## Docker

```bash
docker compose up -d
```

Images are published to GHCR on every push to `main`:

- `ghcr.io/therainisme/prober-dashboard:latest`
- `ghcr.io/therainisme/prober-agent:latest`

Both support `linux/amd64` and `linux/arm64`.

## Build from source

### Prerequisites

- Rust >= 1.85 (edition 2024)

### Build

```bash
cargo build --workspace
```

### Start dashboard

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

### Start agent

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

## Config precedence

`CLI args > ENV vars > config file > defaults`

Template: `config/dashboard.toml.example`

## API reference

| Method | Path | Auth |
|--------|------|------|
| GET | `/healthz` | - |
| GET | `/api/events` (SSE) | - |
| GET | `/api/agents` | - |
| GET | `/api/agents/{id}` | - |
| GET | `/api/agents/{id}/history` | - |
| GET | `/api/agent/commands?agent_id={id}` (SSE) | token |
| POST | `/api/agent/register` | token |
| POST | `/api/agent/report` | token |
| POST | `/api/agent/command-ack` | token |
| POST | `/api/admin/login` | - |
| POST | `/api/admin/logout` | session |
| GET | `/api/admin/session` | session |
| POST | `/api/admin/reload-config` | session |
| POST | `/api/admin/upgrade` | session |
| POST | `/api/admin/backup` | session |
| POST | `/api/admin/agents/{id}/delete` | session |
| POST | `/api/admin/agents/{id}/recover` | session |

## License

MIT
