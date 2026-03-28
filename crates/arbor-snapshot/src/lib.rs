#![allow(dead_code, unused_variables, unused_imports)]
/// Snapshot service — manages checkpoint artifact lifecycle.
///
/// Responsibilities:
///   - Upload state/mem files from runner-local paths to object store
///   - Compute and verify sha256 digests
///   - Seal checkpoint manifests
///   - GC: delete unreferenced artifacts after TTL
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use object_store::{path::Path as ObjPath, ObjectStore};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tracing::{info, instrument};

use arbor_common::{CheckpointArtifacts, CheckpointId};

pub struct SnapshotService {
    store: Arc<dyn ObjectStore>,
    bucket_prefix: String,
}

impl SnapshotService {
    pub fn new(store: Arc<dyn ObjectStore>, bucket_prefix: String) -> Self {
        Self { store, bucket_prefix }
    }

    /// Upload state and mem files, compute digests, return sealed artifacts.
    #[instrument(skip(self), fields(%checkpoint_id))]
    pub async fn upload_and_seal(
        &self,
        checkpoint_id: CheckpointId,
        state_path: &str,
        mem_path: &str,
    ) -> Result<CheckpointArtifacts> {
        let base = format!("{}/checkpoints/{}", self.bucket_prefix, checkpoint_id);

        info!(%checkpoint_id, "uploading state file");
        let (state_uri, state_digest) = self
            .upload_file(state_path, &format!("{}/state.snap", base))
            .await
            .context("upload state.snap")?;

        info!(%checkpoint_id, "uploading mem file");
        let (mem_uri, mem_digest) = self
            .upload_file(mem_path, &format!("{}/mem.snap", base))
            .await
            .context("upload mem.snap")?;

        // Write block manifest (MVP: just records the disk path reference)
        let manifest = serde_json::json!({
            "checkpoint_id": checkpoint_id.to_string(),
            "block_layout_version": 1,
            "drives": [{ "drive_id": "rootfs", "path": "overlay.raw" }]
        });
        let manifest_key = format!("{}/block.json", base);
        self.store
            .put(
                &ObjPath::from(manifest_key.clone()),
                Bytes::from(serde_json::to_vec(&manifest)?).into(),
            )
            .await
            .context("upload block.json")?;

        info!(%checkpoint_id, %state_uri, %mem_uri, "checkpoint sealed");

        Ok(CheckpointArtifacts {
            state_uri: Some(state_uri),
            mem_uri:   Some(mem_uri),
            block_manifest_uri: Some(format!("objstore://{}", manifest_key)),
            state_digest: Some(state_digest),
            mem_digest:   Some(mem_digest),
        })
    }

    /// Download mem file to local path for restore (must survive full VM lifetime).
    #[instrument(skip(self), fields(%checkpoint_id))]
    pub async fn download_mem(
        &self,
        checkpoint_id: CheckpointId,
        dest_path: &str,
        expected_digest: Option<&str>,
    ) -> Result<()> {
        let key = format!(
            "{}/checkpoints/{}/mem.snap",
            self.bucket_prefix, checkpoint_id
        );
        let result = self.store.get(&ObjPath::from(key.clone())).await
            .with_context(|| format!("get {key}"))?;
        let data = result.bytes().await?;

        if let Some(expected) = expected_digest {
            let actual = hex::encode(Sha256::digest(&data));
            anyhow::ensure!(
                actual == expected,
                "mem digest mismatch: expected {expected}, got {actual}"
            );
        }

        tokio::fs::write(dest_path, &data).await
            .with_context(|| format!("write {dest_path}"))?;
        info!(%checkpoint_id, dest = %dest_path, bytes = data.len(), "mem file downloaded");
        Ok(())
    }

    /// Download state file to local path for restore.
    #[instrument(skip(self))]
    pub async fn download_state(
        &self,
        checkpoint_id: CheckpointId,
        dest_path: &str,
        expected_digest: Option<&str>,
    ) -> Result<()> {
        let key = format!(
            "{}/checkpoints/{}/state.snap",
            self.bucket_prefix, checkpoint_id
        );
        let result = self.store.get(&ObjPath::from(key.clone())).await
            .with_context(|| format!("get {key}"))?;
        let data = result.bytes().await?;

        if let Some(expected) = expected_digest {
            let actual = hex::encode(Sha256::digest(&data));
            anyhow::ensure!(
                actual == expected,
                "state digest mismatch: expected {expected}, got {actual}"
            );
        }

        tokio::fs::write(dest_path, &data).await?;
        info!(%checkpoint_id, dest = %dest_path, "state file downloaded");
        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn upload_file(&self, local_path: &str, obj_key: &str) -> Result<(String, String)> {
        let data = tokio::fs::read(local_path).await
            .with_context(|| format!("read {local_path}"))?;
        let digest = hex::encode(Sha256::digest(&data));

        self.store
            .put(&ObjPath::from(obj_key), Bytes::from(data).into())
            .await
            .with_context(|| format!("put {obj_key}"))?;

        Ok((format!("objstore://{}", obj_key), digest))
    }
}
