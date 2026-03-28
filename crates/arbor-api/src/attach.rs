/// WebSocket attach endpoint.
///
/// Flow:
///   client ──WS──▶ arbor-api  ──vsock──▶ arbor-runner-agent ──vsock──▶ arbor-guest-agent
///
/// The runner-agent acts as a multiplexer: it holds a persistent vsock connection
/// to the guest-agent and fans out sessions over it. The API server opens a
/// separate vsock (or TCP) connection to the runner-agent's attach port for each
/// client attach request.
///
/// In M1 the API server directly proxies to the runner-agent's raw TCP attach
/// port (runner exposes one TCP port per PTY session). M2 will replace this with
/// a proper multiplexed vsock channel.
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use arbor_common::{SessionId, WorkspaceId};
use arbor_controller::state_machine::verify_attach_token;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/attach/:session_id", get(ws_attach))
        .route("/v1/sessions/:session_id/attach", axum::routing::post(get_attach_url))
}

#[derive(Deserialize)]
struct AttachQuery {
    token: String,
}

// ── POST /v1/sessions/:session_id/attach  ────────────────────────────────────
// Returns a signed WebSocket URL with a short-lived token.

async fn get_attach_url(
    State(st): State<AppState>,
    Path(session_id): Path<Uuid>,
) -> impl IntoResponse {
    // In M1 we don't validate session ownership — add auth middleware in M2.
    // We still need workspace_id; for now we look up session from DB.
    // Simplified: just build the URL with the session_id token for M1.
    let token = format!("m1-{}", session_id); // real signing in state_machine.rs
    let url = format!("{}/v1/attach/{}?token={}", "ws://localhost:8080", session_id, token);
    axum::response::Json(serde_json::json!({
        "transport": "websocket",
        "url": url,
        "expires_at": chrono::Utc::now() + chrono::Duration::minutes(15),
    }))
}

// ── GET /v1/attach/:session_id  ───────────────────────────────────────────────
// Upgrades to WebSocket then bridges to the runner-agent's PTY stream.

async fn ws_attach(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(q): Query<AttachQuery>,
) -> impl IntoResponse {
    // TODO: verify token against attach_token_secret
    // For M1 we just let all connects through to runner.
    info!(%session_id, "WebSocket attach request");
    ws.on_upgrade(move |socket| handle_attach(socket, session_id, st))
}

async fn handle_attach(socket: WebSocket, session_id: Uuid, st: AppState) {
    // Look up which runner host holds this session.
    // In M1: runner-agent exposes per-session TCP on ephemeral ports.
    // We store the (runner_address, port) in a shared map when exec is called.
    // For now we use a hard-coded dev runner address as placeholder.
    let runner_addr = "127.0.0.1:9100"; // TODO: look up from session registry

    match TcpStream::connect(runner_addr).await {
        Ok(stream) => {
            info!(%session_id, "connected to runner attach port");
            bridge_ws_tcp(socket, stream, session_id).await;
        }
        Err(e) => {
            warn!(%session_id, ?e, "failed to connect to runner attach port");
        }
    }
}

/// Bidirectional bridge between WebSocket and a raw TCP stream.
/// The TCP stream carries raw PTY bytes (no framing at this layer in M1).
async fn bridge_ws_tcp(ws: WebSocket, tcp: TcpStream, session_id: Uuid) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();

    // TCP → WebSocket
    let ws_tx_clone = tokio::sync::Mutex::new(ws_tx);
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 4096];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut tx = ws_tx_clone.lock().await;
                    if tx.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!(%session_id, ?e, "tcp read error");
                    break;
                }
            }
        }
    };

    // WebSocket → TCP
    let ws_to_tcp = async {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tcp_tx.write_all(&data).await.is_err() { break; }
                }
                Ok(Message::Text(text)) => {
                    if tcp_tx.write_all(text.as_bytes()).await.is_err() { break; }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = tcp_to_ws => {},
        _ = ws_to_tcp => {},
    }

    debug!(%session_id, "attach session ended");
}
