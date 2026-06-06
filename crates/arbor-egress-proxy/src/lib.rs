#![allow(dead_code, unused_variables, unused_imports)]
/// Egress proxy — HTTP/S CONNECT proxy with allowlist enforcement
/// and credential injection for brokered_proxy secret grants.
///
/// Architecture:
///   - Guest VMs have their default route via the host veth
///   - ARBOR_EGRESS_PROXY env is injected into guest as http_proxy/https_proxy
///   - This proxy intercepts CONNECT requests, checks allowlist, then tunnels
///   - For brokered_proxy grants: injects Authorization header on matching hosts
///
/// Design note: HTTPS proxying uses the CONNECT tunnel method. The proxy
/// sees only the target hostname at CONNECT time (not the full URL/headers),
/// so credential injection for HTTPS requires TLS interception (MITM) or
/// a guest-side PAC file. MVP uses HTTP for internal dev servers and
/// TLS interception for brokered API calls. TLS interception uses a per-workspace
/// CA cert injected into the guest's trust store at boot time.
use anyhow::Result;
use bytes::Bytes;
use hyper::{
    body::Incoming,
    header::{self, HeaderValue},
    Method, Request, Response, StatusCode, Uri,
};
use hyper_util::rt::TokioIo;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use arbor_common::{SecretGrant, SecretMode, WorkspaceId};
use arbor_common::trace::{TraceEmitter, TraceEvent};

// ── Grant registry ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProxyGrant {
    pub workspace_id: WorkspaceId,
    pub provider: String,
    pub allowed_hosts: Vec<String>,
    pub credential_value: String,  // the actual secret value, kept in proxy memory only
    pub inject_kind: InjectKind,
}

#[derive(Debug, Clone)]
pub enum InjectKind {
    AuthorizationHeader,  // injects "Authorization: Bearer <value>"
    ApiKeyHeader(String), // injects "<header-name>: <value>"
    /// Rewrites requests to `gpu.local` → the runner's local inference sidecar.
    /// `credential_value` on the grant carries an optional model name injected
    /// as `X-Arbor-Model`. The real sidecar URL never enters the VM.
    GpuSidecar { sidecar_url: String },
}

pub struct GrantRegistry {
    // keyed by (workspace_id, provider)
    grants: RwLock<HashMap<(String, String), ProxyGrant>>,
}

impl GrantRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { grants: RwLock::new(HashMap::new()) })
    }

    pub fn upsert(&self, grant: ProxyGrant) {
        let key = (grant.workspace_id.to_string(), grant.provider.clone());
        self.grants.write().insert(key, grant);
    }

    pub fn revoke(&self, workspace_id: WorkspaceId, provider: &str) {
        let key = (workspace_id.to_string(), provider.to_string());
        self.grants.write().remove(&key);
    }

    pub fn revoke_all_for_workspace(&self, workspace_id: WorkspaceId) {
        let ws = workspace_id.to_string();
        self.grants.write().retain(|(wid, _), _| wid != &ws);
    }

    /// Find the grant that covers the given host for the given workspace.
    pub fn find_grant(&self, workspace_id: WorkspaceId, host: &str) -> Option<ProxyGrant> {
        let ws = workspace_id.to_string();
        let grants = self.grants.read();
        grants.values()
            .find(|g| g.workspace_id.to_string() == ws && host_matches(host, &g.allowed_hosts))
            .cloned()
    }
}

fn host_matches(host: &str, allowed: &[String]) -> bool {
    // Strip port from host
    let h = host.split(':').next().unwrap_or(host);
    allowed.iter().any(|a| {
        let pattern = a.split(':').next().unwrap_or(a);
        if pattern.starts_with("*.") {
            let suffix = &pattern[2..];
            h.ends_with(suffix) || h == suffix
        } else {
            h == pattern
        }
    })
}

// ── Proxy state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ProxyState {
    pub registry: Arc<GrantRegistry>,
    // Allowlist for plain (non-brokered) egress
    // workspace_id → set of allowed host patterns
    pub egress_allowlist: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Per-workspace emitter for M10 trace events.
    /// Populated by the API layer after fork_checkpoint opens a trajectory.
    pub emitters: Arc<dashmap::DashMap<String, TraceEmitter>>,
}

