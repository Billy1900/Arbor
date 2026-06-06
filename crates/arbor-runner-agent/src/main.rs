#![allow(dead_code, unused_variables, unused_imports)]
mod fc_client;
mod netns;
mod session_mux;
mod vm_manager;

use std::sync::Arc;
use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post, put},
    Router,
};
use serde::Deserialize;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use arbor_common::proto::{
    CreateVmRequest, VmCheckpointRequest, VmExecRequest, VmRestoreRequest,
};
use vm_manager::{VmManager, VmManagerConfig};

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AgentConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default)]
    fc_binary: Option<String>,
    #[serde(default)]
    jailer_binary: Option<String>,
    #[serde(default)]
    kernel_path: Option<String>,
    #[serde(default)]
    workspaces_dir: Option<String>,
    #[serde(default)]
    base_images_dir: Option<String>,

    // M6: self-registration
    #[serde(default)]
    controller_url: Option<String>,
    #[serde(default = "default_runner_class")]
    runner_class: String,
    #[serde(default = "default_capacity")]
    capacity_slots: u32,
    #[serde(default = "default_fc_version")]
    firecracker_version: String,
    /// CPU template: "T2" for x86_64, "T2A" for aarch64/Graviton2
    #[serde(default = "default_cpu_template")]
    cpu_template: String,
    /// Host architecture reported to the controller: "x86_64" or "aarch64"
    #[serde(default = "default_arch")]
    arch: String,
    /// The address the controller uses to reach this agent (e.g. http://10.0.0.5:9090)
    #[serde(default)]
    advertise_address: Option<String>,
}

fn default_bind()         -> String { "0.0.0.0:9090".into() }
fn default_runner_class() -> String { "fc-x86_64-v1".into() }
fn default_capacity()     -> u32    { 10 }
fn default_fc_version()   -> String { "1.9.0".into() }
fn default_cpu_template() -> String { "T2".into() }
fn default_arch()         -> String { "x86_64".into() }

// ── AppState ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    mgr: Arc<VmManager>,
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arbor_runner_agent=info,tower_http=debug".into()),
        )
        .json()
        .init();

    let cfg: AgentConfig = config::Config::builder()
        .add_source(config::Environment::with_prefix("ARBOR_RUNNER").separator("__"))
        .build()?
        .try_deserialize()?;

    // ── Prometheus metrics ────────────────────────────────────────────────────
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install prometheus recorder")?;

    // ── VmManager ─────────────────────────────────────────────────────────────
    let mut vm_cfg = VmManagerConfig::default();
    if let Some(v) = cfg.fc_binary       { vm_cfg.fc_binary       = v; }
    if let Some(v) = cfg.jailer_binary   { vm_cfg.jailer_binary   = v; }
    if let Some(v) = cfg.kernel_path     { vm_cfg.kernel_path     = v; }
    if let Some(v) = cfg.workspaces_dir  { vm_cfg.workspaces_dir  = v; }
    if let Some(v) = cfg.base_images_dir { vm_cfg.base_images_dir = v; }
    vm_cfg.cpu_template = cfg.cpu_template.clone();
    vm_cfg.arch         = cfg.arch.clone();

    let mgr = Arc::new(VmManager::new(vm_cfg));

    // ── Self-registration + heartbeat ─────────────────────────────────────────
    if let Some(controller_url) = cfg.controller_url.clone() {
        let advertise = cfg.advertise_address.clone()
            .unwrap_or_else(|| format!("http://localhost:{}", port_from_bind(&cfg.bind)));

        let runner_id = self_register(
            &controller_url,
            &advertise,
            &cfg.runner_class,
            cfg.capacity_slots,
            &cfg.firecracker_version,
            &cfg.cpu_template,
            &cfg.arch,
        ).await.context("failed to register with controller")?;

        info!(%runner_id, "registered with controller");

        let mgr_hb = Arc::clone(&mgr);
        tokio::spawn(heartbeat_loop(controller_url.clone(), runner_id, mgr_hb, cfg.capacity_slots));
    } else {
        warn!("ARBOR_RUNNER__CONTROLLER_URL not set — running without controller registration (dev mode)");
    }

    // ── Router ────────────────────────────────────────────────────────────────
    let state = AppState { mgr: Arc::clone(&mgr) };
    let prom  = prometheus_handle;

    let app = Router::new()
        .route("/health",                    get(health))
        .route("/metrics",                   get(move || async move { prom.render() }))
        .route("/drain",                     put(drain))
        .route("/vms",                       post(create_vm))
        .route("/vms/restore",               post(restore_vm))
        .route("/vms/active/exec",           post(exec_vm))
        .route("/vms/:vm_id",                delete(destroy_vm))
        .route("/vms/:vm_id/checkpoint",     post(checkpoint_vm))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    info!(bind = %cfg.bind, "arbor-runner-agent listening");

    // ── Graceful shutdown on SIGTERM ──────────────────────────────────────────
    let mgr_shutdown = Arc::clone(&mgr);
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
            info!("SIGTERM received — entering drain mode");
            mgr_shutdown.set_draining();

            // Wait up to 60 s for running VMs to finish/checkpoint
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(60);
            while mgr_shutdown.active_vm_count() > 0 {
                if tokio::time::Instant::now() >= deadline {
                    warn!(
                        active = mgr_shutdown.active_vm_count(),
                        "drain timeout — shutting down with VMs still running"
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            info!("drain complete — shutting down");
        });

    server.await?;
    Ok(())
}

