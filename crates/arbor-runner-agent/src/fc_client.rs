/// Firecracker REST API client.
///
/// Firecracker exposes a REST API on a Unix domain socket.
/// We speak HTTP/1.1 over that socket using hyper with a custom connector.
///
/// Key constraint: `/snapshot/load` must be called BEFORE the VM is fully
/// configured (only logger and metrics may be pre-configured).
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use hyper::{body::Incoming, Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::net::UnixStream;
use tracing::{debug, instrument};

// ── Firecracker REST types ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    pub boot_args: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MachineConfig {
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub guest_mac: String,
    pub host_dev_name: String,
}

#[derive(Debug, Serialize)]
pub struct VsockDevice {
    pub vsock_id: String,
    pub guest_cid: u32,
    pub uds_path: String,
}

#[derive(Debug, Serialize)]
pub struct LoggerConfig {
    pub log_path: String,
    pub level: String,
    pub show_origin: bool,
}

#[derive(Debug, Serialize)]
pub struct SnapshotCreateParams {
    pub snapshot_type: String,  // "Full" or "Diff" (Diff is still developer preview)
    pub snapshot_path: String,
    pub mem_file_path: String,
}

#[derive(Debug, Serialize)]
pub struct SnapshotLoadParams {
    pub snapshot_path: String,
    pub mem_backend: MemBackend,
    pub enable_diff_snapshots: bool,
    pub resume_vm: bool,
}

#[derive(Debug, Serialize)]
pub struct MemBackend {
    pub backend_path: String,
    pub backend_type: String, // "File" | "Uffd"
}

#[derive(Debug, Serialize)]
pub struct VmAction {
    pub action_type: String, // "InstanceStart" | "SendCtrlAltDel" | "FlushMetrics"
}

#[derive(Debug, Serialize)]
pub struct VmState {
    pub state: String, // "Paused" | "Resumed"
}

#[derive(Debug, Deserialize)]
pub struct FcError {
    pub fault_message: String,
}

// ── Client ───────────────────────────────────────────────────────────────────

pub struct FcClient {
    socket_path: String,
}

impl FcClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self { socket_path: socket_path.into() }
    }

    // ── VM boot sequence ─────────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn put_logger(&self, log_path: &str) -> Result<()> {
        self.put("/logger", &LoggerConfig {
            log_path: log_path.to_string(),
            level: "Info".into(),
            show_origin: false,
        }).await
    }

    #[instrument(skip(self))]
    pub async fn put_machine_config(&self, vcpu: u32, mem_mib: u32) -> Result<()> {
        self.put("/machine-config", &MachineConfig {
            vcpu_count: vcpu,
            mem_size_mib: mem_mib,
            cpu_template: Some("T2".into()),  // x86_64 template
            smt: Some(false),
        }).await
    }

    #[instrument(skip(self))]
    pub async fn put_boot_source(&self, kernel_path: &str) -> Result<()> {
        self.put("/boot-source", &BootSource {
            kernel_image_path: kernel_path.to_string(),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
            initrd_path: None,
        }).await
    }

    #[instrument(skip(self))]
    pub async fn put_drive(&self, drive_id: &str, path: &str, root: bool) -> Result<()> {
        self.put(&format!("/drives/{}", drive_id), &Drive {
            drive_id: drive_id.to_string(),
            path_on_host: path.to_string(),
            is_root_device: root,
            is_read_only: false,
            cache_type: Some("Unsafe".into()),
        }).await
    }

    #[instrument(skip(self))]
    pub async fn put_network_interface(&self, iface_id: &str, tap: &str, mac: &str) -> Result<()> {
        self.put(&format!("/network-interfaces/{}", iface_id), &NetworkInterface {
            iface_id: iface_id.to_string(),
            guest_mac: mac.to_string(),
            host_dev_name: tap.to_string(),
        }).await
    }

    #[instrument(skip(self))]
    pub async fn put_vsock(&self, guest_cid: u32, uds_path: &str) -> Result<()> {
        self.put("/vsock", &VsockDevice {
            vsock_id: "vsock0".into(),
            guest_cid,
            uds_path: uds_path.to_string(),
        }).await
    }

    #[instrument(skip(self))]
    pub async fn start_instance(&self) -> Result<()> {
        self.put("/actions", &VmAction { action_type: "InstanceStart".into() }).await
    }

    // ── Snapshot ─────────────────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn pause(&self) -> Result<()> {
        self.patch("/vm", &VmState { state: "Paused".into() }).await
    }

    #[instrument(skip(self))]
    pub async fn resume(&self) -> Result<()> {
        self.patch("/vm", &VmState { state: "Resumed".into() }).await
    }

    /// Create a full VM snapshot.
    /// Firecracker MUST be paused before calling this.
    #[instrument(skip(self))]
    pub async fn create_snapshot(&self, state_path: &str, mem_path: &str) -> Result<()> {
        self.put("/snapshot/create", &SnapshotCreateParams {
            snapshot_type: "Full".into(),
            snapshot_path: state_path.to_string(),
            mem_file_path: mem_path.to_string(),
        }).await
    }

    /// Load a snapshot. Must be called BEFORE the VM is fully configured —
    /// only logger and metrics may be set up first.
    #[instrument(skip(self))]
    pub async fn load_snapshot(&self, state_path: &str, mem_path: &str) -> Result<()> {
        self.put("/snapshot/load", &SnapshotLoadParams {
            snapshot_path: state_path.to_string(),
            mem_backend: MemBackend {
                backend_path: mem_path.to_string(),
                backend_type: "File".into(),
            },
            enable_diff_snapshots: false, // diff is still developer preview
            resume_vm: false,             // we resume explicitly after reseal hooks
        }).await
    }

    // ── HTTP helpers ─────────────────────────────────────────────────────────

    async fn put(&self, path: &str, body: &impl Serialize) -> Result<()> {
        let body_bytes = serde_json::to_vec(body)?;
        let resp = self.request(Method::PUT, path, Some(body_bytes)).await?;
        Self::check_status(resp, path).await
    }

    async fn patch(&self, path: &str, body: &impl Serialize) -> Result<()> {
        let body_bytes = serde_json::to_vec(body)?;
        let resp = self.request(Method::PATCH, path, Some(body_bytes)).await?;
        Self::check_status(resp, path).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to FC socket {}", self.socket_path))?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        tokio::spawn(conn);

        let uri = Uri::builder()
            .scheme("http")
            .authority("localhost")
            .path_and_query(path)
            .build()?;

        let content_type = "application/json";
        let body_bytes = body.unwrap_or_default();
        let len = body_bytes.len();

        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("Content-Type", content_type)
            .header("Content-Length", len)
            .header("Accept", "application/json")
            .body(http_body_util::Full::new(Bytes::from(body_bytes)))?;

        debug!("FC API request: {} {}", req.method(), path);
        Ok(sender.send_request(req).await?)
    }

    async fn check_status(resp: Response<Incoming>, path: &str) -> Result<()> {
        let status = resp.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        // Try to parse error body
        use http_body_util::BodyExt;
        let body = resp.collect().await?.to_bytes();
        let msg = if let Ok(e) = serde_json::from_slice::<FcError>(&body) {
            e.fault_message
        } else {
            String::from_utf8_lossy(&body).to_string()
        };
        bail!("Firecracker API {path} returned {status}: {msg}")
    }
}
