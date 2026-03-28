-- Arbor schema v1
-- Run with: sqlx migrate run

-- ── Custom types ─────────────────────────────────────────────────────────────

CREATE TYPE workspace_state AS ENUM (
    'creating', 'ready', 'running', 'checkpointing',
    'restoring', 'quarantined', 'terminating', 'terminated', 'error'
);

CREATE TYPE checkpoint_state AS ENUM (
    'pending', 'uploading', 'sealed', 'failed'
);

CREATE TYPE operation_type AS ENUM (
    'create_workspace', 'terminate_workspace', 'create_checkpoint',
    'restore_checkpoint', 'fork_checkpoint', 'exec_session'
);

CREATE TYPE operation_status AS ENUM (
    'pending', 'running', 'succeeded', 'failed', 'canceled'
);

-- ── Runner nodes ─────────────────────────────────────────────────────────────

CREATE TABLE runner_nodes (
    id                  UUID PRIMARY KEY,
    runner_class        TEXT NOT NULL,
    address             TEXT NOT NULL,    -- http://host:port
    arch                TEXT NOT NULL,
    firecracker_version TEXT NOT NULL,
    cpu_template        TEXT NOT NULL,
    capacity_slots      INT  NOT NULL DEFAULT 10,
    used_slots          INT  NOT NULL DEFAULT 0,
    healthy             BOOL NOT NULL DEFAULT true,
    last_heartbeat      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX runner_nodes_class_healthy ON runner_nodes (runner_class, healthy);

-- ── Workspaces ───────────────────────────────────────────────────────────────

CREATE TABLE workspaces (
    id                      UUID PRIMARY KEY,
    name                    TEXT NOT NULL,
    state                   workspace_state NOT NULL DEFAULT 'creating',
    repo_provider           TEXT NOT NULL,
    repo_url                TEXT NOT NULL,
    repo_ref                TEXT NOT NULL,
    repo_commit             TEXT,
    runner_class            TEXT NOT NULL,
    vcpu_count              INT NOT NULL DEFAULT 2,
    memory_mib              INT NOT NULL DEFAULT 2048,
    disk_gb                 INT NOT NULL DEFAULT 30,
    base_image_id           TEXT NOT NULL,
    compatibility_key       JSONB NOT NULL,
    current_checkpoint_id   UUID,
    runner_id               UUID REFERENCES runner_nodes(id),
    vm_id                   TEXT,        -- internal VM identifier on runner
    identity_epoch          BIGINT NOT NULL DEFAULT 0,
    network_epoch           BIGINT NOT NULL DEFAULT 0,
    error_message           TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX workspaces_state     ON workspaces (state);
CREATE INDEX workspaces_runner_id ON workspaces (runner_id);

-- ── Checkpoints ──────────────────────────────────────────────────────────────

CREATE TABLE checkpoints (
    id                      UUID PRIMARY KEY,
    workspace_id            UUID NOT NULL REFERENCES workspaces(id),
    parent_id               UUID REFERENCES checkpoints(id),
    name                    TEXT,
    state                   checkpoint_state NOT NULL DEFAULT 'pending',
    compatibility_key       JSONB NOT NULL,
    artifacts               JSONB NOT NULL DEFAULT '{}',
    resume_hooks_version    INT NOT NULL DEFAULT 1,
    identity_epoch          BIGINT NOT NULL,
    network_epoch           BIGINT NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX checkpoints_workspace_id ON checkpoints (workspace_id);
CREATE INDEX checkpoints_parent_id    ON checkpoints (parent_id);

-- ── Operations ───────────────────────────────────────────────────────────────

CREATE TABLE operations (
    id              UUID PRIMARY KEY,
    op_type         operation_type NOT NULL,
    target_id       TEXT NOT NULL,   -- workspace_id or checkpoint_id
    status          operation_status NOT NULL DEFAULT 'pending',
    progress_pct    SMALLINT,
    error_json      JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX operations_target_id ON operations (target_id);
CREATE INDEX operations_status    ON operations (status) WHERE status IN ('pending','running');

-- ── Sessions ─────────────────────────────────────────────────────────────────

CREATE TABLE sessions (
    id              UUID PRIMARY KEY,
    workspace_id    UUID NOT NULL REFERENCES workspaces(id),
    command         TEXT[] NOT NULL,
    cwd             TEXT NOT NULL DEFAULT '/workspace',
    env_json        JSONB NOT NULL DEFAULT '{}',
    pty             BOOL NOT NULL DEFAULT false,
    status          TEXT NOT NULL DEFAULT 'starting',
    exit_code       INT,
    reconnectable   BOOL NOT NULL DEFAULT true,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    exited_at       TIMESTAMPTZ
);

CREATE INDEX sessions_workspace_id ON sessions (workspace_id);

-- ── Secret grants ─────────────────────────────────────────────────────────────

CREATE TABLE secret_grants (
    id              UUID PRIMARY KEY,
    workspace_id    UUID NOT NULL REFERENCES workspaces(id),
    provider        TEXT NOT NULL,
    mode            TEXT NOT NULL DEFAULT 'brokered_proxy',
    vault_ref       TEXT NOT NULL,
    allowed_hosts   TEXT[] NOT NULL DEFAULT '{}',
    ttl_seconds     INT NOT NULL DEFAULT 3600,
    active          BOOL NOT NULL DEFAULT true,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX secret_grants_workspace_id ON secret_grants (workspace_id);
CREATE INDEX secret_grants_active       ON secret_grants (workspace_id, active) WHERE active = true;

-- ── Trigger: updated_at ──────────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER workspaces_updated_at
    BEFORE UPDATE ON workspaces
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER operations_updated_at
    BEFORE UPDATE ON operations
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