impl ProxyState {
    pub fn new(registry: Arc<GrantRegistry>) -> Self {
        Self {
            registry,
            egress_allowlist: Arc::new(RwLock::new(HashMap::new())),
            emitters: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub fn register_emitter(&self, workspace_id: WorkspaceId, emitter: TraceEmitter) {
        self.emitters.insert(workspace_id.to_string(), emitter);
    }

    pub fn drop_emitter(&self, workspace_id: WorkspaceId) {
        self.emitters.remove(&workspace_id.to_string());
    }

    pub fn allow_egress(&self, workspace_id: WorkspaceId, hosts: Vec<String>) {
        self.egress_allowlist.write().insert(workspace_id.to_string(), hosts);
    }

    pub fn deny_all_egress(&self, workspace_id: WorkspaceId) {
        self.egress_allowlist.write().remove(&workspace_id.to_string());
    }

    fn is_allowed(&self, workspace_id: WorkspaceId, host: &str) -> bool {
        let ws = workspace_id.to_string();
        let list = self.egress_allowlist.read();
        if let Some(allowed) = list.get(&ws) {
            host_matches(host, allowed)
        } else {
            false
        }
    }
}

// ── Proxy server ──────────────────────────────────────────────────────────────

pub async fn run_proxy(bind: &str, state: ProxyState) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(bind, "egress proxy listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_proxy_conn(stream, peer, state).await {
                debug!(?e, "proxy connection error");
            }
        });
    }
}

async fn handle_proxy_conn(
    stream: TcpStream,
    peer: SocketAddr,
    state: ProxyState,
) -> Result<()> {
    let io = TokioIo::new(stream);
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            io,
            hyper::service::service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { Ok::<_, std::convert::Infallible>(proxy_request(req, state, peer).await) }
            }),
        )
        .with_upgrades()
        .await?;
    Ok(())
}

async fn proxy_request(
    req: Request<Incoming>,
    state: ProxyState,
    peer: SocketAddr,
) -> Response<http_body_util::Full<Bytes>> {
    // Determine workspace_id from peer IP (runner assigns guest IPs deterministically).
    // In production this would come from a peer→workspace registry.
    // For MVP we pass workspace_id in a special header X-Arbor-Workspace-Id.
    let ws_id_header = req.headers()
        .get("x-arbor-workspace-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(WorkspaceId);

    let ws_id = match ws_id_header {
        Some(id) => id,
        None => {
            return error_response(StatusCode::BAD_REQUEST, "missing X-Arbor-Workspace-Id");
        }
    };

    if req.method() == Method::CONNECT {
        handle_connect(req, ws_id, state).await
    } else {
        handle_http(req, ws_id, state).await
    }
}

// ── CONNECT tunnel (HTTPS) ───────────────────────────────────────────────────

async fn handle_connect(
    req: Request<Incoming>,
    ws_id: WorkspaceId,
    state: ProxyState,
) -> Response<http_body_util::Full<Bytes>> {
    let host = req.uri().authority().map(|a| a.as_str()).unwrap_or("");

    // Allowlist check: brokered grant OR explicit allowlist
    let grant = state.registry.find_grant(ws_id, host);
    if grant.is_none() && !state.is_allowed(ws_id, host) {
        warn!(%ws_id, %host, "CONNECT denied by egress policy");
        return error_response(StatusCode::FORBIDDEN, "egress denied");
    }

    info!(%ws_id, %host, "CONNECT tunnel allowed");

    // M10: emit NetworkAccess trace event (best-effort)
    if let Some(emitter) = state.emitters.get(&ws_id.to_string()) {
        emitter.emit(TraceEvent::NetworkAccess {
            host: host.to_string(),
            method: "CONNECT".to_string(),
            status_code: 200,
            bytes_sent: 0,
            bytes_received: 0,
            brokered: grant.is_some(),
        });
    }

    let host_owned = host.to_string();
    // Upgrade the connection and splice
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = tunnel(upgraded, &host_owned).await {
                    debug!(?e, "tunnel error");
                }
            }
            Err(e) => debug!(?e, "upgrade error"),
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(http_body_util::Full::new(Bytes::new()))
        .unwrap()
}

