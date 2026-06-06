/// M10 Trajectory Store — DB write layer and query API.
///
/// The store owns a background writer task that batch-inserts trace events
/// every 50 ms or when 100 events accumulate. Writers never block callers —
/// TraceEmitter::emit is fire-and-forget and drops the record only if the receiver is closed.
#[allow(dead_code)]
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use futures::future::Future;
use sqlx::{PgPool, Row as _};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use arbor_common::{
    CheckpointId, RolloutId, RolloutRow, RolloutSummary, TrajDiff, TraceEmitter, TraceEvent,
    TraceEventRow, TraceRecord, TrajectoryId, TrajectoryRow, WorkspaceId,
};

// ── TrajectoryStore ───────────────────────────────────────────────────────────

pub struct TrajectoryStore {
    pool:     PgPool,
    tx:       mpsc::UnboundedSender<TraceRecord>,
    /// Per-workspace emitters so controller modules can find the right handle.
    emitters: DashMap<WorkspaceId, TraceEmitter>,
}

impl TrajectoryStore {
    /// Construct the store and return the writer future that must be spawned.
    pub fn new(pool: PgPool) -> (Arc<Self>, impl Future<Output = ()> + Send + 'static) {
        let (tx, rx) = mpsc::unbounded_channel::<TraceRecord>();
        let store = Arc::new(Self {
            pool: pool.clone(),
            tx,
            emitters: DashMap::new(),
        });
        let writer = run_writer(pool, rx);
        (store, writer)
    }

    // ── Trajectory lifecycle ──────────────────────────────────────────────────

