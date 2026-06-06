/// SessionMux — runner-agent side of the vsock channel to guest-agent.
///
/// Maintains a single vsock connection to the guest-agent and fans out
/// sessions over it using the length-prefixed JSON frame protocol defined
/// in arbor-common::proto.
///
/// Architecture:
///   - One background reader task demultiplexes incoming GuestMessages
///   - Senders enqueue HostMessages via a mpsc channel → writer task
///   - Per-session output is delivered via broadcast channels
///   - PTY attach is done by subscribing to a session's broadcast channel
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use arbor_common::proto::{
    decode_frame_length, encode_frame, GuestMessage, HostMessage, VSOCK_AGENT_PORT,
};
use arbor_common::trace::{TraceEmitter, TraceEvent};
use arbor_common::SessionId;

const OUTPUT_BROADCAST_CAP: usize = 512;
const CMD_CHANNEL_CAP: usize = 256;

// ── Per-session state ─────────────────────────────────────────────────────────

struct SessionState {
    output_tx:   broadcast::Sender<Vec<u8>>,
    exited_tx:   Option<oneshot::Sender<i32>>,
    output_tail: Vec<u8>,     // last 4 KiB of combined output for trace
    started_at:  std::time::Instant,
    command:     Vec<String>, // stored at Exec dispatch time for ExecStarted
}

// ── SessionMux ───────────────────────────────────────────────────────────────

pub struct SessionMux {
    vsock_path: String,
    cmd_tx:     mpsc::Sender<Vec<u8>>,
    sessions:   Arc<RwLock<HashMap<SessionId, SessionState>>>,
    quiesce_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    pong_tx:    Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// Injected by vm_manager after a trajectory is opened for this workspace.
    emitter:    Arc<RwLock<Option<TraceEmitter>>>,
}

impl SessionMux {
    pub fn new(vsock_path: String) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let sessions   = Arc::new(RwLock::new(HashMap::new()));
        let quiesce_tx = Arc::new(RwLock::new(None));
        let pong_tx    = Arc::new(RwLock::new(None));
        let emitter    = Arc::new(RwLock::new(None));

