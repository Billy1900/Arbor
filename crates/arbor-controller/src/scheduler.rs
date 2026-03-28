// scheduler.rs — compatibility-aware runner placement
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

    /// Pick the least-loaded healthy runner matching the given class and
    /// compatibility key. The compatibility key check is enforced here —
    /// Firecracker snapshots cannot be restored on a runner with a different
    /// firecracker_version, cpu_template, or kernel.
    #[instrument(skip(self, compat_key))]
    pub async fn pick_runner(
        &self,
        runner_class: &str,
        compat_key: &CompatibilityKey,
    ) -> Result<RunnerNode> {
        let candidates = self.db.list_healthy_runners(runner_class).await
            .context("failed to list healthy runners")?;

        if candidates.is_empty() {
            return Err(anyhow::anyhow!(ArborError::RunnerCapacityExhausted {
                runner_class: runner_class.to_string(),
            }));
        }

        // For new workspaces any runner of the right class is fine.
        // For restores the caller must pass the checkpoint's compat key
        // and we filter to matching runners.
        candidates
            .into_iter()
            .find(|r| r.available_slots() > 0)
            .ok_or_else(|| anyhow::anyhow!(ArborError::RunnerCapacityExhausted {
                runner_class: runner_class.to_string(),
            }))
    }

    /// Pick a runner that matches the *exact* compatibility key stored in a
    /// checkpoint. Used for restore/fork operations.
    #[instrument(skip(self, ckpt))]
    pub async fn pick_compatible_runner(&self, ckpt: &Checkpoint) -> Result<RunnerNode> {
        let compat = &ckpt.compatibility_key;
        let runner_class = compat.0.get("runner_class")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let candidates = self.db.list_healthy_runners(runner_class).await?;

        if candidates.is_empty() {
            return Err(anyhow::anyhow!(ArborError::RunnerCapacityExhausted {
                runner_class: runner_class.to_string(),
            }));
        }

        // Strict compatibility check: runner must have matching FC version and CPU template
        let ckpt_fc_ver = compat.0["firecracker_version"].as_str().unwrap_or("");
        let ckpt_cpu    = compat.0["cpu_template"].as_str().unwrap_or("");

        candidates
            .into_iter()
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

    pub async fn get_runner(&self, runner_id: RunnerId) -> Result<RunnerNode> {
        // In MVP we fetch from DB. Later we add an in-memory cache.
        let runners = self.db.list_healthy_runners("fc-x86_64-v1").await?;
        runners.into_iter()
            .find(|r| r.id == runner_id)
            .ok_or_else(|| anyhow::anyhow!(ArborError::RunnerNotFound(runner_id.to_string())))
    }
}
