#![allow(dead_code, unused_variables, unused_imports)]
mod fc_client;
mod netns;
mod session_mux;
mod vm_manager;

use std::sync::Arc;
use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;
use tower_http::trace::TraceLayer;
use tracing::info;

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
}

fn default_bind() -> String { "0.0.0.0:9090".into() }

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

    let mut vm_cfg = VmManagerConfig::default();
    if let Some(v) = cfg.fc_binary       { vm_cfg.fc_binary       = v; }
    if let Some(v) = cfg.jailer_binary   { vm_cfg.jailer_binary   = v; }
    if let Some(v) = cfg.kernel_path     { vm_cfg.kernel_path     = v; }
    if let Some(v) = cfg.workspaces_dir  { vm_cfg.workspaces_dir  = v; }
    if let Some(v) = cfg.base_images_dir { vm_cfg.base_images_dir = v; }

    let mgr   = Arc::new(VmManager::new(vm_cfg));
    let state = AppState { mgr };

    let app = Router::new()
        .route("/health",                    get(health))
        .route("/vms",                       post(create_vm))
        .route("/vms/restore",               post(restore_vm))
        .route("/vms/active/exec",           post(exec_vm))      // simplified for M1
        .route("/vms/:vm_id",                delete(destroy_vm))
        .route("/vms/:vm_id/checkpoint",     post(checkpoint_vm))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    info!(bind = %cfg.bind, "arbor-runner-agent listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "arbor-runner-agent" }))
}

async fn create_vm(
    State(st): State<AppState>,
    Json(req): Json<CreateVmRequest>,
) -> impl IntoResponse {
    match st.mgr.create_vm(req).await {
        Ok(resp) => (StatusCode::CREATED, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn exec_vm(
    State(st): State<AppState>,
    Json(req): Json<VmExecRequest>,
) -> impl IntoResponse {
    let ws_id = req.session_id.to_string(); // in M1 we use session_id; M2 uses proper vm_id
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
// Heartbeat task — registers with controller and sends periodic heartbeats

async fn heartbeat_loop(controller_url: String, runner_id: uuid::Uuid, mgr: Arc<VmManager>) {
    let client = reqwest::Client::new();
    let url    = format!("{}/internal/runners/heartbeat", controller_url.trim_end_matches('/'));
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        let used = mgr.active_vm_count();
        let _ = client.post(&url)
            .json(&serde_json::json!({ "runner_id": runner_id, "used_slots": used }))
            .send()
            .await;
    }
}
