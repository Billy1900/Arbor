use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use std::sync::Arc;
use tracing::{error, info, instrument, warn};

use arbor_common::*;
use arbor_common::proto::{CreateVmRequest, VmExecRequest, VmCheckpointRequest};

use crate::db::Db;
use crate::runner_client::RunnerClient;
use crate::scheduler::Scheduler;

pub struct Controller {
    pub db: Arc<Db>,
    pub scheduler: Arc<Scheduler>,
    pub config: Arc<ControllerConfig>,
}

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub base_images_dir: String,
    pub kernel_path: String,
    pub default_runner_class: String,
    pub attach_token_secret: String,
    pub api_base_url: String,
}

impl Controller {
    pub fn new(db: Arc<Db>, scheduler: Arc<Scheduler>, config: Arc<ControllerConfig>) -> Self {
        Self { db, scheduler, config }
    }

    // ── Create workspace ─────────────────────────────────────────────────────

    #[instrument(skip(self, req), fields(workspace_name = %req.name))]
    pub async fn create_workspace(self: Arc<Self>, req: CreateWorkspaceRequest) -> Result<(Workspace, Operation)> {
        let now = Utc::now();
        let ws_id = WorkspaceId::new();
        let op_id = OperationId::new();

        let compat_key = CompatibilityKey::new(
            &req.runtime.runner_class,
            "x86_64",
            "T2",  // x86_64 template — not T2A (that's ARM)
            "1.9.0",
            "sha256:placeholder",  // replaced when image is baked
            &req.image.base_image_id,
            1,
        );

        let workspace = Workspace {
            id: ws_id,
            name: req.name.clone(),
            state: WorkspaceState::Creating,
            repo: req.repo.clone(),
            runtime: req.runtime.clone(),
            compatibility_key: compat_key,
            current_checkpoint_id: None,
            runner_id: None,
            identity_epoch: 0,
            network_epoch: 0,
            created_at: now,
            updated_at: now,
        };

        let op = Operation {
            id: op_id,
            op_type: OperationType::CreateWorkspace,
            target_id: ws_id.to_string(),
            status: OperationStatus::Pending,
            progress_pct: Some(0),
            error: None,
            created_at: now,
            updated_at: now,
        };

        self.db.insert_workspace(&workspace).await?;
        self.db.insert_operation(&op).await?;

        // Drive async — spawn background task
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

        // 1. Pick a compatible runner
        let runner = self
            .scheduler
            .pick_runner(&ws.runtime.runner_class, &ws.compatibility_key)
            .await?;

        info!(%ws_id, runner_id = %runner.id, runner_addr = %runner.address, "selected runner");
        self.db.increment_runner_slots(runner.id).await?;

        // 2. Build paths on runner host
        let rootfs_path = format!(
            "/var/lib/arbor/workspaces/{}/rootfs.overlay.raw",
            ws_id
        );
        let vsock_uds = format!("/var/lib/arbor/workspaces/{}/vm.vsock", ws_id);
        let tap_name  = tap_name_for(ws_id);

        // 3. Ask runner to create VM
        let client = RunnerClient::new(&runner.address);
        let vm_resp = client.create_vm(CreateVmRequest {
            workspace_id: ws_id.to_string(),
            vcpu_count: ws.runtime.vcpu_count,
            memory_mib: ws.runtime.memory_mib,
            disk_gb: ws.runtime.disk_gb,
            kernel_path: self.config.kernel_path.clone(),
            rootfs_path,
            tap_device: tap_name.clone(),
            vsock_uds_path: vsock_uds,
            base_image_id: "ubuntu-24.04-dev-v1".into(),
            repo_url: ws.repo.url.clone(),
            repo_ref: ws.repo.r#ref.clone(),
        })
        .await
        .context("runner create_vm failed")?;

        info!(%ws_id, vm_id = %vm_resp.vm_id, "VM created and booted");

        // 4. Update DB
        self.db.update_workspace_runner(ws_id, runner.id, &vm_resp.vm_id).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Ready).await?;
        self.db.complete_operation(op_id, true, None).await?;

