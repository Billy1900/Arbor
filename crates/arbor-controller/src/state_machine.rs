#![allow(dead_code)]
//! Workspace state machine and operation orchestration.
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use std::sync::Arc;
use tracing::{error, info, instrument, warn};

use arbor_common::*;
use arbor_common::proto::{
    CreateVmRequest, VmCheckpointRequest, VmExecRequest, VmRestoreRequest,
};

use crate::db::Db;
use crate::grant_registry::GrantRegistry;
use crate::reseal::{run_reseal_hooks, EnvSecretResolver, ResealContext};
use crate::runner_client::RunnerClient;
use crate::scheduler::Scheduler;
use crate::snapshot::SnapshotService;

pub struct Controller {
    pub db:       Arc<Db>,
    pub scheduler: Arc<Scheduler>,
    pub config:   Arc<ControllerConfig>,
    pub snapshot: Arc<SnapshotService>,
    pub grants:   Arc<GrantRegistry>,
}

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub base_images_dir:    String,
    pub kernel_path:        String,
    pub default_runner_class: String,
    pub attach_token_secret: String,
    pub api_base_url:       String,
    pub object_store_prefix: String,
}

impl Controller {
    pub fn new(
        db:       Arc<Db>,
        scheduler: Arc<Scheduler>,
        config:   Arc<ControllerConfig>,
        snapshot: Arc<SnapshotService>,
        grants:   Arc<GrantRegistry>,
    ) -> Self {
        Self { db, scheduler, config, snapshot, grants }
    }

    // ── Create workspace ──────────────────────────────────────────────────────

