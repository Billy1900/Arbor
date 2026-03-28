/// Wire protocol for runner-agent ↔ guest-agent communication over vsock.
///
/// Frame format (little-endian):
///   [4 bytes: payload_length][payload_length bytes: JSON]
///
/// All messages are JSON-encoded GuestMessage / HostMessage enums.
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::SessionId;

pub const VSOCK_HOST_CID: u32 = 2;
pub const VSOCK_GUEST_CID: u32 = 3;
pub const VSOCK_AGENT_PORT: u32 = 9000;
pub const MAX_FRAME_SIZE: usize = 32 * 1024 * 1024; // 32 MiB

// ── Messages sent from host (runner-agent) → guest (guest-agent) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// Start a new process, optionally with a PTY.
    Exec {
        session_id: SessionId,
        command: Vec<String>,
        cwd: String,
        env: HashMap<String, String>,
        pty: bool,
        cols: u16,
        rows: u16,
    },
    /// Send stdin data to a session.
    Input {
        session_id: SessionId,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Resize the PTY window.
    Resize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Send POSIX signal to process group.
    Signal {
        session_id: SessionId,
        signal: i32,
    },
    /// Close stdin for session.
    CloseStdin { session_id: SessionId },
    /// Request filesystem sync + quiesce before checkpoint.
    Quiesce,
    /// Health check.
    Ping,
}

// ── Messages sent from guest (guest-agent) → host (runner-agent) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMessage {
    /// Session started successfully.
    Started { session_id: SessionId },
    /// stdout/stderr output (PTY combines both).
    Output {
        session_id: SessionId,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Process exited.
    Exited {
        session_id: SessionId,
        exit_code: i32,
    },
    /// Error starting or running session.
    Error {
        session_id: Option<SessionId>,
        message: String,
    },
    /// Quiesce completed — safe to snapshot.
    QuiesceOk,
    /// Pong response to Ping.
    Pong {
        uptime_seconds: u64,
        running_sessions: usize,
    },
    /// Port opened inside guest (for preview proxy).
    PortOpened { port: u16, protocol: String },
    /// Port closed.
    PortClosed { port: u16 },
}

// ── Codec ────────────────────────────────────────────────────────────────────

pub fn encode_frame(msg: &impl Serialize) -> anyhow::Result<Bytes> {
    let payload = serde_json::to_vec(msg)?;
    anyhow::ensure!(
        payload.len() <= MAX_FRAME_SIZE,
        "frame too large: {} bytes",
        payload.len()
    );
    let mut buf = BytesMut::with_capacity(4 + payload.len());
    buf.put_u32_le(payload.len() as u32);
    buf.put_slice(&payload);
    Ok(buf.freeze())
}

pub fn decode_frame_length(header: &[u8; 4]) -> usize {
    u32::from_le_bytes(*header) as usize
}

// ── base64 serde helper for binary data ──────────────────────────────────────

pub mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        s.serialize_str(&base64_encode(bytes))
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where D: Deserializer<'de> {
        let s = String::deserialize(d)?;
        base64_decode(&s).map_err(serde::de::Error::custom)
    }

    fn base64_encode(input: &[u8]) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as usize;
            let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
            out.push(CHARS[(b0 >> 2) & 0x3f] as char);
            out.push(CHARS[((b0 << 4) | (b1 >> 4)) & 0x3f] as char);
            out.push(if chunk.len() > 1 { CHARS[((b1 << 2) | (b2 >> 6)) & 0x3f] as char } else { '=' });
            out.push(if chunk.len() > 2 { CHARS[b2 & 0x3f] as char } else { '=' });
        }
        out
    }

    fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
        // Use standard base64 alphabet
        let s = s.trim_end_matches('=');
        let mut out = Vec::new();
        let decode_char = |c: char| -> Result<u8, String> {
            match c {
                'A'..='Z' => Ok(c as u8 - b'A'),
                'a'..='z' => Ok(c as u8 - b'a' + 26),
                '0'..='9' => Ok(c as u8 - b'0' + 52),
                '+' => Ok(62),
                '/' => Ok(63),
                _ => Err(format!("invalid base64 char: {c}")),
            }
        };
        let chars: Vec<char> = s.chars().collect();
        for chunk in chars.chunks(4) {
            let v0 = decode_char(chunk[0])?;
            let v1 = decode_char(chunk[1])?;
            out.push((v0 << 2) | (v1 >> 4));
            if chunk.len() > 2 {
                let v2 = decode_char(chunk[2])?;
                out.push((v1 << 4) | (v2 >> 2));
                if chunk.len() > 3 {
                    let v3 = decode_char(chunk[3])?;
                    out.push((v2 << 6) | v3);
                }
            }
        }
        Ok(out)
    }
}

// ── Runner-agent ↔ Workspace-controller HTTP API types ───────────────────────

/// POST /vms — create and boot a VM
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateVmRequest {
    pub workspace_id: String,
    pub vcpu_count: u32,
    pub memory_mib: u32,
    pub disk_gb: u32,
    pub kernel_path: String,
    pub rootfs_path: String,
    pub tap_device: String,
    pub vsock_uds_path: String,
    pub base_image_id: String,
    pub repo_url: String,
    pub repo_ref: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateVmResponse {
    pub vm_id: String,
    pub vsock_path: String,
    pub tap_device: String,
    pub guest_ip: String,
    pub state: String,
}

/// POST /vms/{vm_id}/exec — start a session inside the VM
#[derive(Debug, Serialize, Deserialize)]
pub struct VmExecRequest {
    pub session_id: SessionId,
    pub command: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub pty: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmExecResponse {
    pub session_id: SessionId,
    pub started: bool,
}

/// POST /vms/{vm_id}/checkpoint — create Firecracker snapshot
#[derive(Debug, Serialize, Deserialize)]
pub struct VmCheckpointRequest {
    pub checkpoint_id: String,
    pub snapshot_dir: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmCheckpointResponse {
    pub state_path: String,
    pub mem_path: String,
    pub state_size_bytes: u64,
    pub mem_size_bytes: u64,
}

/// POST /vms/restore — restore a VM from snapshot files
#[derive(Debug, Serialize, Deserialize)]
pub struct VmRestoreRequest {
    pub workspace_id: String,
    pub checkpoint_id: String,
    pub state_path: String,
    pub mem_path: String,
    pub tap_device: String,
    pub vsock_uds_path: String,
    pub vcpu_count: u32,
    pub memory_mib: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmRestoreResponse {
    pub vm_id: String,
    pub vsock_path: String,
    pub guest_ip: String,
}
