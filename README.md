# Arbor

![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)
![License](https://img.shields.io/badge/license-MIT-blue)
![Stars](https://img.shields.io/github/stars/Billy1900/Arbor)
![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)

**Branch-native sandboxes for AI agent team. Fork state. Restore anywhere. Leak nothing.**


Arbor provides microVM-isolated workspaces that coding agents can run real builds in, checkpoint at any moment, fork into parallel branches, and restore safely — all within your own VPC. Think of it as the infrastructure layer that makes multi-agent experimentation safe and reproducible.

Built in Rust on top of [Firecracker](https://github.com/firecracker-microvm/firecracker).

---

## Why Arbor?

| Feature | Docker | E2B | Arbor |
|---|---|---|---|
| VM-level isolation | ❌ | ✅ | ✅ |
| Branch/fork execution state | ❌ | ❌ | ✅ |
| Snapshot mid-task & resume | ❌ | ❌ | ✅ |
| Credential quarantine on seal | ❌ | ❌ | ✅ |
| Self-hostable | ✅ | ✅ | ✅ |
| Sub-150ms boot | ❌ | ✅ | ✅ |


More details are introduced in [INTRO.md](INTRO.md) and summarized in the differentiators and comparison sections below.

---


## Architecture

![img](arbor_architecture_readme.svg)

### Crates

| Crate | Role |
|---|---|
| `arbor-api` | REST API, WebSocket PTY attach (axum) |
| `arbor-controller` | Workspace state machine, operation orchestration (sqlx/postgres) |
| `arbor-runner-agent` | Firecracker + Jailer lifecycle, netns, vsock multiplexer |
| `arbor-guest-agent` | Static musl binary inside VM: PTY exec, port scan, quiesce |
| `arbor-snapshot` | Checkpoint manifest, S3/MinIO upload, sha256 integrity |
| `arbor-egress-proxy` | CONNECT proxy, allowlist enforcement, credential injection (hyper) |
| `arbor-secret-broker` | Grant lifecycle, Vault integration |
| `arbor-common` | Shared types, vsock frame protocol, error codes |

---

## Code Hooks
```
// Spin up an isolated branch workspace
let workspace = Arbor::new().await?;
let branch = workspace.branch("feature-explore").await?;

// Run agent code in microVM isolation
let result = branch.run("cargo test").await?;

// Snapshot the state mid-task
let checkpoint = branch.snapshot().await?;

// Fork into two parallel explorations
let (branch_a, branch_b) = checkpoint.fork().await?;
```

## API quick reference

```bash
BASE=http://localhost:8080

# Create workspace
curl -X POST $BASE/v1/workspaces -d '{
  "name": "fix-auth-bug",
  "repo": { "provider": "github", "url": "git@github.com:org/repo.git", "ref": "refs/heads/main" },
  "runtime": { "runner_class": "fc-x86_64-v1", "vcpu_count": 4, "memory_mib": 4096, "disk_gb": 40 },
  "image": { "base_image_id": "ubuntu-24.04-dev-v1" },
  "network": { "egress_policy": "default-deny" }
}'

# Open a PTY shell
curl -X POST $BASE/v1/workspaces/{ws_id}/exec \
  -d '{ "command": ["bash", "-l"], "pty": true }'

# Get WebSocket attach URL
curl -X POST $BASE/v1/sessions/{sess_id}/attach
# → wss://host/v1/attach/{sess_id}?token=...

# Checkpoint before a risky operation
curl -X POST $BASE/v1/workspaces/{ws_id}/checkpoints \
  -d '{ "name": "before-migration", "mode": "full_vm" }'

# Fork into parallel attempts (each gets fresh identity)
curl -X POST $BASE/v1/checkpoints/{ckpt_id}/fork \
  -d '{ "branch_name": "attempt-a", "post_restore": { "quarantine": true, "identity_reseal": true } }'

# Bind a secret (credential never enters VM)
curl -X PUT $BASE/v1/workspaces/{ws_id}/secrets/grants/{grant_id} -d '{
  "provider": "openai",
  "mode": "brokered_proxy",
  "vault_ref": "vault://prod/openai-key",
  "allowed_hosts": ["api.openai.com"],
  "inject": { "kind": "authorization_header" }
}'

# Subscribe to events
curl -N $BASE/v1/workspaces/{ws_id}/events
```

### Workspace states

```
creating → ready ⟷ running → checkpointing → ready
                           ↘ terminating → terminated
    (fork/restore) → restoring → quarantined → ready
```

---

## Getting started

### Prerequisites

- Linux host with KVM (`/dev/kvm` accessible)
- [Firecracker + Jailer](https://github.com/firecracker-microvm/firecracker/releases) binaries
- PostgreSQL 16
- Rust 1.82+

### Build

```bash
git clone https://github.com/your-org/arbor && cd arbor

export DATABASE_URL=postgresql://arbor:password@localhost/arbor
createdb arbor

cargo sqlx prepare --workspace   # generates .sqlx/ cache
cargo build --release

# Build static guest agent binary (goes into the VM rootfs)
cargo build --release --target x86_64-unknown-linux-musl -p arbor-guest-agent

# Build guest VM image (requires root, debootstrap)
sudo bash images/ubuntu-24.04-dev/build.sh
```

### Run (single node)

```bash
# Place binaries
cp firecracker jailer /var/lib/arbor/firecracker/bin/
cp vmlinux /var/lib/arbor/firecracker/

# Register this machine as a runner
psql $DATABASE_URL -c "INSERT INTO runner_nodes
  (id, runner_class, address, arch, firecracker_version, cpu_template, capacity_slots)
  VALUES (gen_random_uuid(), 'fc-x86_64-v1', 'http://localhost:9090',
          'x86_64', '1.9.0', 'T2', 10);"

# Start services
ARBOR__DATABASE_URL=$DATABASE_URL \
ARBOR__ATTACH_TOKEN_SECRET=$(openssl rand -hex 32) \
  ./target/release/arbor-api &

./target/release/arbor-runner-agent &
./target/release/arbor-egress-proxy &
```

### Docker Compose (development)

```bash
cp deploy/.env.example deploy/.env
docker-compose -f deploy/docker-compose.yml up
```

---

## Key design decisions

**CPU template:** Uses `T2` (Intel x86_64), not `T2A` (ARM/Graviton2). Firecracker requires the CPU template to match between snapshot creation and restore. This is enforced via the `compatibility_key` stored in every checkpoint manifest.

**Diff snapshots:** Not used in MVP. Firecracker's diff snapshot support is still marked developer preview. All checkpoints are full VM snapshots. Incremental support is on the roadmap.

**Memory file lifecycle:** After restore, Firecracker maps guest memory from the mem snapshot file via `MAP_PRIVATE`. That file must remain accessible for the entire VM lifetime. Arbor keeps a hot copy on local NVMe for active VMs and fetches from object storage for cold restores.

**Egress via netns:** Each workspace gets its own Linux network namespace. The TAP device for Firecracker lives inside the netns. Traffic flows through a veth pair to the host, where nftables enforces the allowlist and the egress proxy handles credential injection. This makes it physically impossible for a VM to bypass the policy — there is no route out except through the proxy.

---

## Roadmap

| Milestone | Feature | Status |
|---|---|---|
| M1 | Single-node create / exec / terminate | Complete |
| M2 | Guest rootfs + private Docker daemon | Build script ready |
| M3 | Full VM checkpoint + S3 upload | Complete |
| M4 | Branch-safe fork: quarantine + reseal | Complete |
| M5 | Secret Broker + Egress Proxy | Complete |
| M6 | Multi-runner pool + Prometheus + Helm | In progress |
| M7 | Diff snapshots (Firecracker GA) | Planned |
| M8 | ARM64 runner class | Planned |
| M9 | GPU passthrough runner | Planned |

---

## Contributing

```bash
# Check (no live DB needed)
SQLX_OFFLINE=true cargo check --workspace

# Test
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all
```

High-value contribution areas:

- Integration tests for the fork + reseal flow
- Prometheus metrics in `arbor-runner-agent`
- Multi-runner heartbeat + drain protocol (M6)
- Python and TypeScript SDKs

---

## License

MIT. See [LICENSE](LICENSE).
