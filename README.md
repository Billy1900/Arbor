# Arbor — Branch-Native Coding Workspace Infrastructure

Checkpoint-native, VPC-first coding workspace platform built in Rust.

## Architecture

```
arbor-api            REST + WebSocket gateway (axum)
arbor-controller     Workspace state machine + scheduler (sqlx/postgres)
arbor-runner-agent   Firecracker/Jailer orchestrator + vsock mux
arbor-guest-agent    In-VM agent — PTY exec, port scan (musl static binary)
arbor-snapshot       Checkpoint manifest + S3 upload (object_store)
arbor-egress-proxy   Allowlist CONNECT proxy + credential injection (hyper)
arbor-secret-broker  Grant lifecycle — brokered_proxy / ephemeral_env
arbor-common         Shared types, vsock frame protocol, errors
```

## Core Differentiators

- **Branch-safe restore**: fork from any checkpoint, quarantine + reseal before
  opening egress — prevents entropy/token collision across multi-restore
- **VPC-first**: secrets never leave your network; egress proxy injects
  credentials at the host, not inside the VM
- **Checkpoint DAG**: `POST /v1/checkpoints/{id}/fork` creates isolated
  workspaces from a common snapshot for parallel agent experiments

More details are in the "Why build Arbor?" section of the [INTRO.md](INTRO.md) document.

## Quick Start (Rust 1.82+)

```bash
# 1. Prerequisites
apt install -y postgresql nftables iproute2
# Download Firecracker + Jailer from:
# https://github.com/firecracker-microvm/firecracker/releases
# Place binaries at /var/lib/arbor/firecracker/bin/{firecracker,jailer}
# Download kernel image to /var/lib/arbor/firecracker/vmlinux

# 2. Build
export DATABASE_URL=postgresql://arbor:password@localhost/arbor
createdb arbor
cargo sqlx prepare --workspace  # generates .sqlx/ compile-time cache
cargo build --release

# 3. Build guest rootfs (requires root + debootstrap)
sudo bash images/ubuntu-24.04-dev/build.sh
# Also build static guest-agent and embed in rootfs:
cargo build --release --target x86_64-unknown-linux-musl -p arbor-guest-agent
# Copy to rootfs at /usr/local/bin/arbor-guest-agent

# 4. Register a runner node in PostgreSQL
psql $DATABASE_URL -c "INSERT INTO runner_nodes
  (id, runner_class, address, arch, firecracker_version, cpu_template, capacity_slots)
  VALUES (gen_random_uuid(), 'fc-x86_64-v1', 'http://localhost:9090',
          'x86_64', '1.9.0', 'T2', 10);"

# 5. Start services
ARBOR__DATABASE_URL=$DATABASE_URL \
ARBOR__ATTACH_TOKEN_SECRET=change-me \
  ./target/release/arbor-api &

./target/release/arbor-runner-agent &
./target/release/arbor-egress-proxy &
```

## API Usage

```bash
BASE=http://localhost:8080

# Create workspace
curl -s -X POST $BASE/v1/workspaces \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "my-repo",
    "repo": {"provider":"github","url":"git@github.com:org/repo.git","ref":"refs/heads/main"},
    "runtime": {"runner_class":"fc-x86_64-v1","vcpu_count":2,"memory_mib":2048,"disk_gb":20},
    "image": {"base_image_id":"ubuntu-24.04-dev-v1"},
    "network": {"egress_policy":"default-deny"}
  }'
# -> {"workspace_id":"ws_...","operation_id":"op_...","state":"creating"}

# Poll until ready
curl -s $BASE/v1/operations/{op_id}

# Open a PTY shell
curl -s -X POST $BASE/v1/workspaces/{ws_id}/exec \
  -d '{"command":["bash","-l"],"pty":true}'
# -> {"session_id":"sess_...","attachable":true}

# Get WebSocket attach URL
curl -s -X POST $BASE/v1/sessions/{sess_id}/attach
# -> {"transport":"websocket","url":"wss://...?token=..."}

# Take a checkpoint
curl -s -X POST $BASE/v1/workspaces/{ws_id}/checkpoints \
  -d '{"name":"before-migration","mode":"full_vm"}'

# Fork — creates an isolated copy with fresh identity/tokens
curl -s -X POST $BASE/v1/checkpoints/{ckpt_id}/fork \
  -d '{"branch_name":"attempt-b","workspace_name":"repo-attempt-b",
       "post_restore":{"quarantine":true,"identity_reseal":true}}'
```

## Milestone Roadmap

| M | Feature | Status |
|---|---------|--------|
| M1 | Single-node: create/exec/terminate | Code complete |
| M2 | Guest image + private Docker daemon | Build script ready |
| M3 | Full VM checkpoint + restore | Code complete |
| M4 | Branch-safe quarantine + reseal | Code complete |
| M5 | Secret Broker + Egress Proxy | Code complete |
| M6 | Multi-runner pool + ops hardening | Compose/Helm ready |

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `ARBOR__DATABASE_URL` | — | PostgreSQL connection string |
| `ARBOR__BIND` | `0.0.0.0:8080` | API server listen address |
| `ARBOR__ATTACH_TOKEN_SECRET` | `change-me` | HMAC secret for attach tokens |
| `ARBOR__API_BASE_URL` | `http://localhost:8080` | Public base URL |
| `ARBOR_RUNNER__BIND` | `0.0.0.0:9090` | Runner agent listen address |
| `ARBOR_PROXY_BIND` | `0.0.0.0:3128` | Egress proxy listen address |
| `SQLX_OFFLINE` | — | Set to `true` to skip live DB at compile time |

## Key Design Decisions

**CPU template**: uses `T2` (x86_64 Intel), not `T2A` (which is ARM/Graviton2).
Snapshot compatibility requires runner class, FC version, and CPU template to
match exactly between checkpoint creation and restore.

**Diff snapshots**: not used — Firecracker still marks them as developer preview.
All checkpoints are full VM snapshots.

**Memory file lifecycle**: Firecracker uses `MAP_PRIVATE` to load guest memory
from the mem file. The mem file must remain available for the entire VM lifetime
after restore — it is cached on NVMe and never deleted while the VM is running.

**Reseal hooks**: after any fork/restore, the workspace enters `quarantined`
state. Egress, attach, and secret grants are all blocked until the reseal hook
chain completes (identity_epoch bump, token rotation, preview URL re-sign).
This prevents entropy/token duplication when the same snapshot is restored
multiple times.
