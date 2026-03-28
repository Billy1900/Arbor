//! Database access layer — uses dynamic queries to avoid needing
//! a live DB at compile time (no sqlx::query! macros).
use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use arbor_common::*;

pub struct Db {
    pool: PgPool,
}

impl Db {
    pub fn new(pool: PgPool) -> Self { Self { pool } }

    // ── Workspaces ───────────────────────────────────────────────────────────

    pub async fn insert_workspace(&self, ws: &Workspace) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspaces
             (id, name, state, repo_provider, repo_url, repo_ref, repo_commit,
              runner_class, vcpu_count, memory_mib, disk_gb, base_image_id,
              compatibility_key, identity_epoch, network_epoch, created_at, updated_at)
             VALUES ($1,$2,$3::workspace_state,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)"
        )
        .bind(ws.id.0)
        .bind(&ws.name)
        .bind(ws.state.to_string())
        .bind(&ws.repo.provider)
        .bind(&ws.repo.url)
        .bind(&ws.repo.r#ref)
        .bind(&ws.repo.commit)
        .bind(&ws.runtime.runner_class)
        .bind(ws.runtime.vcpu_count as i32)
        .bind(ws.runtime.memory_mib as i32)
        .bind(ws.runtime.disk_gb as i32)
        .bind("ubuntu-24.04-dev-v1")
        .bind(&ws.compatibility_key.0)
        .bind(ws.identity_epoch as i64)
        .bind(ws.network_epoch as i64)
        .bind(ws.created_at)
        .bind(ws.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        let row = sqlx::query(
            "SELECT id, name, state,
                    repo_provider, repo_url, repo_ref, repo_commit,
                    runner_class, vcpu_count, memory_mib, disk_gb,
                    compatibility_key, current_checkpoint_id, runner_id,
                    identity_epoch, network_epoch, created_at, updated_at
             FROM workspaces WHERE id = $1"
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let state_str: String = r.get("state");
            let state = match state_str.as_str() {
                "creating"      => WorkspaceState::Creating,
                "ready"         => WorkspaceState::Ready,
                "running"       => WorkspaceState::Running,
                "checkpointing" => WorkspaceState::Checkpointing,
                "restoring"     => WorkspaceState::Restoring,
                "quarantined"   => WorkspaceState::Quarantined,
                "terminating"   => WorkspaceState::Terminating,
                "terminated"    => WorkspaceState::Terminated,
                _               => WorkspaceState::Error,
            };
            Workspace {
                id,
                name: r.get("name"),
                state,
                repo: RepoConfig {
                    provider: r.get("repo_provider"),
                    url:      r.get("repo_url"),
                    r#ref:    r.get("repo_ref"),
                    commit:   r.get("repo_commit"),
                },
                runtime: RuntimeConfig {
                    runner_class: r.get("runner_class"),
                    vcpu_count:   r.get::<i32,_>("vcpu_count") as u32,
                    memory_mib:   r.get::<i32,_>("memory_mib") as u32,
                    disk_gb:      r.get::<i32,_>("disk_gb") as u32,
                },
                compatibility_key: CompatibilityKey(r.get("compatibility_key")),
                current_checkpoint_id: r.get::<Option<Uuid>,_>("current_checkpoint_id").map(CheckpointId),
                runner_id:             r.get::<Option<Uuid>,_>("runner_id").map(RunnerId),
                identity_epoch: r.get::<i64,_>("identity_epoch") as u64,
                network_epoch:  r.get::<i64,_>("network_epoch") as u64,
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            }
        }))
    }

    pub async fn update_workspace_state(&self, id: WorkspaceId, state: WorkspaceState) -> Result<()> {
        sqlx::query("UPDATE workspaces SET state = $1::workspace_state WHERE id = $2")
            .bind(state.to_string())
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_workspace_runner(&self, id: WorkspaceId, runner_id: RunnerId, vm_id: &str) -> Result<()> {
        sqlx::query("UPDATE workspaces SET runner_id = $1, vm_id = $2 WHERE id = $3")
            .bind(runner_id.0)
            .bind(vm_id)
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn bump_identity_epoch(&self, id: WorkspaceId) -> Result<u64> {
        let row = sqlx::query(
            "UPDATE workspaces
             SET identity_epoch = identity_epoch + 1, network_epoch = network_epoch + 1
             WHERE id = $1
             RETURNING identity_epoch"
        )
        .bind(id.0)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64,_>("identity_epoch") as u64)
    }

    pub async fn set_error(&self, id: WorkspaceId, message: &str) -> Result<()> {
        sqlx::query("UPDATE workspaces SET state = 'error'::workspace_state, error_message = $1 WHERE id = $2")
            .bind(message)
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Checkpoints ──────────────────────────────────────────────────────────

    pub async fn insert_checkpoint(&self, ckpt: &Checkpoint) -> Result<()> {
        sqlx::query(
            "INSERT INTO checkpoints
             (id, workspace_id, parent_id, name, state, compatibility_key,
              artifacts, resume_hooks_version, identity_epoch, network_epoch, created_at)
             VALUES ($1,$2,$3,$4,$5::checkpoint_state,$6,$7,$8,$9,$10,$11)"
        )
        .bind(ckpt.id.0)
        .bind(ckpt.workspace_id.0)
        .bind(ckpt.parent_id.map(|x| x.0))
        .bind(&ckpt.name)
        .bind(ckpt.state.to_string())
        .bind(&ckpt.compatibility_key.0)
        .bind(serde_json::to_value(&ckpt.artifacts)?)
        .bind(ckpt.resume_hooks_version as i32)
        .bind(ckpt.identity_epoch as i64)
        .bind(ckpt.network_epoch as i64)
        .bind(ckpt.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn seal_checkpoint(&self, id: CheckpointId, artifacts: &CheckpointArtifacts) -> Result<()> {
        sqlx::query("UPDATE checkpoints SET state = 'sealed'::checkpoint_state, artifacts = $1 WHERE id = $2")
            .bind(serde_json::to_value(artifacts)?)
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_checkpoint(&self, id: CheckpointId) -> Result<Option<Checkpoint>> {
        let row = sqlx::query(
            "SELECT id, workspace_id, parent_id, name, state, compatibility_key,
                    artifacts, resume_hooks_version, identity_epoch, network_epoch, created_at
             FROM checkpoints WHERE id = $1"
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let state_str: String = r.get("state");
            let state = match state_str.as_str() {
                "uploading" => CheckpointState::Uploading,
                "sealed"    => CheckpointState::Sealed,
                "failed"    => CheckpointState::Failed,
                _           => CheckpointState::Pending,
            };
            Checkpoint {
                id,
                workspace_id: WorkspaceId(r.get("workspace_id")),
                parent_id: r.get::<Option<Uuid>,_>("parent_id").map(CheckpointId),
                name: r.get("name"),
                state,
                compatibility_key: CompatibilityKey(r.get("compatibility_key")),
                artifacts: serde_json::from_value(r.get("artifacts")).unwrap_or_else(|_| CheckpointArtifacts::empty()),
                resume_hooks_version: r.get::<i32,_>("resume_hooks_version") as u32,
                identity_epoch: r.get::<i64,_>("identity_epoch") as u64,
                network_epoch:  r.get::<i64,_>("network_epoch") as u64,
                created_at: r.get("created_at"),
            }
        }))
    }

    // ── Operations ───────────────────────────────────────────────────────────

    pub async fn insert_operation(&self, op: &Operation) -> Result<()> {
        sqlx::query(
            "INSERT INTO operations (id, op_type, target_id, status, created_at, updated_at)
             VALUES ($1,$2::operation_type,$3,$4::operation_status,$5,$6)"
        )
        .bind(op.id.0)
        .bind(op.op_type.to_string())
        .bind(&op.target_id)
        .bind(op.status.to_string())
        .bind(op.created_at)
        .bind(op.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn complete_operation(&self, id: OperationId, success: bool, err: Option<&str>) -> Result<()> {
        let status = if success { "succeeded" } else { "failed" };
        let err_json = err.map(|e| serde_json::json!({ "message": e }));
        sqlx::query("UPDATE operations SET status = $1::operation_status, error_json = $2 WHERE id = $3")
            .bind(status)
            .bind(err_json)
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_operation(&self, id: OperationId) -> Result<Option<Value>> {
        let row = sqlx::query(
            "SELECT id, op_type, target_id, status, progress_pct, error_json, created_at, updated_at
             FROM operations WHERE id = $1"
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| serde_json::json!({
            "id":           r.get::<Uuid,_>("id"),
            "op_type":      r.get::<String,_>("op_type"),
            "target_id":    r.get::<String,_>("target_id"),
            "status":       r.get::<String,_>("status"),
            "progress_pct": r.get::<Option<i16>,_>("progress_pct"),
            "error":        r.get::<Option<Value>,_>("error_json"),
            "created_at":   r.get::<chrono::DateTime<Utc>,_>("created_at"),
            "updated_at":   r.get::<chrono::DateTime<Utc>,_>("updated_at"),
        })))
    }

    // ── Runner nodes ─────────────────────────────────────────────────────────

    pub async fn list_healthy_runners(&self, runner_class: &str) -> Result<Vec<RunnerNode>> {
        let rows = sqlx::query(
            "SELECT id, runner_class, address, arch, firecracker_version, cpu_template,
                    capacity_slots, used_slots, healthy, last_heartbeat
             FROM runner_nodes
             WHERE runner_class = $1 AND healthy = true AND used_slots < capacity_slots
             ORDER BY used_slots ASC"
        )
        .bind(runner_class)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| RunnerNode {
            id:                  RunnerId(r.get("id")),
            runner_class:        r.get("runner_class"),
            address:             r.get("address"),
            arch:                r.get("arch"),
            firecracker_version: r.get("firecracker_version"),
            cpu_template:        r.get("cpu_template"),
            capacity_slots:      r.get::<i32,_>("capacity_slots") as u32,
            used_slots:          r.get::<i32,_>("used_slots") as u32,
            healthy:             r.get("healthy"),
            last_heartbeat:      r.get("last_heartbeat"),
        }).collect())
    }

    pub async fn increment_runner_slots(&self, runner_id: RunnerId) -> Result<()> {
        sqlx::query("UPDATE runner_nodes SET used_slots = used_slots + 1 WHERE id = $1")
            .bind(runner_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn decrement_runner_slots(&self, runner_id: RunnerId) -> Result<()> {
        sqlx::query("UPDATE runner_nodes SET used_slots = GREATEST(0, used_slots - 1) WHERE id = $1")
            .bind(runner_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_runner(&self, node: &RunnerNode) -> Result<()> {
        sqlx::query(
            "INSERT INTO runner_nodes
             (id, runner_class, address, arch, firecracker_version, cpu_template, capacity_slots)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (id) DO UPDATE
             SET address = EXCLUDED.address, healthy = true, last_heartbeat = now()"
        )
        .bind(node.id.0)
        .bind(&node.runner_class)
        .bind(&node.address)
        .bind(&node.arch)
        .bind(&node.firecracker_version)
        .bind(&node.cpu_template)
        .bind(node.capacity_slots as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Sessions ─────────────────────────────────────────────────────────────

    pub async fn insert_session(&self, sess: &ExecSession) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, workspace_id, command, cwd, env_json, pty, status, reconnectable, started_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"
        )
        .bind(sess.id.0)
        .bind(sess.workspace_id.0)
        .bind(&sess.command)
        .bind(&sess.cwd)
        .bind(serde_json::to_value(&sess.env)?)
        .bind(sess.pty)
        .bind("starting")
        .bind(sess.reconnectable)
        .bind(sess.started_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