        let mux = Self {
            vsock_path: vsock_path.clone(),
            cmd_tx,
            sessions,
            quiesce_tx,
            pong_tx,
            emitter,
        };
        mux.start_background(vsock_path, cmd_rx);
        mux
    }

    /// Called by vm_manager once the trajectory is open for this workspace.
    pub fn set_emitter(&self, emitter: TraceEmitter) {
        *self.emitter.write() = Some(emitter);
    }

    // ── Public API ───────────────────────────────────────────────────────────

    /// Send a HostMessage to the guest-agent.
    pub async fn send(&self, msg: HostMessage) -> Result<()> {
        let frame = encode_frame(&msg)?;
        self.cmd_tx.send(frame.to_vec()).await
            .map_err(|_| anyhow::anyhow!("vsock command channel closed"))?;
        Ok(())
    }

    /// Store the command for a session so ExecStarted can include it.
    /// Call this right before sending HostMessage::Exec.
    pub fn register_exec(&self, session_id: SessionId, command: Vec<String>) {
        let mut s = self.sessions.write();
        let state = s.entry(session_id).or_insert_with(|| SessionState {
            output_tx:   broadcast::channel(OUTPUT_BROADCAST_CAP).0,
            exited_tx:   None,
            output_tail: Vec::new(),
            started_at:  std::time::Instant::now(),
            command:     Vec::new(),
        });
        state.command    = command;
        state.started_at = std::time::Instant::now();
        state.output_tail.clear();
    }

    /// Subscribe to output bytes for a session.
    pub fn subscribe_output(&self, session_id: SessionId) -> broadcast::Receiver<Vec<u8>> {
        let sessions = self.sessions.read();
        if let Some(s) = sessions.get(&session_id) {
            return s.output_tx.subscribe();
        }
        drop(sessions);
        // Create entry if not yet present (guest-agent might send Started shortly)
        let (tx, rx) = broadcast::channel(OUTPUT_BROADCAST_CAP);
        self.sessions.write().insert(session_id, SessionState {
            output_tx:   tx,
            exited_tx:   None,
            output_tail: Vec::new(),
            started_at:  std::time::Instant::now(),
            command:     Vec::new(),
        });
        rx
    }

    /// Ping the guest-agent and return true if it responds within timeout.
    pub async fn ping(&self) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        *self.pong_tx.write() = Some(tx);
        self.send(HostMessage::Ping).await?;
        Ok(timeout(Duration::from_secs(3), rx).await.is_ok())
    }

    /// Send quiesce and wait for acknowledgement.
    pub async fn wait_quiesce(&self, dur: Duration) -> bool {
        let (tx, rx) = oneshot::channel();
        *self.quiesce_tx.write() = Some(tx);
        if self.send(HostMessage::Quiesce).await.is_err() {
            return false;
        }
        timeout(dur, rx).await.is_ok()
    }

    // ── Background tasks ─────────────────────────────────────────────────────

    fn start_background(&self, vsock_path: String, mut cmd_rx: mpsc::Receiver<Vec<u8>>) {
        let sessions   = Arc::clone(&self.sessions);
        let quiesce_tx = Arc::clone(&self.quiesce_tx);
        let pong_tx    = Arc::clone(&self.pong_tx);
        let emitter    = Arc::clone(&self.emitter);

        tokio::spawn(async move {
            loop {
                match Self::connect_and_run(
                    &vsock_path,
                    &mut cmd_rx,
                    Arc::clone(&sessions),
                    Arc::clone(&quiesce_tx),
                    Arc::clone(&pong_tx),
                    Arc::clone(&emitter),
                ).await {
                    Ok(()) => {
                        info!("vsock connection closed cleanly");
                        break;
                    }
                    Err(e) => {
                        warn!(?e, "vsock connection error, reconnecting in 1s");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    }

    async fn connect_and_run(
        vsock_path: &str,
        cmd_rx: &mut mpsc::Receiver<Vec<u8>>,
        sessions: Arc<RwLock<HashMap<SessionId, SessionState>>>,
        quiesce_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
        pong_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
        emitter: Arc<RwLock<Option<TraceEmitter>>>,
    ) -> Result<()> {
        let stream = UnixStream::connect(vsock_path).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        info!(path = %vsock_path, "vsock connected to guest-agent");

        let mut len_buf = [0u8; 4];
        loop {
            tokio::select! {
                // Outbound: forward queued frames to guest
                Some(frame) = cmd_rx.recv() => {
                    writer.write_all(&frame).await?;
                }
                // Inbound: read and dispatch guest messages
                result = reader.read_exact(&mut len_buf) => {
                    result?;
                    let payload_len = decode_frame_length(&len_buf);
                    anyhow::ensure!(
                        payload_len <= arbor_common::proto::MAX_FRAME_SIZE,
                        "guest frame too large: {payload_len}"
                    );
                    let mut payload = vec![0u8; payload_len];
                    reader.read_exact(&mut payload).await?;
                    let msg: GuestMessage = match serde_json::from_slice(&payload) {
                        Ok(m)  => m,
                        Err(e) => { warn!(?e, "bad guest message"); continue; }
                    };
                    Self::dispatch(msg, &sessions, &quiesce_tx, &pong_tx, &emitter);
                }
            }
        }
    }

    fn dispatch(
        msg: GuestMessage,
        sessions:   &Arc<RwLock<HashMap<SessionId, SessionState>>>,
        quiesce_tx: &Arc<RwLock<Option<oneshot::Sender<()>>>>,
        pong_tx:    &Arc<RwLock<Option<oneshot::Sender<()>>>>,
        emitter:    &Arc<RwLock<Option<TraceEmitter>>>,
    ) {
        match msg {
            GuestMessage::Started { session_id } => {
                debug!(%session_id, "session started");
                let mut s = sessions.write();
                let state = s.entry(session_id).or_insert_with(|| SessionState {
                    output_tx:   broadcast::channel(OUTPUT_BROADCAST_CAP).0,
                    exited_tx:   None,
                    output_tail: Vec::new(),
                    started_at:  std::time::Instant::now(),
                    command:     Vec::new(),
                });
                // reset timer on each start confirmation
                state.started_at = std::time::Instant::now();
                // Emit ExecStarted — command was stored when the HostMessage::Exec was sent
                if let Some(ref e) = *emitter.read() {
                    e.emit(TraceEvent::ExecStarted {
                        session_id,
                        command: state.command.clone(),
                        cwd:     String::new(),
                    });
                }
            }

            GuestMessage::Output { session_id, data } => {
                let mut s = sessions.write();
                if let Some(entry) = s.get_mut(&session_id) {
                    let _ = entry.output_tx.send(data.clone());
                    // Keep only the last 4 KiB
                    entry.output_tail.extend_from_slice(&data);
                    let len = entry.output_tail.len();
                    if len > 4096 {
                        entry.output_tail.drain(..len - 4096);
                    }
                }
            }

            GuestMessage::Exited { session_id, exit_code } => {
                debug!(%session_id, %exit_code, "session exited");
                let mut s = sessions.write();
                let (duration_ms, stdout_tail) = if let Some(entry) = s.get_mut(&session_id) {
                    let ms = entry.started_at.elapsed().as_millis() as u64;
                    let tail = String::from_utf8_lossy(&entry.output_tail).into_owned();
                    if let Some(tx) = entry.exited_tx.take() {
                        let _ = tx.send(exit_code);
                    }
                    (ms, tail)
                } else {
                    (0, String::new())
                };
                s.remove(&session_id);

                // Emit ExecExited
                if let Some(ref e) = *emitter.read() {
                    e.emit(TraceEvent::ExecExited {
                        session_id,
                        exit_code,
                        duration_ms,
                        stdout_tail,
                    });
                }
            }

            GuestMessage::QuiesceOk => {
                if let Some(tx) = quiesce_tx.write().take() {
                    let _ = tx.send(());
                }
            }

            GuestMessage::Pong { .. } => {
                if let Some(tx) = pong_tx.write().take() {
                    let _ = tx.send(());
                }
            }

            GuestMessage::PortOpened { port, protocol } => {
                info!(%port, %protocol, "guest port opened");
            }

            GuestMessage::PortClosed { port } => {
                debug!(%port, "guest port closed");
            }

            GuestMessage::Error { session_id, message } => {
                error!(?session_id, %message, "guest-agent error");
            }
        }
    }
}
