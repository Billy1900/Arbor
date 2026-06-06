.PHONY: build check test test-unit test-integration fmt lint clean \
        docker-up docker-down db-setup db-reset \
        guest-agent guest-agent-arm64 \
        image image-arm64 \
        firecracker-bins firecracker-bins-arm64 \
        help

# ── Default ───────────────────────────────────────────────────────────────────

help:
	@echo "Arbor — build and development tasks"
	@echo ""
	@echo "  make build               Build all services (release)"
	@echo "  make check               cargo check --workspace (no DB needed)"
	@echo "  make test-unit           Run unit + pure logic tests (no DB)"
	@echo "  make test-integration    Run DB integration tests (needs DB)"
	@echo "  make fmt                 Format all Rust code"
	@echo "  make lint                clippy --workspace"
	@echo "  make docker-up           Start postgres + minio + services via compose"
	@echo "  make docker-down         Stop compose stack"
	@echo "  make db-setup            Create DB + run migrations"
	@echo "  make db-reset            Drop and recreate DB"
	@echo "  make guest-agent         Build static musl guest-agent (x86_64)"
	@echo "  make guest-agent-arm64   Build static musl guest-agent (aarch64)"
	@echo "  make image               Build Ubuntu 24.04 guest rootfs x86_64 (requires root)"
	@echo "  make image-arm64         Build Ubuntu 24.04 guest rootfs aarch64 (requires root)"
	@echo "  make firecracker-bins        Download Firecracker + Jailer (x86_64)"
	@echo "  make firecracker-bins-arm64  Download Firecracker + Jailer (aarch64)"

# ── Rust ──────────────────────────────────────────────────────────────────────

check:
	SQLX_OFFLINE=true cargo check --workspace

build:
	SQLX_OFFLINE=true cargo build --release --workspace

fmt:
	cargo fmt --all

lint:
	SQLX_OFFLINE=true cargo clippy --workspace -- -D warnings

# ── Tests ─────────────────────────────────────────────────────────────────────

test-unit:
	@echo "Running unit tests (no DB required)..."
	SQLX_OFFLINE=true cargo test -p arbor-tests \
		--test safety \
		--test reseal \
		-- --nocapture 2>&1

test-api-smoke:
	@echo "Running API smoke tests..."
	SQLX_OFFLINE=true cargo test -p arbor-tests \
		--test api_smoke \
		-- --nocapture 2>&1

test-m6:
	@echo "Running M6 runner pool tests (no DB)..."
	SQLX_OFFLINE=true cargo test -p arbor-tests --test m6_smoke -- --nocapture 2>&1
	SQLX_OFFLINE=true cargo test -p arbor-runner-agent -- --nocapture 2>&1

test-m8:
	@echo "Running M8 ARM64 runner class tests (no DB)..."
	SQLX_OFFLINE=true cargo test -p arbor-tests --test m8_arm64_tests -- --nocapture 2>&1
	SQLX_OFFLINE=true cargo test -p arbor-runner-agent -- --nocapture 2>&1

test-integration: db-setup
	@echo "Running integration tests (requires PostgreSQL)..."
	DATABASE_URL=$(DATABASE_URL) \
	TEST_DATABASE_URL=$(DATABASE_URL) \
		cargo test -p arbor-tests --test db_tests -- --nocapture 2>&1
	DATABASE_URL=$(DATABASE_URL) \
	TEST_DATABASE_URL=$(DATABASE_URL) \
		cargo test -p arbor-controller --test integration_test -- --nocapture 2>&1

test-all: test-unit test-api-smoke test-m6 test-m8 test-integration

# ── Database ──────────────────────────────────────────────────────────────────

DATABASE_URL ?= postgresql://arbor:arbor_dev_only@localhost/arbor