        info!(%ws_id, "workspace ready");
        Ok(())
    }

    // ── Exec session ─────────────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn exec_session(
        &self,
        ws_id: WorkspaceId,
        req: ExecRequest,
    ) -> Result<ExecSession> {
        let ws = self.require_workspace(ws_id).await?;
        self.assert_state(&ws, &[WorkspaceState::Ready, WorkspaceState::Running])?;

        let session_id = SessionId::new();
        let now = Utc::now();

        let session = ExecSession {
            id: session_id,
            workspace_id: ws_id,
            command: req.command.clone(),
            cwd: req.cwd.clone(),
            env: req.env.clone(),
            pty: req.pty,
            status: SessionStatus::Starting,
            exit_code: None,
            reconnectable: true,
            started_at: now,
        };

        self.db.insert_session(&session).await?;

        // Ask runner to start the process in the VM
        let runner = self.get_runner_for(&ws).await?;
        let client = RunnerClient::new(&runner.address);
        let vm_id = ws.runtime.runner_class.clone(); // stored in ws.vm_id via DB, but we use runner_class as placeholder key
        // In real code, fetch vm_id from workspace row
        client.vm_exec(VmExecRequest {
            session_id,
            command: req.command,
            cwd: req.cwd,
            env: req.env,
            pty: req.pty,
            cols: req.cols,
            rows: req.rows,
        }).await.context("runner vm_exec failed")?;

        // Transition to Running if not already
        if ws.state == WorkspaceState::Ready {
            self.db.update_workspace_state(ws_id, WorkspaceState::Running).await?;
        }

        Ok(session)
    }

    // ── Terminate workspace ──────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn terminate_workspace(self: Arc<Self>, ws_id: WorkspaceId) -> Result<Operation> {
        let ws = self.require_workspace(ws_id).await?;
        // Termination is always allowed (highest priority)
        let op_id = OperationId::new();
        let now = Utc::now();
        let op = Operation {
            id: op_id,
            op_type: OperationType::TerminateWorkspace,
            target_id: ws_id.to_string(),
            status: OperationStatus::Pending,
            progress_pct: Some(0),
            error: None,
            created_at: now,
            updated_at: now,
        };
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
        if let Some(runner_id) = ws.runner_id {
            let runner = self.scheduler.get_runner(runner_id).await?;
            let client = RunnerClient::new(&runner.address);
            // best-effort VM kill
            let _ = client.destroy_vm(&ws_id.to_string()).await;
            self.db.decrement_runner_slots(runner_id).await?;
        }
        self.db.update_workspace_state(ws_id, WorkspaceState::Terminated).await?;
        self.db.complete_operation(op_id, true, None).await?;
        info!(%ws_id, "workspace terminated");
        Ok(())
    }

    // ── Create checkpoint ────────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn create_checkpoint(
        self: Arc<Self>,
        ws_id: WorkspaceId,
        req: CheckpointRequest,
    ) -> Result<(Checkpoint, Operation)> {
        let ws = self.require_workspace(ws_id).await?;
        self.assert_state(&ws, &[WorkspaceState::Ready, WorkspaceState::Running])?;

        let ckpt_id = CheckpointId::new();
        let op_id   = OperationId::new();
        let now     = Utc::now();

        let ckpt = Checkpoint {
            id: ckpt_id,
            workspace_id: ws_id,
            parent_id: ws.current_checkpoint_id,
            name: req.name.clone(),
            state: CheckpointState::Pending,
            compatibility_key: ws.compatibility_key.clone(),
            artifacts: CheckpointArtifacts::empty(),
            resume_hooks_version: 1,
            identity_epoch: ws.identity_epoch,
            network_epoch: ws.network_epoch,
            created_at: now,
        };

        let op = Operation {
            id: op_id,
            op_type: OperationType::CreateCheckpoint,
            target_id: ckpt_id.to_string(),
            status: OperationStatus::Pending,
            progress_pct: Some(0),
            error: None,
            created_at: now,
            updated_at: now,
        };

        self.db.insert_checkpoint(&ckpt).await?;
        self.db.insert_operation(&op).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Checkpointing).await?;

        let ctrl = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ctrl.drive_checkpoint(ws_id, ckpt_id, op_id, ws).await {
                error!(?e, %ws_id, %ckpt_id, "checkpoint failed");
                let _ = ctrl.db.update_workspace_state(ws_id, WorkspaceState::Error).await;
                let _ = ctrl.db.complete_operation(op_id, false, Some(&e.to_string())).await;
            }
        });

        Ok((ckpt, op))
    }

    async fn drive_checkpoint(
        &self,
        ws_id: WorkspaceId,
        ckpt_id: CheckpointId,
        op_id: OperationId,
        ws: Workspace,
    ) -> Result<()> {
        let runner_id = ws.runner_id.ok_or_else(|| anyhow!("no runner assigned"))?;
        let runner = self.scheduler.get_runner(runner_id).await?;
        let client = RunnerClient::new(&runner.address);

        let snap_dir = format!("/var/lib/arbor/snapshots/cache/{}", ckpt_id);

        info!(%ws_id, %ckpt_id, "requesting checkpoint from runner");
        let snap_resp = client.checkpoint_vm(&ws_id.to_string(), VmCheckpointRequest {
            checkpoint_id: ckpt_id.to_string(),
            snapshot_dir: snap_dir,
        }).await?;

        let artifacts = CheckpointArtifacts {
            state_uri: Some(snap_resp.state_path),
            mem_uri: Some(snap_resp.mem_path),
            block_manifest_uri: Some(format!("local://{}/block.json", ckpt_id)),
            state_digest: None,
            mem_digest: None,
        };

        self.db.seal_checkpoint(ckpt_id, &artifacts).await?;
        self.db.update_workspace_state(ws_id, WorkspaceState::Ready).await?;
        self.db.complete_operation(op_id, true, None).await?;

        info!(%ws_id, %ckpt_id, "checkpoint sealed");
        Ok(())
    }

    // ── Build attach URL ─────────────────────────────────────────────────────

    pub async fn build_attach_url(&self, ws_id: WorkspaceId, sess_id: SessionId) -> Result<String> {
        // Generate a short-lived signed token for the WebSocket attach endpoint
        let token = sign_attach_token(
            ws_id,
            sess_id,
            &self.config.attach_token_secret,
        );
        Ok(format!("{}/v1/attach/{}?token={}", self.config.api_base_url, sess_id, token))
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn require_workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        self.db.get_workspace(id)
            .await?
            .ok_or_else(|| anyhow!(ArborError::WorkspaceNotFound(id.to_string())))
    }

    fn assert_state(&self, ws: &Workspace, allowed: &[WorkspaceState]) -> Result<()> {
        if !allowed.contains(&ws.state) {
            bail!(ArborError::WorkspaceBusy { state: ws.state.to_string() });
        }
        Ok(())
    }

    async fn get_runner_for(&self, ws: &Workspace) -> Result<RunnerNode> {
        let runner_id = ws.runner_id.ok_or_else(|| anyhow!("workspace has no assigned runner"))?;
        self.scheduler.get_runner(runner_id).await
    }

    fn clone_arc(&self, arc: &Arc<Self>) -> Arc<Self> { Arc::clone(arc) }
}

