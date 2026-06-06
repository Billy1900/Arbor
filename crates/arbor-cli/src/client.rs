/// Thin HTTP client over the Arbor REST API.
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;

use arbor_common::{
    CheckpointId, OperationId, RolloutId, RolloutSummary, TrajDiff, TraceEventRow,
    TrajectoryId, TrajectoryRow, WorkspaceId, WorkspaceState,
};

pub struct ArborClient {
    base_url: String,
    http:     reqwest::Client,
}

impl ArborClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.http.get(&url).send().await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("GET {url} → {status}: {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("decode response from GET {url}"))
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.http.post(&url).json(body).send().await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("POST {url} → {status}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("decode response from POST {url}"))
    }

    // ── Workspace ─────────────────────────────────────────────────────────────

    pub async fn get_workspace_state(&self, ws_id: WorkspaceId) -> Result<WorkspaceState> {
        #[derive(serde::Deserialize)]
        struct Ws { state: WorkspaceState }
        let ws: Ws = self.get(&format!("/v1/workspaces/{}", ws_id)).await?;
        Ok(ws.state)
    }

    /// Poll until the workspace is READY or ERROR, up to `timeout_secs`.
    pub async fn wait_ready(&self, ws_id: WorkspaceId, timeout_secs: u64) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let state = self.get_workspace_state(ws_id).await?;
            match state {
                WorkspaceState::Ready => return Ok(()),
                WorkspaceState::Error | WorkspaceState::Terminated => {
                    bail!("workspace {ws_id} entered terminal state {state:?}")
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                bail!("timeout waiting for workspace {ws_id} to become READY");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// POST /v1/workspaces/:id/exec and return the session_id.
    pub async fn exec(&self, ws_id: WorkspaceId, command: &str) -> Result<arbor_common::SessionId> {
        #[derive(serde::Serialize)]
        struct Req { command: Vec<String>, pty: bool }
        #[derive(serde::Deserialize)]
        struct Resp { session_id: arbor_common::SessionId }
        let resp: Resp = self.post(
            &format!("/v1/workspaces/{}/exec", ws_id),
            &Req { command: vec!["sh".into(), "-c".into(), command.to_string()], pty: false },
        ).await?;
        Ok(resp.session_id)
    }

    /// Poll session until it exits; returns exit code.
    pub async fn wait_session(&self, ws_id: WorkspaceId, sess_id: arbor_common::SessionId, timeout_secs: u64) -> Result<i32> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            #[derive(serde::Deserialize)]
            struct Sess { exit_code: Option<i32>, status: String }
            let sess: Sess = self.get(&format!("/v1/workspaces/{}/sessions/{}", ws_id, sess_id)).await?;
            if let Some(code) = sess.exit_code {
                return Ok(code);
            }
            if sess.status == "error" {
                bail!("session {sess_id} errored");
            }
            if std::time::Instant::now() >= deadline {
                bail!("timeout waiting for session {sess_id}");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // ── Rollout ───────────────────────────────────────────────────────────────

    pub async fn create_rollout(
        &self,
        checkpoint_id:         CheckpointId,
        reward_command:        &str,
        fork_count:            u32,
        workspace_name_prefix: Option<String>,
    ) -> Result<CreateRolloutResponse> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            source_checkpoint_id:  uuid::Uuid,
            reward_command:        &'a str,
            fork_count:            u32,
            workspace_name_prefix: Option<String>,
        }
        self.post("/v1/rollouts", &Req {
            source_checkpoint_id: checkpoint_id.0,
            reward_command,
            fork_count,
            workspace_name_prefix,
        }).await
    }

    pub async fn get_rollout(&self, rollout_id: RolloutId) -> Result<RolloutSummary> {
        self.get(&format!("/v1/rollouts/{}", rollout_id)).await
    }

    // ── Trajectory ────────────────────────────────────────────────────────────

    pub async fn get_trajectory(&self, tid: TrajectoryId) -> Result<TrajectoryRow> {
        self.get(&format!("/v1/trajectories/{}", tid)).await
    }

    pub async fn get_trajectory_for_workspace(
        &self,
        ws_id: WorkspaceId,
    ) -> Result<Option<TrajectoryRow>> {
        // Uses the workspace trajectory endpoint added in M10
        match self.get::<TrajectoryRow>(&format!("/v1/workspaces/{}/trajectory", ws_id)).await {
            Ok(row) => Ok(Some(row)),
            Err(_)  => Ok(None),
        }
    }

    pub async fn list_events(&self, tid: TrajectoryId, limit: u32, after: i64) -> Result<Vec<TraceEventRow>> {
        self.get(&format!("/v1/trajectories/{}/events?limit={}&after={}", tid, limit, after)).await
    }

    pub async fn diff(&self, rollout_id: RolloutId, a: TrajectoryId, b: TrajectoryId) -> Result<TrajDiff> {
        self.get(&format!("/v1/rollouts/{}/diff?a={}&b={}", rollout_id, a, b)).await
    }

    /// Download all events for a trajectory as NDJSON lines and write to file.
    pub async fn export_jsonl(&self, tid: TrajectoryId, out_path: &std::path::Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let url = format!("{}/v1/trajectories/{}/export", self.base_url, tid);
        let mut resp = self.http.get(&url).send().await
            .with_context(|| format!("export GET {url}"))?;
        if !resp.status().is_success() {
            bail!("export failed: {}", resp.status());
        }
        let mut file = tokio::fs::File::create(out_path).await?;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
pub struct CreateRolloutResponse {
    pub rollout_id:    RolloutId,
    pub workspace_ids: Vec<WorkspaceId>,
    pub operation_ids: Vec<OperationId>,
}
