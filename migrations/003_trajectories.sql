-- Arbor M10: Trajectory Tracer
-- Adds rollouts, trajectories, and trace_events tables.

-- ── Rollout ───────────────────────────────────────────────────────────────────
-- A rollout = N parallel attempts forked from the same checkpoint.

CREATE TABLE rollouts (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_checkpoint_id UUID NOT NULL REFERENCES checkpoints(id),
    reward_command       TEXT NOT NULL,
    fork_count           INT  NOT NULL,
    started_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at         TIMESTAMPTZ
);

-- ── Trajectory ────────────────────────────────────────────────────────────────
-- A trajectory = one workspace's complete execution record from fork to finish.

CREATE TABLE trajectories (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id     UUID        NOT NULL REFERENCES workspaces(id),
    rollout_id       UUID        REFERENCES rollouts(id),
    parent_id        UUID        REFERENCES trajectories(id),
    started_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at         TIMESTAMPTZ,
    reward_passed    BOOL,
    reward_exit_code INT,
    step_count       INT         NOT NULL DEFAULT 0,
    total_cost_usd   NUMERIC(10,6) NOT NULL DEFAULT 0,
    wall_time_ms     BIGINT
);

CREATE INDEX trajectories_rollout   ON trajectories (rollout_id);
CREATE INDEX trajectories_workspace ON trajectories (workspace_id);

-- ── Trace events ──────────────────────────────────────────────────────────────
-- Append-only event log, one row per step.
-- step_seq is monotonically increasing per trajectory, maintained by the writer.
-- kind mirrors the TraceEvent serde tag: exec_started, exec_exited, file_diff,
--   network_access, llm_call, checkpoint_taken, workspace_forked, reward_evaluated.

CREATE TABLE trace_events (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    trajectory_id UUID        NOT NULL REFERENCES trajectories(id),
    workspace_id  UUID        NOT NULL,
    step_seq      BIGINT      NOT NULL,
    kind          TEXT        NOT NULL,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload       JSONB       NOT NULL
);

CREATE UNIQUE INDEX trace_events_traj_seq  ON trace_events (trajectory_id, step_seq);
CREATE INDEX trace_events_workspace ON trace_events (workspace_id);
CREATE INDEX trace_events_kind      ON trace_events (kind);
