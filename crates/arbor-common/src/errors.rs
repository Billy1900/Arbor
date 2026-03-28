use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArborError {
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("runner not found: {0}")]
    RunnerNotFound(String),

    #[error("workspace is busy (current state: {state})")]
    WorkspaceBusy { state: String },

    #[error("runner capacity exhausted for class {runner_class}")]
    RunnerCapacityExhausted { runner_class: String },

    #[error("runner class incompatible: checkpoint needs {checkpoint_class}, runner is {runner_class}")]
    RunnerClassIncompatible { checkpoint_class: String, runner_class: String },

    #[error("checkpoint not sealed: {0}")]
    CheckpointNotSealed(String),

    #[error("checkpoint artifact missing: {0}")]
    CheckpointArtifactMissing(String),

    #[error("reseal failed: {0}")]
    ResealFailed(String),

    #[error("egress denied: {0}")]
    EgressDenied(String),

    #[error("secret policy denied: {0}")]
    SecretPolicyDenied(String),

    #[error("attach session gone: {0}")]
    AttachSessionGone(String),

    #[error("snapshot storage quota exceeded")]
    SnapshotStorageExceeded,

    #[error("unsupported in MVP: {0}")]
    UnsupportedInMvp(String),

    #[error("firecracker API error: {0}")]
    FirecrackerApi(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

impl ArborError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::WorkspaceNotFound(_) => "WORKSPACE_NOT_FOUND",
            Self::CheckpointNotFound(_) => "CHECKPOINT_NOT_FOUND",
            Self::SessionNotFound(_) => "SESSION_NOT_FOUND",
            Self::RunnerNotFound(_) => "RUNNER_NOT_FOUND",
            Self::WorkspaceBusy { .. } => "WORKSPACE_BUSY",
            Self::RunnerCapacityExhausted { .. } => "RUNNER_CAPACITY_EXHAUSTED",
            Self::RunnerClassIncompatible { .. } => "RUNNER_CLASS_INCOMPATIBLE",
            Self::CheckpointNotSealed(_) => "CHECKPOINT_NOT_SEALED",
            Self::CheckpointArtifactMissing(_) => "CHECKPOINT_ARTIFACT_MISSING",
            Self::ResealFailed(_) => "RESUME_RESEAL_FAILED",
            Self::EgressDenied(_) => "EGRESS_DENIED",
            Self::SecretPolicyDenied(_) => "SECRET_POLICY_DENIED",
            Self::AttachSessionGone(_) => "ATTACH_SESSION_GONE",
            Self::SnapshotStorageExceeded => "SNAPSHOT_STORAGE_EXCEEDED",
            Self::UnsupportedInMvp(_) => "UNSUPPORTED_OPERATION_IN_MVP",
            Self::FirecrackerApi(_) => "FIRECRACKER_API_ERROR",
            Self::Database(_) => "DATABASE_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::RunnerCapacityExhausted { .. } | Self::Database(_) | Self::Internal(_))
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::WorkspaceNotFound(_) | Self::CheckpointNotFound(_)
            | Self::SessionNotFound(_) | Self::RunnerNotFound(_) => 404,
            Self::WorkspaceBusy { .. } | Self::RunnerClassIncompatible { .. }
            | Self::CheckpointNotSealed(_) => 409,
            Self::RunnerCapacityExhausted { .. } => 503,
            Self::UnsupportedInMvp(_) => 501,
            Self::SecretPolicyDenied(_) | Self::EgressDenied(_) => 403,
            _ => 500,
        }
    }
}
