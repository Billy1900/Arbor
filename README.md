# Arbor

[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Stars](https://img.shields.io/github/stars/Billy1900/Arbor)](https://github.com/Billy1900/Arbor)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)](https://github.com/Billy1900/Arbor/pulls)

**Checkpoint-native rollout infrastructure for agentic RL.**

Arbor is the execution layer for branching coding-agent rollouts: snapshot mid-task, fork into parallel attempts, replay any failure, collect trajectories for training — all inside your own VPC. No SaaS. No credential leaks. No shared entropy between forks.

Built in Rust on top of [Firecracker](https://github.com/firecracker-microvm/firecracker).

> *Time-travel infrastructure for coding agents. Fork, replay, and train from real execution states.*

---

## Why Arbor?

Agentic RL over coding tasks — SWE-bench, internal bug benchmarks, multi-step refactors — hits three problems no existing sandbox solves together:

**1. You can't safely branch rollouts.** Running the same checkpoint twice gives both VMs identical PRNG seeds, session tokens, and SSH state (Firecracker's own docs warn about this). For RL rollouts where you need N independent attempts from the same start state, that's a correctness bug that silently poisons your reward signal.

**2. You have no trajectory visibility.** You can see whether a test passed, but not which tool call introduced the regression, which step wasted tokens on a wrong hypothesis, or which fork's approach was actually better. Without per-step traces tied to execution state, attribution and replay are impossible.

**3. Your training data can't leave your VPC.** Every existing coding sandbox is SaaS-only. Proprietary codebases, internal APIs, and the trajectories you're collecting for fine-tuning can't touch a third-party cloud.

Arbor solves all three.

---

## Quick look

```bash
# Run a SWE-bench-style rollout: fork 8 independent attempts from one checkpoint
arbor run-benchmark swebench-lite \
  --models claude-opus-4,claude-sonnet-4 \
  --forks 8 \
  --checkpoint repo@HEAD \
  --reward "cargo test --test integration"

# → spins up 8 isolated microVMs from the same snapshot
# → each attempt gets fresh identity, entropy, credentials
# → traces collected: prompt · tool calls · shell cmds · file diffs · cost · wall time
# → success/failure attributed per step
# → winning patch exported; failing attempts available for replay
```

Or drive it directly from the API:

```rust
// Snapshot the repo at a known-broken state
let workspace = Arbor::new().repo("git@github.com:acme/monorepo.git").await?;
let checkpoint = workspace.snapshot("bug-repro").await?;

// Fork 8 isolated rollouts — each gets a fresh identity, entropy, and secret grants
let attempts: Vec<_> = (0..8)
    .map(|i| checkpoint.fork(format!("attempt-{i}")))
    .collect::<FuturesOrdered<_>>()
    .collect()
    .await;

// Run agents in parallel — none can observe or interfere with each other
let results = join_all(
    attempts.iter().map(|ws| ws.run("cargo test --test integration"))
).await;

// Export the winning trajectory for fine-tuning; replay the failures
let winner = results.iter().find(|r| r.exit_code == 0);
winner.map(|r| r.export_trajectory("s3://my-bucket/trajectories/"));
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
| Multi-runner pool + Helm | ✅ | ❌ | ❌ | ✅ Partial | ✅ Partial |
| ARM64 / Graviton2 | ✅ `fc-arm64-v1` | ❌ | ❌ | ✅ | ❌ |
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

### 4. Trajectory tracer (M10)

The checkpoint DAG is the execution graph. M10 adds a trace layer that attaches to every fork:

```
fork(checkpoint_id)
 └─ attempt-0: ✅ tests pass  · 47 tool calls · $0.23 · 4m12s
     ├─ step 14: patch src/auth.rs  ← reward-contributing diff
     └─ trajectory exported → s3://bucket/trajectories/attempt-0.jsonl

 └─ attempt-1: ❌ tests fail  · 61 tool calls · $0.31 · 6m08s
     ├─ step 22: introduced regression in src/db.rs  ← failure attribution
     └─ replay available: arbor replay attempt-1 --from-step 20
```

Per-fork trace contents: prompt, tool calls, shell commands, file diffs, test results, network accesses, token cost, wall time. Cross-fork diff shows exactly where strategies diverged. Reward attribution traces which steps contributed to the final test result. Successful trajectories export as JSONL for fine-tuning.

This turns Arbor from an infra repo into an **agentic RL experimentation platform**: rollouts, reward, trajectory, fork, replay — all in one place.

### 5. Structural security, not policy security

Each workspace lives in its own Linux network namespace. The TAP device for Firecracker lives inside that netns. Traffic flows through a veth pair to the host, where nftables enforces the allowlist and the egress proxy handles credential injection. A VM **cannot** bypass the egress policy — there is no route out except through the proxy. This is structural, not configurable.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        arbor-api                             │
│   Workspace lifecycle · Scheduler · Checkpoint DAG · Auth    │
│                    GET /metrics (Prometheus)                  │
└──────────────────┬───────────────────────────────────────────┘
                   │ runner pool (HTTP)
      ┌────────────┼─────────────┐
      ▼            ▼             ▼
┌──────────┐ ┌──────────┐ ┌──────────┐
│ runner-1 │ │ runner-2 │ │ runner-N │  ← bare-metal KVM hosts
│ FC+Jailer│ │ FC+Jailer│ │ FC+Jailer│  ← heartbeat every 15s
│ /metrics │ │ /metrics │ │ /metrics │  ← drain via PUT /drain
└────┬─────┘ └────┬─────┘ └────┬─────┘
     │             │             │
  VMs/workspaces per runner (capacity_slots)
     │
┌────┴──────────────────────────────────┐
│  arbor-egress-proxy  │ arbor-snapshot  │
│  Allowlist · Inject  │ DAG · S3/MinIO  │
└───────────────────────────────────────┘
```

### Crates

| Crate | Role |
|---|---|
| `arbor-api` | REST API, WebSocket PTY attach, runner pool management (axum) |
| `arbor-controller` | Workspace state machine, scheduler, fork/restore orchestration (sqlx/postgres) |
| `arbor-runner-agent` | Firecracker + Jailer lifecycle, netns, vsock multiplexer, Prometheus metrics |
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
- Metrics (API): `http://localhost:8080/metrics`
- Metrics (runner): `http://localhost:9090/metrics`
- MinIO console: `http://localhost:9001`

### Kubernetes / Helm (production)

```bash
# Install the control plane (arbor-api + arbor-egress-proxy)
helm install arbor deploy/helm/arbor \
  --namespace arbor --create-namespace \
  --set api.config.databaseUrl="postgresql://arbor:pass@pg:5432/arbor" \
  --set api.config.attachTokenSecret="$(openssl rand -hex 32)" \
  --set api.config.apiBaseUrl="https://arbor.example.com"

# Runner agents run as systemd services on bare-metal KVM hosts (not in k8s).
# On each runner host, set ARBOR_RUNNER__CONTROLLER_URL and start the agent:
ARBOR_RUNNER__CONTROLLER_URL=http://arbor-api:8080 \
ARBOR_RUNNER__ADVERTISE_ADDRESS=http://$(hostname -I | awk '{print $1}'):9090 \
ARBOR_RUNNER__CAPACITY_SLOTS=10 \
  ./arbor-runner-agent
# The agent self-registers and sends heartbeats automatically.
```

### Single-node (manual)

```bash
# Download Firecracker binaries
make firecracker-bins  # installs to /var/lib/arbor/firecracker/bin/

# Build the guest agent (static musl binary for the rootfs)
make guest-agent

# Build the Ubuntu 24.04 guest rootfs (requires root + debootstrap)
sudo make image

# Start the API
ARBOR__DATABASE_URL=$DATABASE_URL \
ARBOR__ATTACH_TOKEN_SECRET=$(openssl rand -hex 32) \
  ./target/release/arbor-api &

# Start the runner agent (self-registers with the API)
ARBOR_RUNNER__CONTROLLER_URL=http://localhost:8080 \
ARBOR_RUNNER__ADVERTISE_ADDRESS=http://localhost:9090 \
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

### Runner pool management

```bash
# List all registered runners and their health
curl $BASE/v1/runners

# Manually register a runner (or let the agent self-register on startup)
# x86_64 runner:
curl -X POST $BASE/internal/runners/register \
  -H 'Content-Type: application/json' \
  -d '{
    "runner_class":        "fc-x86_64-v1",
    "address":             "http://10.0.0.5:9090",
    "arch":                "x86_64",
    "firecracker_version": "1.9.0",
    "cpu_template":        "T2",
    "capacity_slots":      10
  }'