db-setup:
	@echo "Setting up database..."
	psql $(DATABASE_URL) -c "SELECT 1" 2>/dev/null || \
		(createdb arbor 2>/dev/null || true)
	DATABASE_URL=$(DATABASE_URL) cargo sqlx prepare --workspace
	DATABASE_URL=$(DATABASE_URL) sqlx migrate run --source migrations/

db-reset:
	@echo "Dropping and recreating database..."
	dropdb --if-exists arbor
	createdb arbor
	DATABASE_URL=$(DATABASE_URL) sqlx migrate run --source migrations/

db-prepare:
	DATABASE_URL=$(DATABASE_URL) cargo sqlx prepare --workspace

# ── Guest agent (musl static binaries) ───────────────────────────────────────

guest-agent:
	@echo "Building static guest-agent for x86_64..."
	rustup target add x86_64-unknown-linux-musl
	cargo build --release \
		--target x86_64-unknown-linux-musl \
		-p arbor-guest-agent
	@echo "Binary: target/x86_64-unknown-linux-musl/release/arbor-guest-agent"
	@ls -lh target/x86_64-unknown-linux-musl/release/arbor-guest-agent

guest-agent-arm64:
	@echo "Building static guest-agent for aarch64..."
	rustup target add aarch64-unknown-linux-musl
	@# Cross-linker required on x86_64 hosts: apt install gcc-aarch64-linux-gnu
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
	cargo build --release \
		--target aarch64-unknown-linux-musl \
		-p arbor-guest-agent
	@echo "Binary: target/aarch64-unknown-linux-musl/release/arbor-guest-agent"
	@ls -lh target/aarch64-unknown-linux-musl/release/arbor-guest-agent

# ── Guest rootfs images ───────────────────────────────────────────────────────

image: guest-agent
	@echo "Building Ubuntu 24.04 guest rootfs (x86_64, requires root)..."
	cp target/x86_64-unknown-linux-musl/release/arbor-guest-agent \
		images/ubuntu-24.04-dev/arbor-guest-agent
	sudo bash images/ubuntu-24.04-dev/build.sh

image-arm64: guest-agent-arm64
	@echo "Building Ubuntu 24.04 guest rootfs (aarch64, requires root)..."
	cp target/aarch64-unknown-linux-musl/release/arbor-guest-agent \
		images/ubuntu-24.04-dev/arbor-guest-agent
	sudo ARCH=aarch64 bash images/ubuntu-24.04-dev/build.sh

# ── Firecracker binaries ──────────────────────────────────────────────────────

FC_VERSION ?= v1.9.0
FC_DIR     ?= /var/lib/arbor/firecracker/bin

firecracker-bins:
	@echo "Downloading Firecracker $(FC_VERSION) for x86_64..."
	mkdir -p $(FC_DIR)
	curl -L \
		"https://github.com/firecracker-microvm/firecracker/releases/download/$(FC_VERSION)/firecracker-$(FC_VERSION)-x86_64.tgz" \
		| tar -xz -C /tmp/
	install -m 755 /tmp/release-$(FC_VERSION)-x86_64/firecracker-$(FC_VERSION)-x86_64 \
		$(FC_DIR)/firecracker
	install -m 755 /tmp/release-$(FC_VERSION)-x86_64/jailer-$(FC_VERSION)-x86_64 \
		$(FC_DIR)/jailer
	@echo "Binaries installed to $(FC_DIR)"

firecracker-bins-arm64:
	@echo "Downloading Firecracker $(FC_VERSION) for aarch64..."
	mkdir -p $(FC_DIR)
	curl -L \
		"https://github.com/firecracker-microvm/firecracker/releases/download/$(FC_VERSION)/firecracker-$(FC_VERSION)-aarch64.tgz" \
		| tar -xz -C /tmp/
	install -m 755 /tmp/release-$(FC_VERSION)-aarch64/firecracker-$(FC_VERSION)-aarch64 \
		$(FC_DIR)/firecracker
	install -m 755 /tmp/release-$(FC_VERSION)-aarch64/jailer-$(FC_VERSION)-aarch64 \
		$(FC_DIR)/jailer
	@echo "Binaries installed to $(FC_DIR)"