    #[instrument(skip(self, req), fields(workspace_name = %req.name))]
    pub async fn create_workspace(self: Arc<Self>, req: CreateWorkspaceRequest)
        -> Result<(Workspace, Operation)>
    {
        let now   = Utc::now();
        let ws_id = WorkspaceId::new();
        let op_id = OperationId::new();

        let compat_key = CompatibilityKey::new(
            &req.runtime.runner_class, "x86_64", "T2", "1.9.0",
            "sha256:placeholder", &req.image.base_image_id, 1,
        );
        let workspace = Workspace {
            id: ws_id, name: req.name.clone(), state: WorkspaceState::Creating,
            repo: req.repo.clone(), runtime: req.runtime.clone(),
            compatibility_key: compat_key, current_checkpoint_id: None,
            runner_id: None, identity_epoch: 0, network_epoch: 0,
            created_at: now, updated_at: now,
        };
        let op = make_op(op_id, OperationType::CreateWorkspace, ws_id.to_string());

        self.db.insert_workspace(&workspace).await?;
        self.db.insert_operation(&op).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_create_workspace(ws_id, op_id).await {
                error!(?e, %ws_id, "create_workspace failed");
                let _ = ctrl.db.set_error(ws_id, &e.to_string()).await;
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });
        Ok((workspace, op))
    }

    async fn drive_create_workspace(&self, ws_id: WorkspaceId, op_id: OperationId) -> Result<()> {
        let ws = self.db.get_workspace(ws_id).await?.context("workspace vanished")?;
        let runner = self.scheduler.pick_runner(&ws.runtime.runner_class, &ws.compatibility_key).await?;
        self.db.increment_runner_slots(runner.id).await?;

        let rootfs_path = format!("/var/lib/arbor/workspaces/{}/rootfs.overlay.raw", ws_id);
        let vsock_uds   = format!("/var/lib/arbor/workspaces/{}/vm.vsock", ws_id);

        self.db.update_operation_progress(op_id, 20).await?;

        let client = RunnerClient::new(&runner.address);
        let vm_resp = client.create_vm(CreateVmRequest {
            workspace_id: ws_id.to_string(),
            vcpu_count:   ws.runtime.vcpu_count,
            memory_mib:   ws.runtime.memory_mib,
            disk_gb:      ws.runtime.disk_gb,
            kernel_path:  self.config.kernel_path.clone(),
            rootfs_path,
            tap_device:   tap_name_for(ws_id),
            vsock_uds_path: vsock_uds,
            base_image_id:  "ubuntu-24.04-dev-v1".into(),
            repo_url:     ws.repo.url.clone(),
            repo_ref:     ws.repo.r#ref.clone(),
        }).await.context("runner create_vm failed")?;

        self.db.update_workspace_runner(ws_id, runner.id, &vm_resp.vm_id).await?;
        self.db.update_operation_progress(op_id, 90).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Ready).await?;
        self.db.complete_operation(op_id, true, None).await?;
        info!(%ws_id, vm_id = %vm_resp.vm_id, "workspace ready");
        Ok(())
    }

    // ── Exec session ──────────────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn exec_session(&self, ws_id: WorkspaceId, req: ExecRequest) -> Result<ExecSession> {
        let ws = self.require_workspace(ws_id).await?;
        self.assert_state(&ws, &[WorkspaceState::Ready, WorkspaceState::Running])?;

        let sess_id = SessionId::new();
        let now     = Utc::now();
        let session = ExecSession {
            id: sess_id, workspace_id: ws_id,
            command: req.command.clone(), cwd: req.cwd.clone(), env: req.env.clone(),
            pty: req.pty, status: SessionStatus::Starting,
            exit_code: None, reconnectable: true, started_at: now,
        };
        self.db.insert_session(&session).await?;

        let runner = self.get_runner_for(&ws).await?;
        let vm_id  = self.db.get_vm_id(ws_id).await?.unwrap_or_default();
        RunnerClient::new(&runner.address)
            .vm_exec(VmExecRequest {
                session_id: sess_id, command: req.command, cwd: req.cwd,
                env: req.env, pty: req.pty, cols: req.cols, rows: req.rows,
            })
            .await
            .context("runner vm_exec failed")?;

        if ws.state == WorkspaceState::Ready {
            self.db.update_workspace_state(ws_id, WorkspaceState::Running).await?;
        }
        Ok(session)
    }

    // ── Terminate workspace ───────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn terminate_workspace(self: Arc<Self>, ws_id: WorkspaceId) -> Result<Operation> {
        let ws    = self.require_workspace(ws_id).await?;
        let op_id = OperationId::new();
        let op    = make_op(op_id, OperationType::TerminateWorkspace, ws_id.to_string());
        self.db.insert_operation(&op).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Terminating).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_terminate(ws_id, op_id, ws).await {
                error!(?e, %ws_id, "terminate failed");
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });
        Ok(op)
    }

    async fn drive_terminate(&self, ws_id: WorkspaceId, op_id: OperationId, ws: Workspace) -> Result<()> {
        // Revoke all grants immediately
        self.grants.revoke_all_for_workspace(ws_id);
        self.db.revoke_all_grants(ws_id).await?;

        if let Some(runner_id) = ws.runner_id {
            if let Ok(runner) = self.scheduler.get_runner(runner_id).await {
                let _ = RunnerClient::new(&runner.address).destroy_vm(&ws_id.to_string()).await;
            }
            self.db.decrement_runner_slots(runner_id).await?;
        }
        self.db.update_workspace_state(ws_id, WorkspaceState::Terminated).await?;
        self.db.complete_operation(op_id, true, None).await?;
        info!(%ws_id, "workspace terminated");
        Ok(())
    }

    // ── Create checkpoint (M3 — uploads to S3) ────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn create_checkpoint(
        self: Arc<Self>, ws_id: WorkspaceId, req: CheckpointRequest,
    ) -> Result<(Checkpoint, Operation)> {
        let ws     = self.require_workspace(ws_id).await?;
        self.assert_state(&ws, &[WorkspaceState::Ready, WorkspaceState::Running])?;

        let ckpt_id = CheckpointId::new();
        let op_id   = OperationId::new();
        let now     = Utc::now();

        let ckpt = Checkpoint {
            id: ckpt_id, workspace_id: ws_id, parent_id: ws.current_checkpoint_id,
            name: req.name.clone(), state: CheckpointState::Pending,
            compatibility_key: ws.compatibility_key.clone(),
            artifacts: CheckpointArtifacts::empty(), resume_hooks_version: 1,
            identity_epoch: ws.identity_epoch, network_epoch: ws.network_epoch,
            created_at: now,
        };
        let op = make_op(op_id, OperationType::CreateCheckpoint, ckpt_id.to_string());

        self.db.insert_checkpoint(&ckpt).await?;
        self.db.insert_operation(&op).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Checkpointing).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_checkpoint(ws_id, ckpt_id, op_id, ws).await {
                error!(?e, %ws_id, %ckpt_id, "checkpoint failed");
                let _ = ctrl.db.fail_checkpoint(ckpt_id).await;
                let _ = ctrl.db.update_workspace_state(ws_id, WorkspaceState::Error).await;
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });
        Ok((ckpt, op))
    }

    async fn drive_checkpoint(
        &self, ws_id: WorkspaceId, ckpt_id: CheckpointId, op_id: OperationId, ws: Workspace,
    ) -> Result<()> {
        let runner_id = ws.runner_id.ok_or_else(|| anyhow!("no runner assigned"))?;
        let runner = self.scheduler.get_runner(runner_id).await?;
        let client = RunnerClient::new(&runner.address);
        let snap_dir = format!("/var/lib/arbor/snapshots/cache/{}", ckpt_id);

        // Step 1: suspend + snapshot via runner
        self.db.update_operation_progress(op_id, 10).await?;
        info!(%ws_id, %ckpt_id, "requesting checkpoint from runner");
        let snap_resp = client.checkpoint_vm(&ws_id.to_string(), VmCheckpointRequest {
            checkpoint_id: ckpt_id.to_string(),
            snapshot_dir:  snap_dir,
        }).await.context("runner checkpoint_vm failed")?;

        // Step 2: upload artifacts to object store (M3)
        self.db.update_operation_progress(op_id, 40).await?;
        info!(%ckpt_id, "uploading checkpoint artifacts to object store");
        let artifacts = self.snapshot
            .upload_and_seal(ckpt_id, &snap_resp.state_path, &snap_resp.mem_path)
            .await
            .context("snapshot upload failed")?;

        // Step 3: seal checkpoint in DB
        self.db.update_operation_progress(op_id, 90).await?;
        self.db.seal_checkpoint(ckpt_id, &artifacts).await?;
        self.db.update_current_checkpoint(ws_id, ckpt_id).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Ready).await?;
        self.db.complete_operation(op_id, true, None).await?;

        info!(%ws_id, %ckpt_id, "checkpoint sealed and uploaded");
        metrics::counter!("arbor.checkpoint.created").increment(1);
        Ok(())
    }

    // ── Restore checkpoint (M4) ───────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn restore_checkpoint(
        self: Arc<Self>, ckpt_id: CheckpointId, req: RestoreRequest,
    ) -> Result<(Workspace, Operation)> {
        let ckpt = self.db.get_checkpoint(ckpt_id).await?
            .ok_or_else(|| anyhow!(ArborError::CheckpointNotFound(ckpt_id.to_string())))?;

        if ckpt.state != CheckpointState::Sealed {
            bail!(ArborError::CheckpointNotSealed(ckpt_id.to_string()));
        }

        let now   = Utc::now();
        let ws_id = WorkspaceId::new();
        let op_id = OperationId::new();

        let new_ws = Workspace {
            id:    ws_id,
            name:  req.workspace_name.unwrap_or_else(|| format!("restore-{}", &ckpt_id.to_string()[..8])),
            state: WorkspaceState::Restoring,
            repo:  RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig {
                runner_class: ckpt.compatibility_key.0["runner_class"]
                    .as_str().unwrap_or("fc-x86_64-v1").to_string(),
                vcpu_count: 2, memory_mib: 2048, disk_gb: 30,
            },
            compatibility_key: ckpt.compatibility_key.clone(),
            current_checkpoint_id: Some(ckpt_id),
            runner_id: None, identity_epoch: 0, network_epoch: 0,
            created_at: now, updated_at: now,
        };
        let op = make_op(op_id, OperationType::RestoreCheckpoint, ws_id.to_string());

        self.db.insert_workspace(&new_ws).await?;
        self.db.insert_operation(&op).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_restore(ws_id, op_id, ckpt, true).await {
                error!(?e, %ws_id, "restore failed");
                let _ = ctrl.db.set_error(ws_id, &e.to_string()).await;
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });
        Ok((new_ws, op))
    }

    // ── Fork checkpoint (M4) ─────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn fork_checkpoint(
        self: Arc<Self>, ckpt_id: CheckpointId, req: RestoreRequest,
    ) -> Result<(Workspace, Operation)> {
        let ckpt = self.db.get_checkpoint(ckpt_id).await?
            .ok_or_else(|| anyhow!(ArborError::CheckpointNotFound(ckpt_id.to_string())))?;

        if ckpt.state != CheckpointState::Sealed {
            bail!(ArborError::CheckpointNotSealed(ckpt_id.to_string()));
        }

        let now   = Utc::now();
        let ws_id = WorkspaceId::new();
        let op_id = OperationId::new();

        let fork_ws = Workspace {
            id:    ws_id,
            name:  req.workspace_name.unwrap_or_else(|| format!("fork-{}", &ckpt_id.to_string()[..8])),
            state: WorkspaceState::Restoring,
            repo:  RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig {
                runner_class: ckpt.compatibility_key.0["runner_class"]
                    .as_str().unwrap_or("fc-x86_64-v1").to_string(),
                vcpu_count: 2, memory_mib: 2048, disk_gb: 30,
            },
            compatibility_key: ckpt.compatibility_key.clone(),
            current_checkpoint_id: Some(ckpt_id),
            runner_id: None, identity_epoch: 0, network_epoch: 0,
            created_at: now, updated_at: now,
        };
        let op = make_op(op_id, OperationType::ForkCheckpoint, ws_id.to_string());

        self.db.insert_workspace(&fork_ws).await?;
        self.db.insert_operation(&op).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_restore(ws_id, op_id, ckpt, true).await {
                error!(?e, %ws_id, "fork failed");
                let _ = ctrl.db.set_error(ws_id, &e.to_string()).await;
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });

        metrics::counter!("arbor.fork.created").increment(1);
        Ok((fork_ws, op))
    }

    /// Core restore driver — shared by restore and fork.
    /// Always quarantines then reseals before releasing the workspace.
    async fn drive_restore(
        &self,
        ws_id:    WorkspaceId,
        op_id:    OperationId,
        ckpt:     Checkpoint,
        quarantine: bool,
    ) -> Result<()> {
        // Step 1: pick a compatible runner (MUST match compat key)
        self.db.update_operation_progress(op_id, 10).await?;
        let runner = self.scheduler.pick_compatible_runner(&ckpt).await?;
        self.db.increment_runner_slots(runner.id).await?;
        self.db.update_workspace_runner(ws_id, runner.id, "restoring").await?;

        // Step 2: download snapshot artifacts to runner-local cache
        self.db.update_operation_progress(op_id, 20).await?;
        let artifacts = &ckpt.artifacts;
        let ckpt_id   = ckpt.id;

        let local_state = format!("/var/lib/arbor/snapshots/cache/{}/state.snap", ckpt_id);
        let local_mem   = format!("/var/lib/arbor/snapshots/cache/{}/mem.snap",   ckpt_id);

        self.snapshot.download_state(ckpt_id, &local_state, artifacts.state_digest.as_deref()).await
            .context("download state.snap")?;
        // mem file must stay alive for the VM's lifetime (MAP_PRIVATE semantics)
        self.snapshot.download_mem(ckpt_id, &local_mem, artifacts.mem_digest.as_deref()).await
            .context("download mem.snap")?;

        // Step 3: restore VM on runner (stays quarantined on boot)
        self.db.update_operation_progress(op_id, 50).await?;
        info!(%ws_id, %ckpt_id, "restoring VM on runner {}", runner.id);
        let vm_resp = RunnerClient::new(&runner.address)
            .restore_vm(VmRestoreRequest {
                workspace_id: ws_id.to_string(),
                checkpoint_id: ckpt_id.to_string(),
                state_path:   local_state,
                mem_path:     local_mem,
                tap_device:   tap_name_for(ws_id),
                vsock_uds_path: format!("/var/lib/arbor/workspaces/{}/vm.vsock", ws_id),
                vcpu_count:   2,
                memory_mib:   ckpt.compatibility_key.0["mem_mib"]
                                  .as_u64().unwrap_or(2048) as u32,
            })
            .await
            .context("runner restore_vm failed")?;

        self.db.update_workspace_runner(ws_id, runner.id, &vm_resp.vm_id).await?;

        // Step 4: enter quarantine — block egress + attach
        self.db.update_workspace_state(ws_id, WorkspaceState::Quarantined).await?;
        self.db.update_operation_progress(op_id, 70).await?;
        info!(%ws_id, "workspace quarantined — running reseal hooks");

        // Step 5: run reseal hooks (M4 — the core differentiator)
        let resolver = Arc::new(EnvSecretResolver);
        let ctx = ResealContext {
            ws_id,
            runner_addr: runner.address.clone(),
            db:    Arc::clone(&self.db),
            grants: Arc::clone(&self.grants),
            secret_vals: resolver,
        };
        let new_epoch = run_reseal_hooks(&ctx).await
            .context("reseal hooks failed — workspace remains quarantined")?;

        // Step 6: release to READY
        self.db.update_operation_progress(op_id, 95).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Ready).await?;
        self.db.complete_operation(op_id, true, None).await?;

        info!(%ws_id, epoch = new_epoch, "workspace restored and resealed — READY");
        metrics::counter!("arbor.restore.completed").increment(1);
        Ok(())
    }

    // ── Build attach URL ──────────────────────────────────────────────────────

    pub async fn build_attach_url(&self, ws_id: WorkspaceId, sess_id: SessionId) -> Result<String> {
        let token = sign_attach_token(ws_id, sess_id, &self.config.attach_token_secret);
        Ok(format!("{}/v1/attach/{}?token={}", self.config.api_base_url, sess_id, token))
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    pub async fn require_workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        self.db.get_workspace(id).await?
            .ok_or_else(|| anyhow!(ArborError::WorkspaceNotFound(id.to_string())))
    }

    fn assert_state(&self, ws: &Workspace, allowed: &[WorkspaceState]) -> Result<()> {
        if !allowed.contains(&ws.state) {
            bail!(ArborError::WorkspaceBusy { state: ws.state.to_string() });
        }
        Ok(())
    }

    async fn get_runner_for(&self, ws: &Workspace) -> Result<RunnerNode> {
        let runner_id = ws.runner_id.ok_or_else(|| anyhow!("no runner assigned"))?;
        self.scheduler.get_runner(runner_id).await
    }
}