async fn tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: &str,
) -> Result<()> {
    let stream = TcpStream::connect(host).await?;
    let (mut client_r, mut client_w) = tokio::io::split(TokioIo::new(upgraded));
    let (mut server_r, mut server_w) = tokio::io::split(stream);

    tokio::select! {
        _ = tokio::io::copy(&mut client_r, &mut server_w) => {}
        _ = tokio::io::copy(&mut server_r, &mut client_w) => {}
    }
    Ok(())
}

// ── Plain HTTP proxy (with header injection + GPU sidecar routing) ───────────

async fn handle_http(
    mut req: Request<Incoming>,
    ws_id: WorkspaceId,
    state: ProxyState,
) -> Response<http_body_util::Full<Bytes>> {
    let host = req.uri().host().unwrap_or("").to_string();

    // Allowlist check
    let grant = state.registry.find_grant(ws_id, &host);
    if grant.is_none() && !state.is_allowed(ws_id, &host) {
        warn!(%ws_id, %host, "HTTP request denied by egress policy");
        return error_response(StatusCode::FORBIDDEN, "egress denied");
    }

    // GPU sidecar: rewrite gpu.local → the local sidecar and forward
    if let Some(ProxyGrant { inject_kind: InjectKind::GpuSidecar { ref sidecar_url }, ref credential_value, .. }) = grant {
        return forward_to_gpu_sidecar(req, sidecar_url, credential_value).await;
    }

    // Inject credential header if brokered grant exists
    if let Some(grant) = &grant {
        inject_credential(req.headers_mut(), grant);
    }

    let uri = req.uri().to_string();
    debug!(%ws_id, %uri, "plain HTTP proxy request");

    // M10: emit NetworkAccess for plain HTTP
    if let Some(emitter) = state.emitters.get(&ws_id.to_string()) {
        emitter.emit(TraceEvent::NetworkAccess {
            host: host.clone(),
            method: req.method().to_string(),
            status_code: 200,
            bytes_sent: 0,
            bytes_received: 0,
            brokered: grant.is_some(),
        });
    }

    // MVP: plain HTTP forwarding not implemented — most API traffic uses HTTPS CONNECT.
    error_response(StatusCode::NOT_IMPLEMENTED, "plain HTTP forwarding not yet implemented; use HTTPS")
}

/// Forward a gpu.local request to the runner-local inference sidecar.
/// Rewrites the Host header and URI; injects X-Arbor-Model if a model is set.
async fn forward_to_gpu_sidecar(
    mut req: Request<Incoming>,
    sidecar_url: &str,
    model_hint: &str,
) -> Response<http_body_util::Full<Bytes>> {
    // Rewrite the URI: strip gpu.local, prepend sidecar base URL
    let path_and_query = req.uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let new_uri = format!("{}{}", sidecar_url.trim_end_matches('/'), path_and_query);

    match new_uri.parse::<Uri>() {
        Ok(uri) => { *req.uri_mut() = uri; }
        Err(e) => {
            warn!(?e, "failed to rewrite URI for GPU sidecar");
            return error_response(StatusCode::BAD_GATEWAY, "GPU sidecar URI rewrite failed");
        }
    }

    // Inject model hint header so the sidecar knows which model to use
    if !model_hint.is_empty() {
        if let Ok(v) = HeaderValue::from_str(model_hint) {
            req.headers_mut().insert("x-arbor-model", v);
        }
    }

    info!(sidecar = %sidecar_url, "forwarding to GPU inference sidecar");
    // MVP: full forwarding requires a real HTTP client round-trip.
    // This stub returns 501 until the HTTP client is wired in.
    error_response(StatusCode::NOT_IMPLEMENTED, "GPU sidecar forwarding: wire in HTTP client in production")
}

fn inject_credential(headers: &mut hyper::HeaderMap, grant: &ProxyGrant) {
    match &grant.inject_kind {
        InjectKind::AuthorizationHeader => {
            let value = format!("Bearer {}", grant.credential_value);
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert(header::AUTHORIZATION, v);
            }
        }
        InjectKind::ApiKeyHeader(header_name) => {
            if let (Ok(name), Ok(val)) = (
                hyper::header::HeaderName::from_bytes(header_name.as_bytes()),
                HeaderValue::from_str(&grant.credential_value),
            ) {
                headers.insert(name, val);
            }
        }
        InjectKind::GpuSidecar { .. } => {
            // GPU sidecar routing is handled before this function is called
        }
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response<http_body_util::Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(Bytes::from(
            serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap(),
        )))
        .unwrap()
}