# ── Docker Compose ────────────────────────────────────────────────────────────

docker-up:
	@[ -f deploy/.env ] || (echo "Copy deploy/.env.example to deploy/.env and fill in values"; exit 1)
	docker-compose -f deploy/docker-compose.yml up -d
	@echo ""
	@echo "Services:"
	@echo "  API:       http://localhost:8080"
	@echo "  Metrics:   http://localhost:8080/metrics"
	@echo "  MinIO:     http://localhost:9001 (admin UI)"
	@echo ""
	@echo "Quick test:"
	@echo "  curl -s http://localhost:8080/health | jq ."

docker-down:
	docker-compose -f deploy/docker-compose.yml down

docker-logs:
	docker-compose -f deploy/docker-compose.yml logs -f

# ── Register dev runner ───────────────────────────────────────────────────────

register-dev-runner:
	@echo "Registering localhost as an x86_64 runner node..."
	curl -s -X POST http://localhost:8080/internal/runners/register \
		-H 'Content-Type: application/json' \
		-d '{ \
			"runner_class":        "fc-x86_64-v1", \
			"address":             "http://localhost:9090", \
			"arch":                "x86_64", \
			"firecracker_version": "1.9.0", \
			"cpu_template":        "T2", \
			"capacity_slots":      10 \
		}' | jq .

register-dev-runner-arm64:
	@echo "Registering localhost as an aarch64 runner node..."
	curl -s -X POST http://localhost:8080/internal/runners/register \
		-H 'Content-Type: application/json' \
		-d '{ \
			"runner_class":        "fc-arm64-v1", \
			"address":             "http://localhost:9090", \
			"arch":                "aarch64", \
			"firecracker_version": "1.9.0", \
			"cpu_template":        "T2A", \
			"capacity_slots":      10 \
		}' | jq .

# ── Demo workflow ─────────────────────────────────────────────────────────────

demo-fork:
	@echo "=== Arbor fork demo ==="
	@echo ""
	@echo "1. Create workspace..."
	@WS=$$(curl -s -X POST http://localhost:8080/v1/workspaces \
		-H 'Content-Type: application/json' \
		-d '{"name":"demo","repo":{"provider":"github","url":"git@github.com:x/y.git","ref":"refs/heads/main"},"runtime":{"runner_class":"fc-x86_64-v1","vcpu_count":2,"memory_mib":2048,"disk_gb":20},"image":{"base_image_id":"ubuntu-24.04-dev-v1"},"network":{"egress_policy":"default-deny"}}' \
		| jq -r '.workspace_id'); \
	echo "  workspace_id=$$WS"; \
	echo ""; \
	echo "2. Take checkpoint..."; \
	CKPT=$$(curl -s -X POST http://localhost:8080/v1/workspaces/$$WS/checkpoints \
		-H 'Content-Type: application/json' \
		-d '{"name":"before-experiment","mode":"full_vm"}' \
		| jq -r '.checkpoint_id'); \
	echo "  checkpoint_id=$$CKPT"; \
	echo ""; \
	echo "3. Fork 3 parallel attempts..."; \
	for branch in attempt-a attempt-b attempt-c; do \
		FORK=$$(curl -s -X POST http://localhost:8080/v1/checkpoints/$$CKPT/fork \
			-H 'Content-Type: application/json' \
			-d "{\"branch_name\":\"$$branch\",\"post_restore\":{\"quarantine\":true,\"identity_reseal\":true}}" \
			| jq -r '.workspace_id'); \
		echo "  $$branch → $$FORK"; \
	done; \
	echo ""; \
	echo "Each fork enters quarantine, runs reseal hooks, then is READY with distinct identity."

clean:
	cargo clean
