//! Compatibility-aware runner placement.
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::instrument;

use arbor_common::*;
use crate::db::Db;

pub struct Scheduler {
    db: Arc<Db>,
}

impl Scheduler {
    pub fn new(db: Arc<Db>) -> Self { Self { db } }

    /// Pick a healthy runner with available capacity for a new workspace.
    #[instrument(skip(self, _compat_key))]
    pub async fn pick_runner(
        &self,
        runner_class: &str,
        _compat_key: &CompatibilityKey,
    ) -> Result<RunnerNode> {
        let candidates = self.db.list_healthy_runners(runner_class).await
            .context("list healthy runners")?;

        candidates.into_iter()
            .find(|r| r.available_slots() > 0)
            .ok_or_else(|| anyhow::anyhow!(ArborError::RunnerCapacityExhausted {
                runner_class: runner_class.to_string(),
            }))
    }

    /// Pick a runner that matches the EXACT compatibility key of a checkpoint.
    /// Firecracker snapshots require identical FC version + CPU template to restore.
    #[instrument(skip(self, ckpt))]
    pub async fn pick_compatible_runner(&self, ckpt: &Checkpoint) -> Result<RunnerNode> {
        let ck = &ckpt.compatibility_key.0;
        let runner_class  = ck["runner_class"].as_str().unwrap_or("fc-x86_64-v1");
        let ckpt_fc_ver   = ck["firecracker_version"].as_str().unwrap_or("");
        let ckpt_cpu      = ck["cpu_template"].as_str().unwrap_or("");

        let candidates = self.db.list_healthy_runners(runner_class).await
            .context("list healthy runners for restore")?;

        candidates.into_iter()
            .find(|r| {
                r.firecracker_version == ckpt_fc_ver
                    && r.cpu_template == ckpt_cpu
                    && r.available_slots() > 0
            })
            .ok_or_else(|| anyhow::anyhow!(ArborError::RunnerClassIncompatible {
                checkpoint_class: runner_class.to_string(),
                runner_class: runner_class.to_string(),
            }))
    }

    /// Fetch a specific runner by ID.
    pub async fn get_runner(&self, runner_id: RunnerId) -> Result<RunnerNode> {
        self.db.get_runner(runner_id).await?
            .ok_or_else(|| anyhow::anyhow!(ArborError::RunnerNotFound(runner_id.to_string())))
    }
}
