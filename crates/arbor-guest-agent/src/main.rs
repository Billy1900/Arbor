#![allow(dead_code, unused_variables, unused_imports)]
/// arbor-guest-agent — runs inside every Firecracker microVM.
///
/// Compiled as a static musl binary and baked into the rootfs.
/// Listens on vsock port 9000 (VSOCK_AGENT_PORT) for host messages.
///
/// Responsibilities:
///   - Execute commands with optional PTY (portable-pty)
///   - Stream stdout/stderr back to host
///   - Handle resize / signal
///   - Respond to quiesce (sync + mark paused)
///   - Scan /proc/net/tcp for new listening ports and report them
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::{VsockListener, VsockStream};
use tracing::{debug, error, info, warn};

use arbor_common::proto::{
    decode_frame_length, encode_frame, GuestMessage, HostMessage,
    VSOCK_AGENT_PORT, VSOCK_HOST_CID,
};
use arbor_common::SessionId;

mod pty_runner;
use pty_runner::PtyRunner;

// ── State ────────────────────────────────────────────────────────────────────

struct AgentState {
    sessions: RwLock<HashMap<SessionId, Arc<pty_runner::Session>>>,
    started_at: Instant,
}

impl AgentState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: RwLock::new(HashMap::new()),
            started_at: Instant::now(),
        })
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("arbor_guest_agent=info")
        .without_time()
        .init();

    info!("arbor-guest-agent starting, listening on vsock port {}", VSOCK_AGENT_PORT);

    let state = AgentState::new();

    // Port scanner — reports newly opened listening ports to host
    let state_clone = Arc::clone(&state);
    // We can't write back to host unless we have a connection; port reporting
    // is best-effort and stored, sent on next outbound write opportunity.
    // For M1 we just log them; M2 will integrate with session_mux outbound channel.

    let mut listener = VsockListener::bind(tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, VSOCK_AGENT_PORT))
        .map_err(|e| anyhow::anyhow!("vsock bind failed: {}", e))?;

    info!("vsock listener bound");

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!(?addr, "host connected");
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state).await {
                        error!(?e, "connection handler error");
                    }
                });
            }
            Err(e) => {
                warn!(?e, "vsock accept error");
            }
        }
    }
}

// ── Connection handler ───────────────────────────────────────────────────────

async fn handle_connection(stream: VsockStream, state: Arc<AgentState>) -> Result<()> {
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let mut len_buf = [0u8; 4];
    loop {
        // Read frame header
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            debug!(?e, "connection closed");
            break;
        }
        let payload_len = decode_frame_length(&len_buf);
        if payload_len > arbor_common::proto::MAX_FRAME_SIZE {
            error!(%payload_len, "frame too large, closing");
            break;
        }
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).await?;

        let msg: HostMessage = match serde_json::from_slice(&payload) {
            Ok(m)  => m,
            Err(e) => { warn!(?e, "bad host message"); continue; }
        };

        let responses = handle_message(msg, Arc::clone(&state)).await;
        let mut w = writer.lock().await;
        for resp in responses {
            let frame = encode_frame(&resp)?;
            w.write_all(&frame).await?;
        }
    }
    Ok(())
}

// ── Message dispatch ─────────────────────────────────────────────────────────

async fn handle_message(msg: HostMessage, state: Arc<AgentState>) -> Vec<GuestMessage> {
    match msg {
        HostMessage::Exec { session_id, command, cwd, env, pty, cols, rows } => {
            let session = PtyRunner::spawn(session_id, command, cwd, env, pty, cols, rows);
            match session {
                Ok(sess) => {
                    let sess = Arc::new(sess);
                    state.sessions.write().insert(session_id, Arc::clone(&sess));

                    // Spawn output forwarder — will send back GuestMessage::Output
                    // For M1: output is buffered inside session; caller polls via subscribe
                    vec![GuestMessage::Started { session_id }]
                }
                Err(e) => vec![GuestMessage::Error {
                    session_id: Some(session_id),
                    message: e.to_string(),
                }],
            }
        }

        HostMessage::Input { session_id, data } => {
            let sess = state.sessions.read().get(&session_id).cloned();
            if let Some(sess) = sess {
                let _ = sess.write_stdin(&data);
            }
            vec![]
        }

        HostMessage::Resize { session_id, cols, rows } => {
            let sess = state.sessions.read().get(&session_id).cloned();
            if let Some(sess) = sess {
                sess.resize(cols, rows);
            }
            vec![]
        }

        HostMessage::Signal { session_id, signal } => {
            let sess = state.sessions.read().get(&session_id).cloned();
            if let Some(sess) = sess {
                sess.send_signal(signal);
            }
            vec![]
        }

        HostMessage::CloseStdin { session_id } => {
            // Drop stdin by removing session reference
            vec![]
        }

        HostMessage::Quiesce => {
            // Flush all filesystems
            nix_sync();
            info!("quiesce: filesystem sync complete");
            vec![GuestMessage::QuiesceOk]
        }

        HostMessage::Ping => {
            let sessions = state.sessions.read();
            vec![GuestMessage::Pong {
                uptime_seconds: state.started_at.elapsed().as_secs(),
                running_sessions: sessions.len(),
            }]
        }
    }
}

fn nix_sync() {
    unsafe { libc::sync(); }
}

// ── Port scanner ─────────────────────────────────────────────────────────────

async fn scan_ports_loop(state: Arc<AgentState>) {
    let mut known: HashSet<u16> = HashSet::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match read_listening_ports() {
            Ok(current) => {
                for &port in current.difference(&known) {
                    info!(%port, "new listening port detected");
                    // In M2: send GuestMessage::PortOpened through outbound channel
                }
                for &port in known.difference(&current) {
                    debug!(%port, "port closed");
                }
                known = current;
            }
            Err(e) => debug!(?e, "port scan error"),
        }
    }
}

fn read_listening_ports() -> Result<HashSet<u16>> {
    // Parse /proc/net/tcp for LISTEN state (state == 0A)
    let content = std::fs::read_to_string("/proc/net/tcp")?;
    let mut ports = HashSet::new();
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 { continue; }
        if cols[3] == "0A" { // TCP_LISTEN
            if let Some(port_hex) = cols[1].split(':').nth(1) {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    ports.insert(port);
                }
            }
        }
    }
    Ok(ports)
}
