# Arbor

[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Stars](https://img.shields.io/github/stars/Billy1900/Arbor)](https://github.com/Billy1900/Arbor)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)](https://github.com/Billy1900/Arbor/pulls)

**The only agent sandbox where forking execution state is a first-class primitive.**

Arbor gives AI agent teams microVM-isolated workspaces they can snapshot mid-task, fork into parallel branches, and restore safely — all inside your own VPC. No SaaS. No credential leaks. No shared entropy between forks.

Built in Rust on top of [Firecracker](https://github.com/firecracker-microvm/firecracker).

---

## Why Arbor?

Multi-agent workflows hit three problems that no existing sandbox solves together:

**1. You can't safely fork execution state.** Firecracker's own documentation warns that restoring the same snapshot twice gives both VMs identical PRNG seeds, token caches, and SSH agent state. For multi-agent branching, that's a correctness bug, not an acceptable limitation.

**2. Credentials end up inside the VM.** The standard pattern — inject `OPENAI_API_KEY` as an environment variable — means any agent that logs its environment, runs a compromised dependency, or is manipulated by prompt injection can exfiltrate your keys.

**3. You can't run this on your own infrastructure.** Every existing coding sandbox is SaaS-only. If your codebase is proprietary, your compliance team won't allow agent traffic to touch a third-party cloud.

Arbor solves all three.

---

## Quick look

```rust
// Spin up an isolated workspace
let workspace = Arbor::new().await?;

// Run real builds inside a Firecracker microVM
workspace.run("git clone git@github.com:acme/monorepo.git .").await?;
workspace.run("cargo build --release").await?;

// Snapshot before the risky part
let checkpoint = workspace.snapshot("before-migration").await?;

// Fork three agents — each gets a fresh, independent identity
let pg    = checkpoint.fork("postgres-migration").await?;
let redis = checkpoint.fork("redis-approach").await?;
let skip  = checkpoint.fork("skip-migration").await?;

// Run in parallel — none can observe or interfere with the others
let results = join_all([
    pg.run("cargo test --test integration"),
    redis.run("cargo test --test integration"),
    skip.run("cargo test --test integration"),
]).await;
```

---

## How it compares

| | Arbor | E2B | Docker Sandboxes | Modal | Daytona |
|---|---|---|---|---|---|
| VM-level isolation | ✅ Firecracker | ✅ Firecracker | ❌ | ❌ Container | ❌ Mixed |
| Fork from checkpoint | ✅ First-class API | ❌ | ❌ | ❌ | ❌ |
| Branch-safe restore | ✅ **Unique** | ❌ | ❌ | ❌ | ❌ |
| Credential brokering | ✅ Host-side proxy | ❌ | ✅ Partial | ❌ | ❌ |
| Default-deny egress | ✅ | ✅ Partial | ✅ | ❌ | ❌ |
| Self-host / VPC-first | ✅ **First-class** | ❌ SaaS only | ❌ SaaS only | ❌ SaaS only | ✅ |
| Sub-150ms boot | ✅ | ✅ | ❌ | ✅ | ❌ |
| Open source | ✅ MIT / Rust | ❌ SDK only | ❌ | ❌ | ✅ |

**E2B** is the closest peer — Firecracker-based, agent-focused — but has no fork API, no branch-safe semantics, and is SaaS-only.

**Docker Sandboxes** pioneered brokered credentials but has no snapshot capability and no self-host option.

**Modal** has strong container checkpointing but is function-oriented, not workspace-oriented. You can't `git clone` a repo and run a multi-hour agent session in a persistent environment.

**Daytona** is self-hostable and git-native but is designed for human developers. No snapshot, no credential brokering, no egress policy, no agent API.

---

## Core differentiators

### 1. Branch-safe restore

Firecracker [explicitly warns](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md) that restoring the same checkpoint twice produces VMs with identical PRNG seeds. For multi-agent experiments, this is a correctness bug — two forks will generate the same tokens, nonces, and session IDs.

Arbor solves this with a **quarantine + reseal** protocol:

```
fork(checkpoint_id)
 └─ new VM boots in QUARANTINED state
     ├─ all egress blocked
     ├─ all attach tokens invalidated
     └─ reseal hook chain runs:
         1. bump identity_epoch  →  new VM identity
         2. rotate session tokens
         3. re-sign preview URLs
         4. revoke + re-issue secret grants
         5. re-seed guest entropy via vsock
         ─────────────────────────────────
         only then: state → READY
```

This is enforced at the infrastructure level. No application-level coordination required.

### 2. VPC-first credential brokering

The VM **never receives your API keys**. When an agent calls `api.openai.com`:

```
agent process
  → VM netns (blocked by default)
  → host TAP device
  → arbor-egress-proxy
      ├─ allowlist check
      ├─ credential injection (Authorization: Bearer <real-key>)
      └─ upstream request to api.openai.com
```

The agent sees `OPENAI_API_KEY=arbor-brokered` in its environment. The real key lives only in host memory. If the agent logs its environment, leaks it to a compromised dependency, or is manipulated by prompt injection — the real key was never there.

### 3. Checkpoint DAG

Every checkpoint records its parent, forming a directed acyclic graph of execution history:

```
ws-main ──ckpt-A "before-migration"
              ├── ws-attempt-1  (fork: postgres path)
              ├── ws-attempt-2  (fork: redis approach)
              └── ws-attempt-3  (fork: skip migration)
```

Each fork has its own isolated identity, its own Docker daemon, its own egress policy, and its own secret grants. The parent workspace keeps running. None of the attempts can observe each other.

### 4. Structural security, not policy security

Each workspace lives in its own Linux network namespace. The TAP device for Firecracker lives inside that netns. Traffic flows through a veth pair to the host, where nftables enforces the allowlist and the egress proxy handles credential injection. A VM **cannot** bypass the egress policy — there is no route out except through the proxy. This is structural, not configurable.

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    arbor-api / arbor-agent               │
│          Branch lifecycle · Snapshot/restore · Fork      │
└────────────────────────┬────────────────────────────────┘
                         │ workspace operations
         ┌───────────────┼───────────────┐
         ▼               ▼               ▼
   ┌──────────┐    ┌──────────┐    ┌──────────┐
   │ Branch A │    │ Branch B │    │ Branch C │
   │ FC + Jailer    FC + Jailer    FC + Jailer│
   │ vsock mux│    │ vsock mux│    │ vsock mux│
   └──────────┘    └──────────┘    └──────────┘
         │               │               │
         └───────────────┼───────────────┘
                         │ host isolation
┌────────────────────────┴────────────────────────────────┐
│  arbor-egress-proxy  │  arbor-snapshot  │  arbor-secret  │
│  Allowlist · Inject  │  DAG · S3/MinIO  │  broker/Vault  │
└─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Role |
|---|---|
| `arbor-api` | REST API, WebSocket PTY attach (axum) |
| `arbor-controller` | Workspace state machine, fork/restore orchestration (sqlx/postgres) |
| `arbor-runner-agent` | Firecracker + Jailer lifecycle, netns, vsock multiplexer |
| `arbor-guest-agent` | Static musl binary inside VM: PTY exec, port scan, quiesce |
| `arbor-snapshot` | Checkpoint manifest, S3/MinIO upload, sha256 integrity |
| `arbor-egress-proxy` | CONNECT proxy, allowlist enforcement, credential injection (hyper) |
| `arbor-secret-broker` | Grant lifecycle, Vault integration |
| `arbor-common` | Shared types, vsock frame protocol, error codes |

---

## Get started in 5 minutes

### Prerequisites

- Linux host with KVM (`/dev/kvm` accessible)
- Docker + Docker Compose
- [Firecracker + Jailer binaries](https://github.com/firecracker-microvm/firecracker/releases) (`v1.9.0`)

### Docker Compose (recommended for development)

```bash
git clone https://github.com/Billy1900/Arbor && cd Arbor

# Copy and fill in your config
cp deploy/.env.example deploy/.env

# Start postgres, MinIO, API, and runner
make docker-up

# Register this machine as a runner node
make register-dev-runner

# Run the fork demo end-to-end
make demo-fork
```

Services:
- API: `http://localhost:8080`
- Metrics: `http://localhost:8080/metrics`
- MinIO console: `http://localhost:9001`

### Single-node (manual)

```bash
# Download Firecracker binaries
make firecracker-bins  # installs to /var/lib/arbor/firecracker/bin/

# Build the guest agent (static musl binary for the rootfs)
make guest-agent

# Build the Ubuntu 24.04 guest rootfs (requires root + debootstrap)
sudo make image

# Start services
ARBOR__DATABASE_URL=$DATABASE_URL \
ARBOR__ATTACH_TOKEN_SECRET=$(openssl rand -hex 32) \
  ./target/release/arbor-api &

./target/release/arbor-runner-agent &
```

---

## API reference

### Workspace lifecycle

```bash
BASE=http://localhost:8080

# Create a workspace
curl -X POST $BASE/v1/workspaces \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "fix-auth-bug",
    "repo": {
      "provider": "github",
      "url": "git@github.com:acme/monorepo.git",
      "ref": "refs/heads/main"
    },
    "runtime": {
      "runner_class": "fc-x86_64-v1",
      "vcpu_count": 4,
      "memory_mib": 4096,
      "disk_gb": 40
    },
    "image": { "base_image_id": "ubuntu-24.04-dev-v1" },
    "network": { "egress_policy": "default-deny" }
  }'

# Execute a command (returns session_id)
curl -X POST $BASE/v1/workspaces/{ws_id}/exec \
  -d '{ "command": ["cargo", "test"], "pty": false }'

# Open a PTY shell
curl -X POST $BASE/v1/workspaces/{ws_id}/exec \
  -d '{ "command": ["bash", "-l"], "pty": true }'
# → attach via: wss://host/v1/attach/{sess_id}?token=...
```

### Checkpoint and fork

```bash
# Take a checkpoint
curl -X POST $BASE/v1/workspaces/{ws_id}/checkpoints \
  -d '{ "name": "before-migration", "mode": "full_vm" }'

# Fork into a parallel branch (quarantine + reseal enforced automatically)
curl -X POST $BASE/v1/checkpoints/{ckpt_id}/fork \
  -d '{
    "branch_name": "postgres-attempt",
    "post_restore": { "quarantine": true, "identity_reseal": true }
  }'

# Restore a checkpoint into a new workspace
curl -X POST $BASE/v1/checkpoints/{ckpt_id}/restore \
  -d '{ "workspace_name": "restored-ws" }'

# List all checkpoints for a workspace
curl $BASE/v1/workspaces/{ws_id}/checkpoints
```

### Secret grants (credentials never enter the VM)

```bash
# Bind an API key — agent sees a placeholder, proxy injects the real value
curl -X PUT $BASE/v1/workspaces/{ws_id}/secrets/grants/{grant_id} \
  -d '{
    "provider": "openai",
    "mode": "brokered_proxy",
    "vault_ref": "vault://prod/openai-key",
    "allowed_hosts": ["api.openai.com"],
    "ttl_seconds": 3600
  }'

# Revoke a grant
curl -X DELETE $BASE/v1/workspaces/{ws_id}/secrets/grants/{grant_id}
```

### Workspace state machine

```
creating → ready ⟷ running → checkpointing → ready
                           ↘ terminating  → terminated
        (fork/restore) → restoring → quarantined → ready
```

---

## Key design decisions

**CPU template:** Uses `T2` (Intel x86_64), not `T2A` (ARM/Graviton2). Firecracker requires the CPU template to match between snapshot creation and restore. This is enforced via the `compatibility_key` stored in every checkpoint manifest. Mismatches return `RUNNER_CLASS_INCOMPATIBLE` before any restore is attempted.

**Full VM snapshots only:** Firecracker's diff snapshot support is still developer preview. All checkpoints are full VM snapshots. Incremental support is on the roadmap for M7 once Firecracker GA lands.

**Memory file lifecycle:** After restore, Firecracker maps guest memory from the mem snapshot file via `MAP_PRIVATE`. That file must remain accessible for the entire VM lifetime. Arbor keeps a hot copy on local NVMe for active VMs and fetches from object storage for cold restores.

**Egress via netns:** Each workspace gets its own Linux network namespace. The TAP device for Firecracker lives inside the netns. Traffic flows through a veth pair to the host, where nftables enforces the allowlist and the egress proxy handles credential injection. There is no route out of a VM except through the proxy — this is a physical constraint, not a configuration option.

**Workspace identity per-header (MVP):** The egress proxy currently identifies the source workspace via an `X-Arbor-Workspace-Id` header. Production deployments should replace this with a cryptographic binding between TAP interface MAC and workspace ID in the runner registry.

---

## Roadmap

| Milestone | Feature | Status |
|---|---|---|
| M1 | Single-node create / exec / terminate | ✅ Complete |
| M2 | Guest rootfs + private Docker daemon | ✅ Complete |
| M3 | Full VM checkpoint + S3 upload | ✅ Complete |
| M4 | Branch-safe fork: quarantine + reseal | ✅ Complete |
| M5 | Secret Broker + Egress Proxy | ✅ Complete |
| M6 | Multi-runner pool + Prometheus + Helm | 🔄 In progress |
| M7 | Diff snapshots (Firecracker GA) | 📋 Planned |
| M8 | ARM64 runner class | 📋 Planned |
| M9 | GPU passthrough runner | 📋 Planned |

---

## Contributing

```bash
# Check without a live database
SQLX_OFFLINE=true cargo check --workspace

# Unit tests (no DB required)
make test-unit

# Integration tests (requires postgres)
make test-integration

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all
```

High-value contribution areas:

- **Integration tests** for the full fork + reseal flow
- **Python and TypeScript SDKs** (currently raw HTTP only)
- **Multi-runner heartbeat + drain protocol** (M6)
- **Prometheus metrics** in `arbor-runner-agent`
- **Vault / AWS Secrets Manager** backend for `arbor-secret-broker` (currently env-var based)

---

## Security model summary

| Threat | Mitigation |
|---|---|
| Agent exfiltrates API key via env | Key never enters VM — proxy injects at egress |
| Agent escapes via kernel exploit | Firecracker microVM + Jailer seccomp/cgroup isolation |
| Two forks share PRNG state | Quarantine + reseal: entropy re-seeded via vsock before READY |
| Agent bypasses egress allowlist | No route exists except through proxy — physically impossible |
| Snapshot restored with stale credentials | Reseal revokes and re-issues all grants before READY |
| Supply-chain attack reads env secrets | Real keys never in VM process environment |

---

## License

MIT. See [LICENSE](LICENSE).