    /// Create a new trajectory row and return an emitter for it.
    /// Called by `fork_checkpoint` and `create_workspace` (standalone trajectories).
    pub async fn open_trajectory(
        &self,
        workspace_id: WorkspaceId,
        rollout_id:   Option<RolloutId>,
        parent_id:    Option<TrajectoryId>,
    ) -> Result<TraceEmitter> {
        let tid = TrajectoryId::new();
        sqlx::query(
            "INSERT INTO trajectories (id, workspace_id, rollout_id, parent_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(tid.0)
        .bind(workspace_id.0)
        .bind(rollout_id.map(|r| r.0))
        .bind(parent_id.map(|p| p.0))
        .execute(&self.pool)
        .await?;

        let emitter = TraceEmitter::new(tid, workspace_id, self.tx.clone());
        self.emitters.insert(workspace_id, emitter.clone());
        Ok(emitter)
    }

    /// Look up the emitter for a workspace (returns None if no open trajectory).
    pub fn emitter_for(&self, ws_id: WorkspaceId) -> Option<TraceEmitter> {
        self.emitters.get(&ws_id).map(|e| e.clone())
    }

    /// Finalise a trajectory with reward outcome and timing.
    pub async fn close_trajectory(
        &self,
        tid:       TrajectoryId,
        passed:    bool,
        exit_code: i32,
        wall_ms:   i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE trajectories
             SET ended_at = now(), reward_passed = $2,
                 reward_exit_code = $3, wall_time_ms = $4
             WHERE id = $1",
        )
        .bind(tid.0)
        .bind(passed)
        .bind(exit_code)
        .bind(wall_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove the emitter from the registry when a workspace terminates.
    pub fn drop_emitter(&self, ws_id: WorkspaceId) {
        self.emitters.remove(&ws_id);
    }

    // ── Rollout lifecycle ─────────────────────────────────────────────────────

    pub async fn create_rollout(
        &self,
        source_checkpoint_id: CheckpointId,
        reward_command:       &str,
        fork_count:           i32,
    ) -> Result<RolloutId> {
        let rid = RolloutId::new();
        sqlx::query(
            "INSERT INTO rollouts (id, source_checkpoint_id, reward_command, fork_count)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(rid.0)
        .bind(source_checkpoint_id.0)
        .bind(reward_command)
        .bind(fork_count)
        .execute(&self.pool)
        .await?;
        Ok(rid)
    }

    pub async fn complete_rollout(&self, rid: RolloutId) -> Result<()> {
        sqlx::query("UPDATE rollouts SET completed_at = now() WHERE id = $1")
            .bind(rid.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub async fn get_trajectory(&self, tid: TrajectoryId) -> Result<Option<TrajectoryRow>> {
        let row = sqlx::query(
            "SELECT id, workspace_id, rollout_id, parent_id,
                    started_at, ended_at, reward_passed, reward_exit_code,
                    step_count, total_cost_usd, wall_time_ms
             FROM trajectories WHERE id = $1",
        )
        .bind(tid.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| trajectory_row_from_pg(&r)))
    }

    pub async fn get_trajectory_for_workspace(
        &self,
        ws_id: WorkspaceId,
    ) -> Result<Option<TrajectoryRow>> {
        let row = sqlx::query(
            "SELECT id, workspace_id, rollout_id, parent_id,
                    started_at, ended_at, reward_passed, reward_exit_code,
                    step_count, total_cost_usd, wall_time_ms
             FROM trajectories WHERE workspace_id = $1
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(ws_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| trajectory_row_from_pg(&r)))
    }

    pub async fn list_events(
        &self,
        tid:       TrajectoryId,
        limit:     i64,
        after_seq: i64,
    ) -> Result<Vec<TraceEventRow>> {
        let rows = sqlx::query(
            "SELECT id, trajectory_id, workspace_id, step_seq, kind, occurred_at, payload
             FROM trace_events
             WHERE trajectory_id = $1 AND step_seq > $2
             ORDER BY step_seq ASC
             LIMIT $3",
        )
        .bind(tid.0)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(trace_event_row_from_pg).collect())
    }

    pub async fn get_rollout(&self, rid: RolloutId) -> Result<Option<RolloutSummary>> {
        let rollout_row = sqlx::query(
            "SELECT id, source_checkpoint_id, reward_command, fork_count, started_at, completed_at
             FROM rollouts WHERE id = $1",
        )
        .bind(rid.0)
        .fetch_optional(&self.pool)
        .await?;

        let Some(rr) = rollout_row else { return Ok(None) };

        let attempts_rows = sqlx::query(
            "SELECT id, workspace_id, rollout_id, parent_id,
                    started_at, ended_at, reward_passed, reward_exit_code,
                    step_count, total_cost_usd, wall_time_ms
             FROM trajectories WHERE rollout_id = $1
             ORDER BY started_at ASC",
        )
        .bind(rid.0)
        .fetch_all(&self.pool)
        .await?;

        Ok(Some(RolloutSummary {
            rollout: RolloutRow {
                id:                   RolloutId(rr.try_get("id").unwrap()),
                source_checkpoint_id: CheckpointId(rr.try_get("source_checkpoint_id").unwrap()),
                reward_command:       rr.try_get("reward_command").unwrap(),
                fork_count:           rr.try_get("fork_count").unwrap(),
                started_at:           rr.try_get("started_at").unwrap(),
                completed_at:         rr.try_get("completed_at").unwrap(),
            },
            attempts: attempts_rows.iter().map(|r| trajectory_row_from_pg(r)).collect(),
        }))
    }

    /// Stream all events for a trajectory as newline-delimited JSON.
    pub async fn export_jsonl(
        &self,
        tid:    TrajectoryId,
        writer: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut after_seq: i64 = -1;
        loop {
            let batch = self.list_events(tid, 200, after_seq).await?;
            if batch.is_empty() { break; }
            after_seq = batch.last().unwrap().step_seq;
            for row in &batch {
                let line = serde_json::to_vec(&row)?;
                writer.write_all(&line).await?;
                writer.write_all(b"\n").await?;
            }
        }
        Ok(())
    }

    /// Find the checkpoint_id from the most recent CheckpointTaken event at or
    /// before `from_step`, so replay can fork from there.
    pub async fn find_replay_checkpoint(
        &self,
        tid:       TrajectoryId,
        from_step: i64,
    ) -> Result<Option<CheckpointId>> {
        let row = sqlx::query(
            "SELECT payload->>'checkpoint_id' AS ckpt_id
             FROM trace_events
             WHERE trajectory_id = $1 AND kind = 'checkpoint_taken' AND step_seq <= $2
             ORDER BY step_seq DESC LIMIT 1",
        )
        .bind(tid.0)
        .bind(from_step)
        .fetch_optional(&self.pool)
        .await?;

        let ckpt_str: Option<String> = row.and_then(|r| r.try_get("ckpt_id").ok());
        let ckpt = ckpt_str
            .and_then(|s| Uuid::parse_str(&s).ok())
            .map(CheckpointId);
        Ok(ckpt)
    }

    /// Simple cross-fork diff over the first 1000 events: find the first step_seq
    /// where the two trajectories' event `kind` differs (payload is not compared).
    pub async fn diff_trajectories(
        &self,
        traj_a: TrajectoryId,
        traj_b: TrajectoryId,
    ) -> Result<TrajDiff> {
        let events_a = self.list_events(traj_a, 1000, -1).await?;
        let events_b = self.list_events(traj_b, 1000, -1).await?;

        let common = events_a
            .iter()
            .zip(events_b.iter())
            .take_while(|(a, b)| a.kind == b.kind)
            .count() as u64;

        let divergence_step = if common < events_a.len().min(events_b.len()) as u64 {
            Some(common)
        } else {
            None
        };

        let only_in_a: Vec<_> = events_a.into_iter().skip(common as usize).collect();
        let only_in_b: Vec<_> = events_b.into_iter().skip(common as usize).collect();

        Ok(TrajDiff {
            traj_a,
            traj_b,
            divergence_step,
            only_in_a,
            only_in_b,
            common_steps: common,
        })
    }
}

// ── Background batch writer ───────────────────────────────────────────────────

async fn run_writer(pool: PgPool, mut rx: mpsc::UnboundedReceiver<TraceRecord>) {
    let mut buf: Vec<TraceRecord> = Vec::with_capacity(100);
    let mut ticker = tokio::time::interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            Some(r) = rx.recv() => {
                buf.push(r);
                if buf.len() >= 100 {
                    flush(&pool, &mut buf).await;
                }
            }
            _ = ticker.tick() => {
                if !buf.is_empty() {
                    flush(&pool, &mut buf).await;
                }
            }
        }
    }
}

async fn flush(pool: &PgPool, buf: &mut Vec<TraceRecord>) {
    if buf.is_empty() { return; }

    // Build a bulk INSERT using raw query construction.
    // Failure is non-fatal — trace events are best-effort.
    let mut q = String::from(
        "INSERT INTO trace_events (trajectory_id, workspace_id, step_seq, kind, occurred_at, payload) VALUES ",
    );
    let mut params: Vec<Box<dyn sqlx::Encode<'_, sqlx::Postgres> + Send + Sync>> = Vec::new();

    // sqlx doesn't support truly dynamic bulk binds easily, so we INSERT one by one
    // inside a transaction. For production scale, switch to COPY.
    let result = async {
        let mut tx = pool.begin().await?;
        for r in buf.iter() {
            let payload = serde_json::to_value(&r.event)?;
            sqlx::query(
                "INSERT INTO trace_events
                 (trajectory_id, workspace_id, step_seq, kind, occurred_at, payload)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT DO NOTHING",
            )
            .bind(r.trajectory_id.0)
            .bind(r.workspace_id.0)
            .bind(r.step_seq as i64)
            .bind(r.event.kind_str())
            .bind(r.occurred_at)
            .bind(payload)
            .execute(&mut *tx)
            .await?;

            // Increment step_count on the trajectory row
            sqlx::query(
                "UPDATE trajectories SET step_count = step_count + 1 WHERE id = $1",
            )
            .bind(r.trajectory_id.0)
            .execute(&mut *tx)
            .await?;

            // Accumulate cost if this is an LlmCall
            if let TraceEvent::LlmCall { cost_usd, .. } = &r.event {
                sqlx::query(
                    "UPDATE trajectories SET total_cost_usd = total_cost_usd + $1 WHERE id = $2",
                )
                .bind(*cost_usd)
                .bind(r.trajectory_id.0)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        warn!(?e, flushed = buf.len(), "trace_events flush failed — events dropped");
    } else {
        debug!(flushed = buf.len(), "trace_events flushed");
    }

    // Drop the params binding we don't use (just to silence the unused warning)
    drop(q);
    drop(params);

    buf.clear();
}

// ── sqlx row helpers ──────────────────────────────────────────────────────────

fn trajectory_row_from_pg(r: &sqlx::postgres::PgRow) -> TrajectoryRow {
    TrajectoryRow {
        id:               TrajectoryId(r.try_get("id").unwrap()),
        workspace_id:     WorkspaceId(r.try_get("workspace_id").unwrap()),
        rollout_id:       r.try_get::<Option<Uuid>, _>("rollout_id").unwrap().map(RolloutId),
        parent_id:        r.try_get::<Option<Uuid>, _>("parent_id").unwrap().map(TrajectoryId),
        started_at:       r.try_get("started_at").unwrap(),
        ended_at:         r.try_get("ended_at").unwrap(),
        reward_passed:    r.try_get("reward_passed").unwrap(),
        reward_exit_code: r.try_get("reward_exit_code").unwrap(),
        step_count:       r.try_get("step_count").unwrap(),
        total_cost_usd:   r.try_get::<f64, _>("total_cost_usd").unwrap_or(0.0),
        wall_time_ms:     r.try_get("wall_time_ms").unwrap(),
    }
}

fn trace_event_row_from_pg(r: &sqlx::postgres::PgRow) -> TraceEventRow {
    TraceEventRow {
        id:            r.try_get("id").unwrap(),
        trajectory_id: TrajectoryId(r.try_get("trajectory_id").unwrap()),
        workspace_id:  WorkspaceId(r.try_get("workspace_id").unwrap()),
        step_seq:      r.try_get("step_seq").unwrap(),
        kind:          r.try_get("kind").unwrap(),
        occurred_at:   r.try_get("occurred_at").unwrap(),
        payload:       r.try_get("payload").unwrap(),
    }
}
