//! Snapshot service wrapper — abstracts local-fs vs S3 object store.
use anyhow::Result;
use std::sync::Arc;
use object_store::{local::LocalFileSystem, ObjectStore};
use arbor_common::*;

pub struct SnapshotService {
    inner: arbor_snapshot::SnapshotService,
}

impl SnapshotService {
    /// Local filesystem backend (development / single-node MVP).
    pub fn new_local(base_dir: &str) -> Result<Self> {
        std::fs::create_dir_all(base_dir)?;
        let store: Arc<dyn ObjectStore> = Arc::new(LocalFileSystem::new_with_prefix(base_dir)?);
        Ok(Self { inner: arbor_snapshot::SnapshotService::new(store, "checkpoints".into()) })
    }

    /// S3-compatible backend. Requires AWS_* env vars or explicit config.
    /// Enable with the `s3` feature flag in production.
    #[cfg(feature = "s3")]
    pub fn new_s3(bucket: &str, endpoint: Option<&str>, prefix: &str) -> Result<Self> {
        let mut builder = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(bucket);
        if let Some(ep) = endpoint {
            builder = builder.with_endpoint(ep).with_allow_http(true);
        }
        let store: Arc<dyn ObjectStore> = Arc::new(builder.build()?);
        Ok(Self { inner: arbor_snapshot::SnapshotService::new(store, prefix.to_string()) })
    }

    pub async fn upload_and_seal(
        &self, ckpt_id: CheckpointId, state_path: &str, mem_path: &str,
    ) -> Result<CheckpointArtifacts> {
        self.inner.upload_and_seal(ckpt_id, state_path, mem_path).await
    }

    pub async fn download_state(
        &self, ckpt_id: CheckpointId, dest: &str, expected_digest: Option<&str>,
    ) -> Result<()> {
        self.inner.download_state(ckpt_id, dest, expected_digest).await
    }

    pub async fn download_mem(
        &self, ckpt_id: CheckpointId, dest: &str, expected_digest: Option<&str>,
    ) -> Result<()> {
        self.inner.download_mem(ckpt_id, dest, expected_digest).await
    }
}