# ARM64 / Graviton2 runner (cpu_template must be T2A):
curl -X POST $BASE/internal/runners/register \
  -H 'Content-Type: application/json' \
  -d '{
    "runner_class":        "fc-arm64-v1",
    "address":             "http://10.0.1.5:9090",
    "arch":                "aarch64",
    "firecracker_version": "1.9.0",
    "cpu_template":        "T2A",
    "capacity_slots":      8
  }'

# Drain a runner before maintenance (stops new placements immediately)
curl -X PUT $BASE/internal/runners/{runner_id}/drain

# Deregister a runner after it has fully drained and shut down
curl -X DELETE $BASE/internal/runners/{runner_id}
```

Runner agents send a heartbeat to `POST /internal/runners/heartbeat` every 15 seconds reporting their current `used_slots`. The control plane marks any runner that misses heartbeats for more than 60 seconds as unhealthy and stops scheduling new workspaces onto it.

### Workspace state machine

```
creating → ready ⟷ running → checkpointing → ready
                           ↘ terminating  → terminated
        (fork/restore) → restoring → quarantined → ready
```

---

## Key design decisions

**CPU templates by runner class:** Two runner classes are supported. `fc-x86_64-v1` requires `cpu_template=T2` (Intel) and `arch=x86_64`. `fc-arm64-v1` requires `cpu_template=T2A` (Graviton2) and `arch=aarch64`. Firecracker requires the CPU template to match exactly between the host that created a snapshot and the host that restores it. The `compatibility_key` stored in every checkpoint manifest enforces this — restoring across architectures or mismatched templates returns `RUNNER_CLASS_INCOMPATIBLE`. Mixing templates at registration time returns `422 RUNNER_ARCH_MISMATCH`.

**Full VM snapshots only:** Firecracker's diff snapshot support is still developer preview. All checkpoints are full VM snapshots. Incremental support is on the roadmap for M7 once Firecracker GA lands.

**Memory file lifecycle:** After restore, Firecracker maps guest memory from the mem snapshot file via `MAP_PRIVATE`. That file must remain accessible for the entire VM lifetime. Arbor keeps a hot copy on local NVMe for active VMs and fetches from object storage for cold restores.

**Egress via netns:** Each workspace gets its own Linux network namespace. The TAP device for Firecracker lives inside the netns. Traffic flows through a veth pair to the host, where nftables enforces the allowlist and the egress proxy handles credential injection. There is no route out of a VM except through the proxy — this is a physical constraint, not a configuration option.

**Workspace identity per-header (MVP):** The egress proxy currently identifies the source workspace via an `X-Arbor-Workspace-Id` header. Production deployments should replace this with a cryptographic binding between TAP interface MAC and workspace ID in the runner registry.

**Runner placement:** The scheduler picks the healthy runner with the lowest `used_slots` that still has available capacity (`used_slots < capacity_slots`). For checkpoint restores it additionally filters by `firecracker_version` and `cpu_template` — Firecracker requires an exact match between the host that created the snapshot and the host that restores it. Mismatches return `RUNNER_CLASS_INCOMPATIBLE` before any restore is attempted.

**GPU-capable workspaces (M9 — host-mediated inference):** Firecracker has no VFIO/GPU passthrough support. Arbor's GPU model is consistent with its credential brokering philosophy: sensitive resources (GPU compute) never enter the VM. Instead, workspaces send inference requests to the sentinel hostname `gpu.local`. The egress proxy intercepts these, rewrites the URI to the local sidecar URL (llama.cpp / vLLM / Ollama running on the host), and injects an `x-arbor-model` header. The sidecar URL is never visible inside the VM. GPU runner classes (`fc-gpu-x86_64-v1`, `fc-gpu-arm64-v1`) register their `gpu_model`, `gpu_count`, and `gpu_vram_mib`. The scheduler filters for runners with free GPU slots when `runtime.gpu_count > 0`. A background `gpu_sidecar` health loop on the runner agent publishes `arbor.runner.gpu_sidecar_healthy`, `arbor.runner.gpu_available`, and `arbor.runner.gpu_vram_total_mib` metrics every 30 seconds.

**ARM64 runner class (`fc-arm64-v1`):** ARM64 Graviton2 hosts use the `T2A` CPU template, not `T2`. The control plane enforces this at registration time — a runner posting `runner_class=fc-arm64-v1` with `cpu_template=T2` is rejected with `422 RUNNER_ARCH_MISMATCH`. The scheduler keeps x86_64 and aarch64 workspaces entirely separate via the `runner_class` field; a checkpoint created on an ARM64 runner can only be restored on another ARM64 runner with the same Firecracker version. Build the ARM64 guest agent and Firecracker binaries with `make guest-agent-arm64` and `make firecracker-bins-arm64`.

**Drain protocol:** Draining is a two-phase handshake. The control plane marks the runner unhealthy via `PUT /internal/runners/{id}/drain` (stops new placements). The runner agent, on receiving `SIGTERM` or a local `PUT /drain` call, sets its internal drain flag (rejects new `POST /vms` with 503), then waits up to 60 seconds for in-flight VMs to finish before exiting. The control plane's 60-second heartbeat timeout acts as the backstop if the runner exits uncleanly.

**Prometheus metrics:** Both `arbor-api` and `arbor-runner-agent` expose `GET /metrics` in Prometheus text format. Key runner-agent metrics: `arbor.runner.active_vms` (gauge), `arbor.runner.vm_boots_total`, `arbor.runner.vm_boot_duration_seconds`, `arbor.runner.checkpoints_total`, `arbor.runner.restores_total`. The pod annotation `prometheus.io/scrape: "true"` is set by default in the Helm chart.

---

## Roadmap

| Milestone | Feature | Status |
|---|---|---|
| M1 | Single-node create / exec / terminate | ✅ Complete |
| M2 | Guest rootfs + private Docker daemon | ✅ Complete |
| M3 | Full VM checkpoint + S3 upload | ✅ Complete |
| M4 | Branch-safe fork: quarantine + reseal | ✅ Complete |
| M5 | Secret Broker + Egress Proxy | ✅ Complete |
| M6 | Multi-runner pool + Prometheus + Helm | ✅ Complete |
| M7 | Diff snapshots (Firecracker GA) | ⏸ Blocked — diff snapshots remain upstream developer preview |
| M8 | ARM64 runner class | ✅ Complete |
| M9 | GPU-capable workspaces via host-mediated inference | ✅ Complete |
| M10 | Trajectory tracer + rollout debugger | ✅ Done |
| M11 | `arbor run-benchmark` CLI + SWE-bench integration | ✅ Done |

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

- **Python and TypeScript SDKs** (currently raw HTTP only)
- **Vault / AWS Secrets Manager** backend for `arbor-secret-broker` (currently env-var based)
- **Diff snapshots** (M7) — blocked on Firecracker diff snapshot feature graduating from developer preview
- **Integration tests** for the full fork + reseal flow against a live runner

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