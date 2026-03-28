use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ── ID newtypes ──────────────────────────────────────────────────────────────

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
        }
        impl Default for $name {
            fn default() -> Self { Self::new() }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(WorkspaceId);
id_newtype!(CheckpointId);
id_newtype!(OperationId);
id_newtype!(SessionId);
id_newtype!(GrantId);
id_newtype!(RunnerId);

// ── Workspace ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "workspace_state", rename_all = "snake_case")]
pub enum WorkspaceState {
    Creating,
    Ready,
    Running,
    Checkpointing,
    Restoring,
    Quarantined,
    Terminating,
    Terminated,
    Error,
}

impl std::fmt::Display for WorkspaceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_string(self).unwrap_or_default();
        write!(f, "{}", s.trim_matches('"'))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub state: WorkspaceState,
    pub repo: RepoConfig,
    pub runtime: RuntimeConfig,
    pub compatibility_key: CompatibilityKey,
    pub current_checkpoint_id: Option<CheckpointId>,
    pub runner_id: Option<RunnerId>,
    pub identity_epoch: u64,
    pub network_epoch: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub provider: String,
    pub url: String,
    pub r#ref: String,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub runner_class: String,
    pub vcpu_count: u32,
    pub memory_mib: u32,
    pub disk_gb: u32,
}

// ── Compatibility key ────────────────────────────────────────────────────────
// Must match exactly for snapshot restore — see Firecracker docs.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct CompatibilityKey(pub serde_json::Value);

impl CompatibilityKey {
    pub fn new(
        runner_class: &str,
        arch: &str,
        cpu_template: &str,
        firecracker_version: &str,
        guest_kernel_hash: &str,
        base_image_id: &str,
        block_layout_version: u32,
    ) -> Self {
        Self(serde_json::json!({
            "runner_class": runner_class,
            "arch": arch,
            "cpu_template": cpu_template,
            "firecracker_version": firecracker_version,
            "guest_kernel_hash": guest_kernel_hash,
            "base_image_id": base_image_id,
            "block_layout_version": block_layout_version,
        }))
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

// ── Checkpoint ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "checkpoint_state", rename_all = "snake_case")]
pub enum CheckpointState {
    Pending,
    Uploading,
    Sealed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub workspace_id: WorkspaceId,
    pub parent_id: Option<CheckpointId>,
    pub name: Option<String>,
    pub state: CheckpointState,
    pub compatibility_key: CompatibilityKey,
    pub artifacts: CheckpointArtifacts,
    pub resume_hooks_version: u32,
    pub identity_epoch: u64,
    pub network_epoch: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointArtifacts {
    pub state_uri: Option<String>,
    pub mem_uri: Option<String>,
    pub block_manifest_uri: Option<String>,
    pub state_digest: Option<String>,
    pub mem_digest: Option<String>,
}

impl CheckpointArtifacts {
    pub fn empty() -> Self {
        Self { state_uri: None, mem_uri: None, block_manifest_uri: None,
               state_digest: None, mem_digest: None }
    }

    pub fn is_complete(&self) -> bool {
        self.state_uri.is_some() && self.mem_uri.is_some() && self.block_manifest_uri.is_some()
    }
}

// ── Operation ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "operation_type", rename_all = "snake_case")]
pub enum OperationType {
    CreateWorkspace,
    TerminateWorkspace,
    CreateCheckpoint,
    RestoreCheckpoint,
    ForkCheckpoint,
    ExecSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "operation_status", rename_all = "snake_case")]
pub enum OperationStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId,
    pub op_type: OperationType,
    pub target_id: String,
    pub status: OperationStatus,
    pub progress_pct: Option<u8>,
    pub error: Option<OperationError>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<serde_json::Value>,
}

// ── Session ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Exited,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSession {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub command: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub pty: bool,
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    pub reconnectable: bool,
    pub started_at: DateTime<Utc>,
}

// ── Runner node ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerNode {
    pub id: RunnerId,
    pub runner_class: String,
    pub address: String, // http://host:port — runner-agent API
    pub arch: String,
    pub firecracker_version: String,
    pub cpu_template: String,
    pub capacity_slots: u32,
    pub used_slots: u32,
    pub healthy: bool,
    pub last_heartbeat: DateTime<Utc>,
}

impl RunnerNode {
    pub fn available_slots(&self) -> u32 {
        self.capacity_slots.saturating_sub(self.used_slots)
    }
}

// ── Secret grant ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretMode {
    BrokeredProxy,
    EphemeralEnv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGrant {
    pub id: GrantId,
    pub workspace_id: WorkspaceId,
    pub provider: String,
    pub mode: SecretMode,
    pub vault_ref: String,
    pub allowed_hosts: Vec<String>,
    pub ttl_seconds: u64,
    pub active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// ── API request / response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub repo: RepoConfig,
    pub runtime: RuntimeConfig,
    pub image: ImageConfig,
    pub network: NetworkConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ImageConfig {
    pub base_image_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NetworkConfig {
    #[serde(default = "default_egress_policy")]
    pub egress_policy: String,
}

fn default_egress_policy() -> String { "default-deny".into() }

#[derive(Debug, Serialize)]
pub struct CreateWorkspaceResponse {
    pub workspace_id: WorkspaceId,
    pub operation_id: OperationId,
    pub state: WorkspaceState,
}

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub pty: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cwd() -> String { "/workspace".into() }
fn default_cols() -> u16 { 220 }
fn default_rows() -> u16 { 50 }

#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub session_id: SessionId,
    pub status: SessionStatus,
    pub attachable: bool,
}

#[derive(Debug, Serialize)]
pub struct AttachResponse {
    pub transport: String,
    pub url: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CheckpointRequest {
    pub name: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub notes: Option<String>,
}

fn default_mode() -> String { "full_vm".into() }

#[derive(Debug, Serialize)]
pub struct CheckpointResponse {
    pub checkpoint_id: CheckpointId,
    pub operation_id: OperationId,
    pub state: CheckpointState,
}

#[derive(Debug, Deserialize)]
pub struct RestoreRequest {
    #[serde(default = "default_target")]
    pub target: String,
    pub workspace_name: Option<String>,
    pub post_restore: PostRestoreConfig,
}

fn default_target() -> String { "new_workspace".into() }

#[derive(Debug, Deserialize)]
pub struct PostRestoreConfig {
    #[serde(default = "yes")]
    pub quarantine: bool,
    #[serde(default = "yes")]
    pub identity_reseal: bool,
}

fn yes() -> bool { true }

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self { error: ApiErrorDetail { code: code.into(), message: message.into(), retryable, details: None } }
    }
}

// ── Display impls (needed for DB string encoding) ─────────────────────────────

impl std::fmt::Display for OperationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::CreateWorkspace    => "create_workspace",
            Self::TerminateWorkspace => "terminate_workspace",
            Self::CreateCheckpoint   => "create_checkpoint",
            Self::RestoreCheckpoint  => "restore_checkpoint",
            Self::ForkCheckpoint     => "fork_checkpoint",
            Self::ExecSession        => "exec_session",
        };
        write!(f, "{s}")
    }
}

impl std::fmt::Display for OperationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Pending   => "pending",
            Self::Running   => "running",
            Self::Succeeded => "succeeded",
            Self::Failed    => "failed",
            Self::Canceled  => "canceled",
        };
        write!(f, "{s}")
    }
}

impl std::fmt::Display for CheckpointState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Pending   => "pending",
            Self::Uploading => "uploading",
            Self::Sealed    => "sealed",
            Self::Failed    => "failed",
        };
        write!(f, "{s}")
    }
}