// ── Token signing ─────────────────────────────────────────────────────────────

fn sign_attach_token(ws_id: WorkspaceId, sess_id: SessionId, secret: &str) -> String {
    use sha2::{Sha256, Digest};
    let expires = Utc::now().timestamp() + 900;
    let payload = format!("{}.{}.{}", ws_id, sess_id, expires);
    let mut h   = Sha256::new();
    h.update(secret.as_bytes());
    h.update(payload.as_bytes());
    format!("{}.{}", payload, &hex::encode(h.finalize())[..16])
}

pub fn verify_attach_token(token: &str, secret: &str) -> Option<(WorkspaceId, SessionId)> {
    use sha2::{Sha256, Digest};
    let parts: Vec<&str> = token.rsplitn(2, '.').collect();
    if parts.len() < 2 { return None; }
    let (sig, payload) = (parts[0], parts[1]);
    let segs: Vec<&str> = payload.split('.').collect();
    if segs.len() < 3 { return None; }
    let expires: i64 = segs[2].parse().ok()?;
    if Utc::now().timestamp() > expires { return None; }
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.update(payload.as_bytes());
    if &hex::encode(h.finalize())[..16] != sig { return None; }
    let ws_id   = uuid::Uuid::parse_str(segs[0]).ok().map(WorkspaceId)?;
    let sess_id = uuid::Uuid::parse_str(segs[1]).ok().map(SessionId)?;
    Some((ws_id, sess_id))
}

// ── Misc helpers ──────────────────────────────────────────────────────────────

fn tap_name_for(ws_id: WorkspaceId) -> String {
    format!("tap{}", &ws_id.to_string()[..8])
}

fn make_op(id: OperationId, op_type: OperationType, target_id: String) -> Operation {
    let now = Utc::now();
    Operation {
        id, op_type, target_id, status: OperationStatus::Pending,
        progress_pct: Some(0), error: None, created_at: now, updated_at: now,
    }
}
