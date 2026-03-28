/// VM lifecycle manager.
///
/// Manages the lifecycle of Firecracker microVMs:
///   - Boot new VMs (with Jailer)
///   - Connect to guest-agent over vsock
///   - Create and restore snapshots
///   - Destroy VMs cleanly
///
/// One VmEntry per running VM, held in a DashMap keyed by workspace_id.
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{debug, error, info, instrument, warn};

use arbor_common::proto::{
    CreateVmRequest, CreateVmResponse, GuestMessage, HostMessage,
    VmCheckpointRequest, VmCheckpointResponse, VmExecRequest, VmExecResponse,
    VmRestoreRequest, VmRestoreResponse, encode_frame, decode_frame_length,
    VSOCK_AGENT_PORT,
};
use arbor_common::SessionId;

use crate::fc_client::FcClient;
use crate::netns::{setup_network, teardown_network, NetConfig};
use crate::session_mux::SessionMux;

const ARBOR_WORKSPACES_DIR: &str = "/var/lib/arbor/workspaces";
const ARBOR_FC_BIN: &str         = "/var/lib/arbor/firecracker/bin/firecracker";
const ARBOR_JAILER_BIN: &str     = "/var/lib/arbor/firecracker/bin/jailer";
const BASE_IMAGES_DIR: &str      = "/var/lib/arbor/images";
const JAILER_UID: u32            = 200001;
const JAILER_GID: u32            = 200001;

// ── VM state ─────────────────────────────────────────────────────────────────

pub struct VmEntry {
    pub vm_id: String,
    pub workspace_id: String,
    pub fc_socket_path: String,
    pub vsock_uds_path: String,
    pub net_cfg: NetConfig,
    pub mux: Arc<SessionMux>,
    _child: Mutex<Option<Child>>,
}

// ── Manager ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct VmManager {
    vms: Arc<parking_lot::RwLock<HashMap<String, Arc<VmEntry>>>>,
    config: Arc<VmManagerConfig>,
}

#[derive(Debug, Clone)]
pub struct VmManagerConfig {
    pub fc_binary: String,
    pub jailer_binary: String,
    pub kernel_path: String,
    pub workspaces_dir: String,
    pub base_images_dir: String,
    pub jailer_uid: u32,
    pub jailer_gid: u32,
}

impl Default for VmManagerConfig {
    fn default() -> Self {
        Self {
            fc_binary:       ARBOR_FC_BIN.into(),
            jailer_binary:   ARBOR_JAILER_BIN.into(),
            kernel_path:     "/var/lib/arbor/firecracker/vmlinux".into(),
            workspaces_dir:  ARBOR_WORKSPACES_DIR.into(),
            base_images_dir: BASE_IMAGES_DIR.into(),
            jailer_uid:      JAILER_UID,
            jailer_gid:      JAILER_GID,
        }
    }
}

impl VmManager {
    pub fn new(config: VmManagerConfig) -> Self {
        Self {
            vms: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            config: Arc::new(config),
        }
    }

    // ── Create VM ────────────────────────────────────────────────────────────

    #[instrument(skip(self, req), fields(ws_id = %req.workspace_id))]
    pub async fn create_vm(&self, req: CreateVmRequest) -> Result<CreateVmResponse> {
        let ws_id = &req.workspace_id;
        let net_cfg = NetConfig::from_ws_id(ws_id);

        // 1. Prepare workspace directory
        let ws_dir = PathBuf::from(&self.config.workspaces_dir).join(ws_id);
        tokio::fs::create_dir_all(&ws_dir).await?;

        let fc_socket  = ws_dir.join("fc.socket");
        let vsock_uds  = ws_dir.join("vm.vsock");
        let log_file   = ws_dir.join("fc.log");
        let rootfs     = ws_dir.join("rootfs.overlay.raw");
        let base_image = PathBuf::from(&self.config.base_images_dir)
            .join(&req.base_image_id)
            .join("rootfs.base.raw");

        // 2. Create overlay disk (copy-on-write from base image)
        create_overlay_disk(&base_image, &rootfs, req.disk_gb).await?;

        // 3. Setup netns + TAP (runs synchronous ip commands on blocking thread)
        let net_cfg_clone = net_cfg.clone();
        tokio::task::spawn_blocking(move || setup_network(&net_cfg_clone))
            .await?
            .context("network setup failed")?;

        // 4. Launch Firecracker via Jailer
        let child = self.spawn_jailer(ws_id, &fc_socket, &net_cfg).await?;
        let vm_id = format!("vm-{}", ws_id);

        // 5. Wait for FC socket to appear (FC is still starting up)
        wait_for_socket(&fc_socket, Duration::from_secs(10)).await
            .context("Firecracker socket never appeared")?;

        // 6. Configure VM via FC REST API
        let fc = FcClient::new(fc_socket.to_string_lossy().as_ref());

        fc.put_logger(&log_file.to_string_lossy()).await?;
        fc.put_machine_config(req.vcpu_count, req.memory_mib).await?;
        fc.put_boot_source(&self.config.kernel_path).await?;
        fc.put_drive("rootfs", &rootfs.to_string_lossy(), true).await?;
        fc.put_network_interface("eth0", &net_cfg.tap_name, &net_cfg.guest_mac).await?;
        fc.put_vsock(arbor_common::proto::VSOCK_GUEST_CID, &vsock_uds.to_string_lossy()).await?;

        // 7. Start VM
        fc.start_instance().await?;
        info!(%ws_id, %vm_id, "Firecracker instance started");

        // 8. Wait for guest-agent to come up via vsock
        let mux = Arc::new(SessionMux::new(vsock_uds.to_string_lossy().to_string()));
        wait_for_guest_agent(&mux, Duration::from_secs(30)).await
            .context("guest-agent did not respond")?;
        info!(%ws_id, "guest-agent ready");

        let entry = Arc::new(VmEntry {
            vm_id: vm_id.clone(),
            workspace_id: ws_id.clone(),
            fc_socket_path: fc_socket.to_string_lossy().into(),
            vsock_uds_path: vsock_uds.to_string_lossy().into(),
            net_cfg: net_cfg.clone(),
            mux,
            _child: Mutex::new(Some(child)),
        });

        self.vms.write().insert(ws_id.clone(), entry);

        Ok(CreateVmResponse {
            vm_id,
            vsock_path: vsock_uds.to_string_lossy().into(),
            tap_device: net_cfg.tap_name,
            guest_ip:   net_cfg.guest_ip,
            state:      "running".into(),
        })
    }

