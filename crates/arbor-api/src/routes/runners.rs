//! Runner node management routes (M6).
//! Runner agents call POST /internal/runners/heartbeat every 15s.
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Json}, routing::post, Router};
use serde::Deserialize;
use arbor_common::*;
use crate::{internal_err, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/internal/runners/heartbeat",      post(heartbeat))
        .route("/internal/runners/register",       post(register))
        .route("/internal/runners/:id/drain",      axum::routing::put(drain_runner))
        .route("/internal/runners/:id",            axum::routing::delete(deregister_runner))
        .route("/v1/runners",                      axum::routing::get(list_runners))
}

#[derive(Deserialize)]
struct HeartbeatRequest {
    runner_id:  uuid::Uuid,
    used_slots: u32,
}

async fn heartbeat(State(st): State<AppState>, Json(req): Json<HeartbeatRequest>) -> impl IntoResponse {
    match st.controller.db.runner_heartbeat(RunnerId(req.runner_id), req.used_slots).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

#[derive(Deserialize)]
struct RegisterRequest {
    runner_class:        String,
    address:             String,
    arch:                String,
    firecracker_version: String,
    cpu_template:        String,
    capacity_slots:      u32,
}

async fn register(State(st): State<AppState>, Json(req): Json<RegisterRequest>) -> impl IntoResponse {
    let node = RunnerNode {
        id:                  RunnerId::new(),
        runner_class:        req.runner_class,
        address:             req.address,
        arch:                req.arch,
        firecracker_version: req.firecracker_version,
        cpu_template:        req.cpu_template,
        capacity_slots:      req.capacity_slots,
        used_slots:          0,
        healthy:             true,
        last_heartbeat:      chrono::Utc::now(),
    };
    match st.controller.db.upsert_runner(&node).await {
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({ "runner_id": node.id }))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

async fn list_runners(State(st): State<AppState>) -> impl IntoResponse {
    match st.controller.db.list_all_runners().await {
        Ok(v)  => Json(v).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

/// PUT /internal/runners/:id/drain — mark the runner as unhealthy so the scheduler
/// stops placing new workspaces on it. The runner-agent then drains in-flight VMs
/// and shuts down. This is the control-plane side of the drain handshake.
async fn drain_runner(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> impl IntoResponse {
    match st.controller.db.mark_runner_unhealthy(RunnerId(id)).await {
        Ok(()) => {
            tracing::info!(%id, "runner marked unhealthy for drain");
            metrics::counter!("arbor.runner.drain_initiated").increment(1);
            StatusCode::OK.into_response()
        }
        Err(e) => internal_err(e).into_response(),
    }
}

/// DELETE /internal/runners/:id — permanently remove a runner record after it has
/// drained and shut down. Idempotent if called on a non-existent runner.
async fn deregister_runner(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> impl IntoResponse {
    match st.controller.db.delete_runner(RunnerId(id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}