// ── Simple HMAC-based attach token ───────────────────────────────────────────

fn sign_attach_token(ws_id: WorkspaceId, sess_id: SessionId, secret: &str) -> String {
    use sha2::{Sha256, Digest};
    let expires = chrono::Utc::now().timestamp() + 900; // 15 min
    let payload = format!("{}.{}.{}", ws_id, sess_id, expires);
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(payload.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{}.{}", payload, &digest[..16])
}

pub fn verify_attach_token(token: &str, secret: &str) -> Option<(WorkspaceId, SessionId)> {
    use sha2::{Sha256, Digest};
    let parts: Vec<&str> = token.rsplitn(2, '.').collect();
    if parts.len() < 2 { return None; }
    let sig      = parts[0];
    let payload  = parts[1];
    let segments: Vec<&str> = payload.split('.').collect();
    if segments.len() < 3 { return None; }

    let expires: i64 = segments[2].parse().ok()?;
    if chrono::Utc::now().timestamp() > expires { return None; }

    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(payload.as_bytes());
    let expected = hex::encode(hasher.finalize());
    if &expected[..16] != sig { return None; }

    let ws_id   = uuid::Uuid::parse_str(segments[0]).ok().map(WorkspaceId)?;
    let sess_id = uuid::Uuid::parse_str(segments[1]).ok().map(SessionId)?;
    Some((ws_id, sess_id))
}

fn tap_name_for(ws_id: WorkspaceId) -> String {
    // TAP device names are limited to 15 chars on Linux
    format!("tap{}", &ws_id.to_string()[..8])
}