    // ── Exec ─────────────────────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn exec(&self, vm_id_or_ws: &str, req: VmExecRequest) -> Result<VmExecResponse> {
        let entry = self.get_vm(vm_id_or_ws)?;
        entry.mux.send(HostMessage::Exec {
            session_id: req.session_id,
            command:    req.command,
            cwd:        req.cwd,
            env:        req.env,
            pty:        req.pty,
            cols:       req.cols,
            rows:       req.rows,
        }).await?;
        Ok(VmExecResponse { session_id: req.session_id, started: true })
    }

    // ── Checkpoint ───────────────────────────────────────────────────────────

    #[instrument(skip(self, req))]
    pub async fn checkpoint(&self, vm_id_or_ws: &str, req: VmCheckpointRequest) -> Result<VmCheckpointResponse> {
        let entry = self.get_vm(vm_id_or_ws)?;

        let snap_dir = PathBuf::from(&req.snapshot_dir);
        tokio::fs::create_dir_all(&snap_dir).await?;

        let state_path = snap_dir.join("state.snap");
        let mem_path   = snap_dir.join("mem.snap");

        // 1. Quiesce guest (sync filesystems, mark paused)
        entry.mux.send(HostMessage::Quiesce).await?;
        let quiesce_ok = entry.mux.wait_quiesce(Duration::from_secs(10)).await;
        if !quiesce_ok {
            warn!("guest did not acknowledge quiesce — continuing anyway");
        }

        // 2. Pause VM via Firecracker API (required before snapshot)
        let fc = FcClient::new(&entry.fc_socket_path);
        fc.pause().await.context("FC pause failed")?;

        // 3. Create full snapshot
        fc.create_snapshot(
            &state_path.to_string_lossy(),
            &mem_path.to_string_lossy(),
        ).await.context("FC create_snapshot failed")?;

        // 4. Resume source VM
        fc.resume().await.context("FC resume failed")?;

        let state_size = tokio::fs::metadata(&state_path).await.map(|m| m.len()).unwrap_or(0);
        let mem_size   = tokio::fs::metadata(&mem_path).await.map(|m| m.len()).unwrap_or(0);

        info!(ws_id = %entry.workspace_id, state_bytes = state_size, mem_bytes = mem_size, "checkpoint created");

        Ok(VmCheckpointResponse {
            state_path: state_path.to_string_lossy().into(),
            mem_path:   mem_path.to_string_lossy().into(),
            state_size_bytes: state_size,
            mem_size_bytes:   mem_size,
        })
    }

    // ── Restore ──────────────────────────────────────────────────────────────

    #[instrument(skip(self, req), fields(ws_id = %req.workspace_id))]
    pub async fn restore(&self, req: VmRestoreRequest) -> Result<VmRestoreResponse> {
        let ws_id = &req.workspace_id;
        let net_cfg = NetConfig::from_ws_id(ws_id);

        let ws_dir    = PathBuf::from(&self.config.workspaces_dir).join(ws_id);
        tokio::fs::create_dir_all(&ws_dir).await?;
        let fc_socket = ws_dir.join("fc.socket");
        let log_file  = ws_dir.join("fc.log");

        // 1. Setup new netns + TAP for this restored workspace
        let net_cfg_clone = net_cfg.clone();
        tokio::task::spawn_blocking(move || setup_network(&net_cfg_clone))
            .await?
            .context("netns setup for restore failed")?;

        // 2. Spawn new Firecracker process (via Jailer)
        let child = self.spawn_jailer(ws_id, &fc_socket, &net_cfg).await?;

        wait_for_socket(&fc_socket, Duration::from_secs(10)).await
            .context("FC socket for restore never appeared")?;

        // 3. Configure ONLY logger + metrics before /snapshot/load
        //    This is a hard Firecracker requirement.
        let fc = FcClient::new(fc_socket.to_string_lossy().as_ref());
        fc.put_logger(&log_file.to_string_lossy()).await?;

        // 4. Load snapshot — must happen before any other configuration
        fc.load_snapshot(&req.state_path, &req.mem_path).await
            .context("FC load_snapshot failed")?;

        // 5. Configure new network interface (network is NOT preserved after restore)
        fc.put_network_interface("eth0", &net_cfg.tap_name, &net_cfg.guest_mac).await?;

        // 6. Resume VM — but keep in quarantine (caller does reseal before opening egress)
        fc.resume().await.context("FC resume after restore failed")?;

        let vm_id = format!("vm-{}", ws_id);
        let vsock_uds = PathBuf::from(&self.config.workspaces_dir).join(ws_id).join("vm.vsock");
        let mux = Arc::new(SessionMux::new(vsock_uds.to_string_lossy().to_string()));

        // Wait for guest-agent to come back (it restarts after resume)
        wait_for_guest_agent(&mux, Duration::from_secs(30)).await
            .context("guest-agent did not come back after restore")?;

        info!(%ws_id, "VM restored, guest-agent ready, entering quarantine");

        let entry = Arc::new(VmEntry {
            vm_id: vm_id.clone(),
            workspace_id: ws_id.clone(),
            fc_socket_path: fc_socket.to_string_lossy().into(),
            vsock_uds_path: vsock_uds.to_string_lossy().into(),
            net_cfg: net_cfg.clone(),
            mux,
            _child: Mutex::new(Some(child)),
        });
        self.vms.write().insert(ws_id.clone(), entry);

        Ok(VmRestoreResponse {
            vm_id,
            vsock_path: vsock_uds.to_string_lossy().into(),
            guest_ip:   net_cfg.guest_ip,
        })
    }

    // ── Destroy ──────────────────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn destroy(&self, ws_id: &str) -> Result<()> {
        let entry = {
            let mut vms = self.vms.write();
            vms.remove(ws_id)
        };

        if let Some(entry) = entry {
            // Kill Firecracker process — extract child before any .await
            let child_opt = entry._child.lock().take(); // guard drops here
            if let Some(mut child) = child_opt {
                let _ = child.kill().await;
            }
            // Tear down network — run on blocking thread (uses ip commands)
            let net_cfg = entry.net_cfg.clone();
            tokio::task::spawn_blocking(move || teardown_network(&net_cfg))
                .await?
                .ok(); // best-effort
        }

        // Clean up workspace directory
        let ws_dir = PathBuf::from(&self.config.workspaces_dir).join(ws_id);
        let _ = tokio::fs::remove_dir_all(&ws_dir).await;

        info!(%ws_id, "VM destroyed");
        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn get_vm(&self, ws_id: &str) -> Result<Arc<VmEntry>> {
        self.vms.read()
            .get(ws_id)
            .cloned()
            .ok_or_else(|| anyhow!("VM not found for workspace {}", ws_id))
    }

    async fn spawn_jailer(
        &self,
        ws_id: &str,
        fc_socket: &PathBuf,
        net_cfg: &NetConfig,
    ) -> Result<Child> {
        let netns_name = format!("arbor-{}", &ws_id[..8]);
        let netns_path = format!("/var/run/netns/{}", netns_name);
        let id = format!("arbor-{}", ws_id);

        let child = Command::new(&self.config.jailer_binary)
            .args([
                "--cgroup-version", "2",
                "--id",             &id,
                "--exec-file",      &self.config.fc_binary,
                "--uid",            &self.config.jailer_uid.to_string(),
                "--gid",            &self.config.jailer_gid.to_string(),
                "--netns",          &netns_path,
                "--",               // separator: rest are FC args
                "--api-sock",       &fc_socket.to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn jailer")?;

        Ok(child)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn create_overlay_disk(base: &PathBuf, overlay: &PathBuf, disk_gb: u32) -> Result<()> {
    // Copy base image to overlay path, then extend to requested size
    tokio::fs::copy(base, overlay).await
        .with_context(|| format!("copy {} -> {}", base.display(), overlay.display()))?;

    // Extend with truncate
    let output = tokio::process::Command::new("truncate")
        .args([
            "-s", &format!("{}G", disk_gb),
            &overlay.to_string_lossy(),
        ])
        .output()
        .await?;

    if !output.status.success() {
        bail!("truncate failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

async fn wait_for_socket(path: &PathBuf, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() { return Ok(()); }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for socket {}", path.display());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_guest_agent(mux: &SessionMux, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(true) = mux.ping().await { return Ok(()); }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for guest-agent");
        }
        sleep(Duration::from_millis(200)).await;
    }
}
