# Arbor

**Branch-native sandboxes for AI agent team. Fork state. Restore anywhere. Leak nothing.**

Arbor provides microVM-isolated workspaces that coding agents can run real builds in, checkpoint at any moment, fork into parallel branches, and restore safely — all within your own VPC. Think of it as the infrastructure layer that makes multi-agent experimentation safe and reproducible.

Built in Rust on top of [Firecracker](https://github.com/firecracker-microvm/firecracker).

---

## The problem Arbor solves

When you run a coding agent on a real repository, three things keep going wrong:

**1. Agents can't safely experiment in parallel.**
If you want to try three different approaches to fixing a bug, you need three isolated environments. Spinning up three fresh VMs from scratch wastes minutes and gigabytes. Existing snapshot tools let you restore a checkpoint — but they don't handle what happens when you restore the *same* snapshot three times. All three copies share the same SSH keys, session tokens, PRNG seeds, and Docker layer cache state. They'll silently collide.

**2. Secrets end up inside the VM.**
The standard pattern — inject an `OPENAI_API_KEY` environment variable into the sandbox — means the agent can read, log, and exfiltrate credentials. An agent executing arbitrary code in a compromised dependency has full access to every secret in its environment.

**3. You can't run this on your own infrastructure.**
Every existing coding sandbox is SaaS-only. If your codebase is proprietary, your compliance team won't let agent traffic touch a third-party cloud. There is no self-hosted option that gives you microVM isolation, checkpoint/restore, and proper secret handling in one coherent system.

Arbor is the answer to all three.

More details are introduced in [INTRO.md](INTRO.md) and summarized in the differentiators and comparison sections below.

---

## Core differentiators

### 1. Branch-safe restore (unique)

Firecracker's official documentation [explicitly warns](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md):

> *Resuming a microVM from a snapshot that has been previously used is possible, but the content of the Guest's memory will have the same entropy as the original snapshot.*

Restoring the same checkpoint twice means both VMs start with identical PRNG seeds, identical in-memory token caches, identical SSH agent state. For single-agent use this is an acceptable limitation. For multi-agent branching experiments, it is a correctness bug.

Arbor solves this with a **quarantine + reseal** protocol. Every fork goes through:

```
fork(checkpoint_id)
 └─ new VM boots in QUARANTINED state
     ├─ all egress blocked (no network out)
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

### 2. VPC-first secret brokering

Arbor's egress proxy sits on the host, outside the VM. When an agent calls `api.openai.com`:

```
agent process
  → VM network stack (blocked by default)
  → host netns + TAP device
  → arbor-egress-proxy
      ├─ allowlist check (is this host permitted?)
      ├─ credential injection (Authorization: Bearer <real-key>)
      └─ upstream request to api.openai.com
```

The VM never receives the credential value. The agent sees a placeholder like `OPENAI_API_KEY=arbor-brokered` in its environment. The real key is injected by the host-side proxy. If the agent logs its environment, leaks it to a supply-chain compromise, or exfiltrates it via a prompt injection — the real key is never exposed.

### 3. Checkpoint DAG

Every checkpoint records its parent, forming a directed acyclic graph:

```
ws-main ──ckpt-A "before-migration"
              ├── ws-attempt-1  (fork: postgres migration path)
              ├── ws-attempt-2  (fork: redis approach)
              └── ws-attempt-3  (fork: skip migration entirely)
```

Each forked workspace has its own isolated identity, its own Docker daemon, its own egress policy, and its own secret grants. The parent workspace keeps running. None of the three attempts can observe or interfere with each other.

### 4. Self-host / VPC-first

Arbor is designed from day one to run inside your own infrastructure. The entire control plane, runner pool, and egress proxy run in your VPC. Code, secrets, and agent activity never leave your network. This is the deployment model, not an enterprise add-on.

---

## How it compares

| | Arbor | E2B | Docker Sandboxes | Modal | Daytona |
|---|---|---|---|---|---|
| Isolation | Firecracker microVM | Firecracker microVM | Firecracker microVM | Container | Container/VM |
| Private Docker daemon | Yes | Yes | Yes | No | No |
| VM checkpoint | Full VM | Basic resume | No | Container-level | No |
| Fork from checkpoint | First-class API | No | No | No | No |
| Branch-safe restore | **Yes (unique)** | No | No | No | No |
| Credential brokering | Host-side proxy | No | Yes | No | No |
| Default-deny egress | Yes | Partial | Yes | No | No |
| Self-host / VPC | **First-class** | SaaS only | SaaS only | SaaS only | Yes |
| Open source | Yes (MIT, Rust) | SDK only | No | No | Yes |

**E2B** is the closest technical peer — also Firecracker-based, also targets AI agents — but has no fork API, no branch-safe semantics, and is SaaS-only. Great for single-agent sandboxing; not built for multi-agent branching.

**Docker Sandboxes** introduced the brokered-credentials pattern that Arbor builds on, but has no snapshot capability at all, no self-host option, and is SaaS-only.

**Modal** has excellent container checkpointing and scale-to-zero, but is function-oriented rather than workspace-oriented. You can't `git clone` a repo and run a multi-hour agent session in a persistent environment.

**Daytona** is self-hostable and git-native, but designed for human developers. No snapshot, no credential brokering, no egress policy, no agent-oriented API.

---

## Architecture

```
                   ┌──────────────────────────────┐
                   │        CLI / SDK / UI        │
                   └──────────────┬───────────────┘
                                  │ REST + WebSocket
              ┌───────────────────▼──────────────────┐
              │           arbor-api  (axum)           │
              │  AuthN · quotas · attach tokens       │
              └──────┬────────────────────┬───────────┘
                     │                    │
          ┌──────────▼──────┐   ┌─────────▼──────────┐
          │ arbor-controller│   │ arbor-secret-broker │
          │ workspace FSM   │   │ grant TTL · rotation│
          └──────┬──────────┘   └─────────┬───────────┘
                 │ schedule               │ push grants
          ┌──────▼──────────────────────────────────────┐
          │           arbor-snapshot-service             │
          │     manifest · S3 upload · digest · GC       │
          └──────────────────┬──────────────────────────┘
                             │
          ┌──────────────────▼──────────────────────────┐
          │              Runner Node Pool                │
          │  ┌────────────────────────────────────────┐ │
          │  │        arbor-runner-agent              │ │
          │  │  Firecracker + Jailer + cgroups v2     │ │
          │  │  netns · TAP · vsock mux · snapshots   │ │
          │  └──────────────────┬─────────────────────┘ │
          │                     │ vsock                  │
          │  ┌──────────────────▼─────────────────────┐ │
          │  │         Workspace microVM              │ │
          │  │  dockerd · git · arbor-guest-agent     │ │
          │  │  language runtimes · agent process     │ │
          │  └────────────────────────────────────────┘ │
          │                                             │
          │  ┌─────────────────────────────────────────┐│
          │  │          arbor-egress-proxy             ││
          │  │  allowlist · injection · audit log      ││
          │  └─────────────────────────────────────────┘│
          └─────────────────────────────────────────────┘
```

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
