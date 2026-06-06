/// M10 Trajectory Tracer — shared types for all crates.
///
/// TraceEvent is the union of all observable execution steps.
/// TraceEmitter is a fire-and-forget handle; callers never block on writes.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{CheckpointId, SessionId, WorkspaceId};

// ── ID newtypes ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrajectoryId(pub Uuid);

impl TrajectoryId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}
impl Default for TrajectoryId {
    fn default() -> Self { Self::new() }
}
impl std::fmt::Display for TrajectoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RolloutId(pub Uuid);

impl RolloutId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}
impl Default for RolloutId {
    fn default() -> Self { Self::new() }
}
impl std::fmt::Display for RolloutId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── TraceEvent ────────────────────────────────────────────────────────────────

/// Every observable step in a workspace's execution.
/// Serialized as the JSONB `payload` column in `trace_events`.
/// The `kind` column mirrors the serde tag string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// A session (exec call) was dispatched to the guest-agent.
    ExecStarted {
        session_id: SessionId,
        command:    Vec<String>,
        cwd:        String,
    },

    /// A session exited (success or failure).
    /// `stdout_tail` holds the last 4 KiB of combined output for quick attribution.
    ExecExited {
        session_id:  SessionId,
        exit_code:   i32,
        duration_ms: u64,
        stdout_tail: String,
    },

    /// File changes detected after an exec (unified diff, hard-truncated at 64 KiB).
    FileDiff {
        session_id:    Option<SessionId>,
        diff:          String,
        files_changed: Vec<String>,
    },

    /// An outbound network request passed the egress allowlist.
    NetworkAccess {
        host:           String,
        method:         String,
        status_code:    u16,
        bytes_sent:     u64,
        bytes_received: u64,
        /// true when the request used a brokered credential (the real key never entered the VM).
        brokered: bool,
    },

    /// An LLM API call completed; token counts and cost parsed from response headers.
    LlmCall {
        provider:          String,
        model:             String,
        prompt_tokens:     u32,
        completion_tokens: u32,
        cost_usd:          f64,
        latency_ms:        u64,
    },

    /// A full-VM checkpoint was created.
    CheckpointTaken {
        checkpoint_id: CheckpointId,
        name:          Option<String>,
    },

    /// This workspace was forked from a checkpoint, starting a new trajectory.
    WorkspaceForked {
        parent_checkpoint_id: CheckpointId,
        rollout_id:           Option<RolloutId>,
    },

    /// The reward command was evaluated at the end of a rollout attempt.
    RewardEvaluated {
        command:     String,
        exit_code:   i32,
        passed:      bool,
        stdout_tail: String,
    },
}

impl TraceEvent {
    /// Returns the `kind` string that will be stored in the DB column.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::ExecStarted { .. }    => "exec_started",
            Self::ExecExited { .. }     => "exec_exited",
            Self::FileDiff { .. }       => "file_diff",
            Self::NetworkAccess { .. }  => "network_access",
            Self::LlmCall { .. }        => "llm_call",
            Self::CheckpointTaken { .. }=> "checkpoint_taken",
            Self::WorkspaceForked { .. }=> "workspace_forked",
            Self::RewardEvaluated { .. }=> "reward_evaluated",
        }
    }
}

// ── TraceRecord ───────────────────────────────────────────────────────────────

/// A fully-qualified event ready to be written to the DB.
pub struct TraceRecord {
    pub trajectory_id: TrajectoryId,
    pub workspace_id:  WorkspaceId,
    pub step_seq:      u64,
    pub event:         TraceEvent,
    pub occurred_at:   DateTime<Utc>,
}

// ── TraceEmitter ──────────────────────────────────────────────────────────────

/// Fire-and-forget handle for one workspace's trajectory.
/// Clone is cheap; the underlying channel is shared.
/// `emit` never blocks — the background writer in `TrajectoryStore` drains the channel.
#[derive(Clone)]
pub struct TraceEmitter {
    pub trajectory_id: TrajectoryId,
    pub workspace_id:  WorkspaceId,
    seq: Arc<AtomicU64>,
    tx:  mpsc::UnboundedSender<TraceRecord>,
}

impl TraceEmitter {
    pub fn new(
        trajectory_id: TrajectoryId,
        workspace_id:  WorkspaceId,
        tx: mpsc::UnboundedSender<TraceRecord>,
    ) -> Self {
        Self { trajectory_id, workspace_id, seq: Arc::new(AtomicU64::new(0)), tx }
    }

    /// Emit an event. Never blocks; silently drops if the channel is closed.
    pub fn emit(&self, event: TraceEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(TraceRecord {
            trajectory_id: self.trajectory_id,
            workspace_id:  self.workspace_id,
            step_seq:      seq,
            event,
            occurred_at:   Utc::now(),
        });
    }
}

// ── Query result types (returned by TrajectoryStore) ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRow {
    pub id:               TrajectoryId,
    pub workspace_id:     WorkspaceId,
    pub rollout_id:       Option<RolloutId>,
    pub parent_id:        Option<TrajectoryId>,
    pub started_at:       DateTime<Utc>,
    pub ended_at:         Option<DateTime<Utc>>,
    pub reward_passed:    Option<bool>,
    pub reward_exit_code: Option<i32>,
    pub step_count:       i32,
    pub total_cost_usd:   f64,
    pub wall_time_ms:     Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEventRow {
    pub id:            Uuid,
    pub trajectory_id: TrajectoryId,
    pub workspace_id:  WorkspaceId,
    pub step_seq:      i64,
    pub kind:          String,
    pub occurred_at:   DateTime<Utc>,
    pub payload:       serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutRow {
    pub id:                   RolloutId,
    pub source_checkpoint_id: CheckpointId,
    pub reward_command:       String,
    pub fork_count:           i32,
    pub started_at:           DateTime<Utc>,
    pub completed_at:         Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutSummary {
    pub rollout:  RolloutRow,
    pub attempts: Vec<TrajectoryRow>,
}

/// Comparison between two trajectories at the step level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajDiff {
    pub traj_a: TrajectoryId,
    pub traj_b: TrajectoryId,
    pub divergence_step: Option<u64>,
    pub only_in_a: Vec<TraceEventRow>,
    pub only_in_b: Vec<TraceEventRow>,
    pub common_steps: u64,
}
