# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Check / build
make check          # cargo check --workspace (SQLX_OFFLINE=true, no DB needed)
make build          # release build

# Tests
make test-unit      # unit tests (no DB): safety + reseal suites
make test-api-smoke # API smoke tests (no DB)
make test-integration  # DB integration tests (needs PostgreSQL running)

# Run M6 unit tests only (no DB)
SQLX_OFFLINE=true cargo test -p arbor-tests --test m6_smoke
SQLX_OFFLINE=true cargo test -p arbor-runner-agent  # drain flag tests

# Code quality
make fmt            # cargo fmt --all
make lint           # clippy --workspace -D warnings

# Database (postgres at postgresql://arbor:arbor_dev_only@localhost/arbor)
make db-setup       # create DB + run migrations
make db-reset       # drop + recreate DB

# Infrastructure
make docker-up      # start postgres + minio + services via compose
make guest-agent    # build static musl arbor-guest-agent binary
make firecracker-bins  # download Firecracker v1.9.0 + jailer to /var/lib/arbor/firecracker/bin

# Helm (production deploy — control plane only)
helm install arbor deploy/helm/arbor --namespace arbor --create-namespace \
  --set api.config.databaseUrl="postgresql://..." \
  --set api.config.attachTokenSecret="$(openssl rand -hex 32)"
```

All `cargo` commands require `SQLX_OFFLINE=true` unless a live DB is present.

## Architecture

Arbor is a Rust workspace that provides Firecracker-microVM-backed agent sandboxes with snapshot/fork/reseal semantics.

### Crates

| Crate | Role |
|---|---|
| `arbor-common` | Shared types (`types.rs`), protobuf-style proto structs (`proto.rs`), error types |
| `arbor-api` | Axum HTTP API (port 8080). Routes under `src/routes/`: workspaces, checkpoints/fork, sessions, runners. Talks to controller via `runner_client`. Manages DB state. Exposes `GET /metrics`. |
| `arbor-controller` | Core lifecycle logic: `state_machine.rs` (workspace states), `scheduler.rs` (assigns workspaces to runners), `reseal.rs` (branch-safe restore protocol), `snapshot.rs`, `grant_registry.rs` |
| `arbor-runner-agent` | Runs on each bare-metal KVM host. Self-registers with the API on startup, sends heartbeats every 15s, manages VM lifecycle (Firecracker + Jailer), vsock, PTY sessions. Exposes `GET /metrics` and `PUT /drain`. |
| `arbor-guest-agent` | Static musl binary embedded in guest rootfs. Provides vsock-based exec/file/attach interface inside the VM |
| `arbor-egress-proxy` | Host-side transparent proxy enforcing egress policy (default-deny with allowlist) and credential brokering |
| `arbor-secret-broker` | Issues and revokes secret grants; proxies credential requests from guest to host-side secrets |
| `arbor-snapshot` | Snapshot create/restore helpers wrapping Firecracker snapshot API |
| `arbor-tests` | Integration + unit test suites (`m6_smoke`, `runner_pool_tests`, `api_smoke`, `db_tests`, `reseal`, `safety`) |

### Key flows

**Fork / reseal**: When a checkpoint is forked, the new VM boots in `QUARANTINED` state (all egress blocked, attach tokens invalidated). The reseal hook chain then runs: bump `identity_epoch`, rotate session tokens, re-sign preview URLs, revoke+re-issue secret grants, re-seed guest entropy via vsock. Only after all hooks pass does the VM move to `READY`. This is Arbor's core correctness guarantee — see `arbor-controller/src/reseal.rs`.

**Credential brokering**: Secrets are never injected as env vars inside VMs. The egress proxy intercepts outbound API calls and injects credentials host-side, so guests never hold raw keys.

**State machine**: Workspace states live in `arbor-controller/src/state_machine.rs`. Transitions are driven by the API and runner-agent heartbeats.

**Runner pool (M6)**: Runners self-register via `POST /internal/runners/register` on startup. The scheduler (`arbor-controller/src/scheduler.rs`) picks the healthy runner with the lowest `used_slots`. For restores it also filters by `firecracker_version` + `cpu_template` — mismatches return `RUNNER_CLASS_INCOMPATIBLE`. The API marks any runner that misses heartbeats for >60s as unhealthy via a background sweep in `arbor-api/src/main.rs`.

**Drain (M6)**: `PUT /internal/runners/{id}/drain` on the API marks the runner unhealthy (stops placements). `PUT /drain` on the runner-agent sets an `AtomicBool` drain flag that rejects new `POST /vms` with 503. On `SIGTERM` the agent sets the flag and waits up to 60s for `active_vm_count() == 0` before exiting.

**Prometheus metrics (M6)**: Both services expose `GET /metrics`. Runner-agent metrics: `arbor.runner.active_vms` (gauge), `arbor.runner.vm_boots_total`, `arbor.runner.vm_boot_duration_seconds` (histogram), `arbor.runner.checkpoints_total`, `arbor.runner.restores_total`. API metrics: `arbor.checkpoint.created`, `arbor.fork.created`, `arbor.restore.completed`, `arbor.runner.unhealthy`, `arbor.runner.drain_initiated`.

### Runner-agent environment variables

| Variable | Default | Description |
|---|---|---|
| `ARBOR_RUNNER__BIND` | `0.0.0.0:9090` | HTTP listen address |
| `ARBOR_RUNNER__CONTROLLER_URL` | _(unset)_ | API base URL for registration + heartbeat. If unset, agent runs in dev mode (no registration). |
| `ARBOR_RUNNER__ADVERTISE_ADDRESS` | `http://localhost:<port>` | Address the controller uses to reach this agent |
| `ARBOR_RUNNER__RUNNER_CLASS` | `fc-x86_64-v1` | Runner class label |
| `ARBOR_RUNNER__CAPACITY_SLOTS` | `10` | Max concurrent VMs |
| `ARBOR_RUNNER__FIRECRACKER_VERSION` | `1.9.0` | Reported FC version (must match binary) |
| `ARBOR_RUNNER__CPU_TEMPLATE` | `T2` | CPU template (T2 for x86_64) |

### Database

PostgreSQL via `sqlx`. Migrations are in `migrations/`. The `.sqlx/` directory holds offline query metadata (`cargo sqlx prepare --workspace`). Always regenerate after schema changes with `make db-prepare`.

### Deploy

**Development**: `deploy/docker-compose.yml` brings up postgres, minio, and the service binaries. Copy `deploy/.env.example` → `deploy/.env` before running `make docker-up`.

**Production (Kubernetes)**: `deploy/helm/arbor/` is a Helm chart for the control plane (`arbor-api` + `arbor-egress-proxy`). Runner agents run as systemd services on bare-metal KVM hosts and are not part of the chart. Required values: `api.config.databaseUrl`, `api.config.attachTokenSecret`.