// ── Registration ──────────────────────────────────────────────────────────────

async fn self_register(
    controller_url: &str,
    advertise_address: &str,
    runner_class: &str,
    capacity_slots: u32,
    firecracker_version: &str,
    cpu_template: &str,
    arch: &str,
) -> Result<uuid::Uuid> {
    let client = reqwest::Client::new();
    let url    = format!("{}/internal/runners/register", controller_url.trim_end_matches('/'));

    let resp = client.post(&url)
        .json(&serde_json::json!({
            "runner_class":        runner_class,
            "address":             advertise_address,
            "arch":                arch,
            "firecracker_version": firecracker_version,
            "cpu_template":        cpu_template,
            "capacity_slots":      capacity_slots,
        }))
        .send()
        .await
        .context("POST /internal/runners/register failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("register returned {status}: {body}");
    }

    let body: serde_json::Value = resp.json().await.context("parse register response")?;
    let id_str = body["runner_id"].as_str().context("register response missing runner_id")?;
    Ok(uuid::Uuid::parse_str(id_str).context("parse runner_id UUID")?)
}

// ── Heartbeat loop ────────────────────────────────────────────────────────────

async fn heartbeat_loop(
    controller_url: String,
    runner_id: uuid::Uuid,
    mgr: Arc<VmManager>,
    capacity_slots: u32,
) {
    let client = reqwest::Client::new();
    let url    = format!("{}/internal/runners/heartbeat", controller_url.trim_end_matches('/'));
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        let used = mgr.active_vm_count();
        match client.post(&url)
            .json(&serde_json::json!({ "runner_id": runner_id, "used_slots": used }))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r)  => warn!(status = %r.status(), "heartbeat non-success response"),
            Err(e) => warn!(?e, "heartbeat request failed"),
        }
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "arbor-runner-agent" }))
}

async fn drain(State(st): State<AppState>) -> impl IntoResponse {
    st.mgr.set_draining();
    let active = st.mgr.active_vm_count();
    info!(active, "drain requested via API");
    Json(serde_json::json!({ "draining": true, "active_vms": active }))
}

async fn create_vm(
    State(st): State<AppState>,
    Json(req): Json<CreateVmRequest>,
) -> impl IntoResponse {
    if st.mgr.is_draining() {
        return (StatusCode::SERVICE_UNAVAILABLE,
                "runner is draining".to_string()).into_response();
    }
    match st.mgr.create_vm(req).await {
        Ok(resp) => (StatusCode::CREATED, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn exec_vm(
    State(st): State<AppState>,
    Json(req): Json<VmExecRequest>,
) -> impl IntoResponse {
    let ws_id = req.session_id.to_string();
    match st.mgr.exec(&ws_id, req).await {
        Ok(resp) => Json(serde_json::to_value(resp).unwrap()).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn checkpoint_vm(
    State(st): State<AppState>,
    Path(vm_id): Path<String>,
    Json(req): Json<VmCheckpointRequest>,
) -> impl IntoResponse {
    match st.mgr.checkpoint(&vm_id, req).await {
        Ok(resp) => Json(serde_json::to_value(resp).unwrap()).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn restore_vm(
    State(st): State<AppState>,
    Json(req): Json<VmRestoreRequest>,
) -> impl IntoResponse {
    match st.mgr.restore(req).await {
        Ok(resp) => (StatusCode::CREATED, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn destroy_vm(
    State(st): State<AppState>,
    Path(vm_id): Path<String>,
) -> impl IntoResponse {
    match st.mgr.destroy(&vm_id).await {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn port_from_bind(bind: &str) -> &str {
    bind.rsplit_once(':').map(|(_, p)| p).unwrap_or("9090")
}
