//! Runner node management routes (M6 + M8).
//! Runner agents call POST /internal/runners/heartbeat every 15s.
//! M8 adds registration validation: runner_class ↔ arch ↔ cpu_template must be consistent.
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Json}, routing::post, Router};
use serde::Deserialize;
use arbor_common::*;
use crate::{arbor_err, internal_err, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/internal/runners/heartbeat",      post(heartbeat))
        .route("/internal/runners/register",       post(register))
        .route("/internal/runners/:id/drain",      axum::routing::put(drain_runner))
        .route("/internal/runners/:id",            axum::routing::delete(deregister_runner))
        .route("/v1/runners",                      axum::routing::get(list_runners))
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Enforces that runner_class, arch, and cpu_template are mutually consistent.
///
/// Firecracker CPU templates are architecture-specific:
///   - `T2`  → Intel x86_64  (fc-x86_64-v1)
///   - `T2A` → ARM Graviton2 (fc-arm64-v1)
///
/// Mixing these silently produces VMs that either crash at boot or cannot be
/// safely restored — the snapshot compatibility_key catches the mismatch at
/// restore time, but we catch it earlier here to give a clear error message.
fn validate_runner_class(
    runner_class: &str,
    arch: &str,
    cpu_template: &str,
) -> Result<(), ArborError> {
    match runner_class {
        "fc-x86_64-v1" => {
            if arch != "x86_64" {
                return Err(ArborError::RunnerArchMismatch {
                    expected: "x86_64".into(),
                    got: arch.into(),
                });
            }
            if cpu_template != "T2" {
                return Err(ArborError::RunnerArchMismatch {
                    expected: "T2 (required for fc-x86_64-v1)".into(),
                    got: cpu_template.into(),
                });
            }
        }
        "fc-arm64-v1" => {
            if arch != "aarch64" {
                return Err(ArborError::RunnerArchMismatch {
                    expected: "aarch64".into(),
                    got: arch.into(),
                });
            }
            if cpu_template != "T2A" {
                return Err(ArborError::RunnerArchMismatch {
                    expected: "T2A (required for fc-arm64-v1)".into(),
                    got: cpu_template.into(),
                });
            }
        }
        unknown => {
            return Err(ArborError::RunnerArchMismatch {
                expected: "fc-x86_64-v1 or fc-arm64-v1".into(),
                got: unknown.into(),
            });
        }
    }
    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

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
    if let Err(e) = validate_runner_class(&req.runner_class, &req.arch, &req.cpu_template) {
        return arbor_err(e).into_response();
    }

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
