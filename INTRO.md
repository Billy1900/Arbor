
The need for an AI agent sandbox became obvious within days of emerging needs of code agent.

### What is Sandbox?
An AI agent sandbox is a secure, isolated, and typically ephemeral cloud environment designed to allow autonomous AI agents to execute code, browse the web, and interact with files without posing risks to the host system, user credentials, or production data.
By early 2026, Cloudflare, Vercel, Ramp, and Modal all shipped sandbox features. Dedicated providers like E2B, Northflank, and Firecrawl built entire platforms around the problem.

### Why do we need sandbox?
- Safety: An AI agent sandbox completely isolates agent execution from your host system, cloud credentials, and sensitive production data.
- Environment: Agents need to actively run to get more runtime information for feedback loop / reasoning
- Scalability — Ability to run thousands of concurrent sandboxes
- Isolation — Code runs in VMs or containers, separated from host systems

### What is difference between the previous sandbox we had? Like docker, KVM?
Key distinction from cloud compute: These are purpose-built for agent use cases, not general application hosting. They optimize for fast creation, easy cleanup, and developer-friendly SDKs.
- Dedicated sandbox providers like Docker, E2B, Modal, Northflank, and Firecrawl's Browser Sandbox are competing heavily on startup speed, isolation quality, developer experience, and what tooling comes pre-loaded. Agent sandboxing is becoming a distinct category, not just a framework feature.
- Whenever you start a new coding session, you want to spin up a new sandbox that has a full development environment. This will allow the agent to work effectively, by having access to all the tools a human would have, while also being isolated from other work. It’s also crucial that time-to-first-token is as fast as possible.

### The problem Arbor solves

When you run a coding agent on a real repository, three things keep going wrong:

**1. Agents can't safely experiment in parallel.**
If you want to try three different approaches to fixing a bug, you need three isolated environments. Spinning up three fresh VMs from scratch wastes minutes and gigabytes. Existing snapshot tools let you restore a checkpoint — but they don't handle what happens when you restore the *same* snapshot three times. All three copies share the same SSH keys, session tokens, PRNG seeds, and Docker layer cache state. They'll silently collide.

**2. Secrets end up inside the VM.**
The standard pattern — inject an `OPENAI_API_KEY` environment variable into the sandbox — means the agent can read, log, and exfiltrate credentials. An agent executing arbitrary code in a compromised dependency has full access to every secret in its environment.

**3. You can't run this on your own infrastructure.**
Every existing coding sandbox is SaaS-only. If your codebase is proprietary, your compliance team won't let agent traffic touch a third-party cloud. There is no self-hosted option that gives you microVM isolation, checkpoint/restore, and proper secret handling in one coherent system.

Arbor is the answer to all three.


### Different types of sandbox
- There are three main categories of sandboxing: browser sandboxes, code execution sandboxes, and full dev environment sandboxes.
  - For agents that browse the web: Firecrawl Browser Sandbox
  - For agents that execute code: E2B or Modal
  - For full coding agents on real projects: Northflank and Docker

| Sandbox Type    | Best For                                  | Typical Provider | Isolation Level                  |
|-----------------|-------------------------------------------|------------------|----------------------------------|
| Browser Sandbox | LLMs scraping the web, filling forms      | Firecrawl        | Cloud Container                  |
| Code Execution  | Data analysis, shell script execution     | E2B              | MicroVM                          |
| Full Dev Env    | Complex coding agents across repositories | Docker           | Unprivileged Container / MicroVM |


Browser sandboxes come in different forms: some are purpose-built for UI automation and form filling, others for visual testing, and others specifically for web extraction. For workflow automation (multi-step browser flows, form submissions, login sequences), tools like Browserbase are a strong fit. Since web extraction is one of the most critical tools in any AI agent's stack (agents constantly need to retrieve, parse, and reason over live web data), that's the type we'll focus on here.
A browser sandbox for web extraction provides such agents with a fully managed, cloud-hosted browser session environment. The agent can freely navigate pages, click elements, aggressively type form inputs, take screenshots, and run local client-side JavaScript inside a real Chromium instance.

A code execution sandbox gives AI agents a remote runtime environment where they can actively write and execute application code.
This includes performing complex file access operations, running full process executions, and managing ad-hoc dependency package installations. The process runs deep inside an isolated microVM or secured container, structurally walled off from your primary host operating system.

E2B is currently the most widely adopted dedicated provider consistently operating within this technical category. Their robust sandbox architecture is built fundamentally on Firecracker, the exact same battle-tested microVM technology AWS specifically uses to securely isolate AWS Lambda serverless functions.


for current software development, we have higher requirement: Full dev environment sandboxes. 
A coding agent working on real software projects needs a persistent repository, a real shell, language servers, package managers, build tools, and test runners. The challenge is execution with fidelity, persistence, and isolation at the same time.
Standard Docker containers are not a strong enough boundary for that on their own. They still share the host kernel, so a kernel escape is part of the threat model. This is especially risky when models use advanced features like parallel agent execution, where a breach could span numerous concurrent sessions.
The bigger practical issue is control-plane access: if an agent can reach the host Docker daemon or a mounted Docker socket, it can often start new containers with host mounts and bypass most of the isolation you thought you had.



### Why build Arbor?

Arbor is a purpose-built sandbox manager for AI agents, designed to provide secure, isolated, and persistent environments for code execution. It leverages Firecracker microVMs to ensure strong isolation while maintaining fast startup times and a rich development environment.

"Core differentiators" goes deep on the two things no competitor has — branch-safe restore and VPC-first credential brokering — with the actual Firecracker documentation quote as evidence. Quoting the upstream project's own warning makes the problem feel authoritative, not invented.

![alt text](image.png)

"Key design decisions" is the section that earns trust from engineers who will read the code. CPU template explanation (T2 vs T2A), memory file lifecycle, and netns egress path — these show that the design choices are deliberate, not accidental.


## Core differentiators in Arbor

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
