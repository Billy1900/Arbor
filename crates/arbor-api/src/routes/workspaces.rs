use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use uuid::Uuid;

use arbor_common::*;
use crate::{anyhow_err_response, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/workspaces",             post(create_workspace))
        .route("/v1/workspaces/:ws_id",      get(get_workspace))
        .route("/v1/workspaces/:ws_id/exec", post(exec_session))
        .route("/v1/workspaces/:ws_id:terminate", post(terminate_workspace))
        .route("/v1/workspaces/:ws_id/checkpoints", post(create_checkpoint))
        .route("/v1/checkpoints/:ckpt_id/restore",  post(restore_checkpoint))
        .route("/v1/checkpoints/:ckpt_id/fork",      post(fork_checkpoint))
        .route("/v1/operations/:op_id", get(get_operation))
}

// ── POST /v1/workspaces ───────────────────────────────────────────────────────

async fn create_workspace(
    State(st): State<AppState>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> impl IntoResponse {
    match st.controller.create_workspace(req).await {
        Ok((ws, op)) => (
            StatusCode::ACCEPTED,
            Json(CreateWorkspaceResponse {
                workspace_id: ws.id,
                operation_id: op.id,
                state: ws.state,
            }),
        )
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}

// ── GET /v1/workspaces/:ws_id ─────────────────────────────────────────────────

async fn get_workspace(
    State(st): State<AppState>,
    Path(ws_id): Path<Uuid>,
) -> impl IntoResponse {
    match st.controller.db.get_workspace(WorkspaceId(ws_id)).await {
        Ok(Some(ws)) => Json(ws).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("WORKSPACE_NOT_FOUND", "workspace not found", false)),
        )
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id/exec ──────────────────────────────────────────

async fn exec_session(
    State(st): State<AppState>,
    Path(ws_id): Path<Uuid>,
    Json(req): Json<ExecRequest>,
) -> impl IntoResponse {
    match st.controller.exec_session(WorkspaceId(ws_id), req).await {
        Ok(sess) => (
            StatusCode::ACCEPTED,
            Json(ExecResponse {
                session_id: sess.id,
                status: sess.status,
                attachable: sess.pty,
            }),
        )
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id:terminate ─────────────────────────────────────

async fn terminate_workspace(
    State(st): State<AppState>,
    Path(ws_id): Path<Uuid>,
) -> impl IntoResponse {
    match st.controller.terminate_workspace(WorkspaceId(ws_id)).await {
        Ok(op) => (StatusCode::ACCEPTED, Json(serde_json::json!({
            "operation_id": op.id,
            "status": "pending"
        })))
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id/checkpoints ────────────────────────────────────

async fn create_checkpoint(
    State(st): State<AppState>,
    Path(ws_id): Path<Uuid>,
    Json(req): Json<CheckpointRequest>,
) -> impl IntoResponse {
    match st.controller.create_checkpoint(WorkspaceId(ws_id), req).await {
        Ok((ckpt, op)) => (
            StatusCode::ACCEPTED,
            Json(CheckpointResponse {
                checkpoint_id: ckpt.id,
                operation_id: op.id,
                state: ckpt.state,
            }),
        )
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}

// ── POST /v1/checkpoints/:ckpt_id/restore ────────────────────────────────────

async fn restore_checkpoint(
    State(st): State<AppState>,
    Path(ckpt_id): Path<Uuid>,
    Json(req): Json<RestoreRequest>,
) -> impl IntoResponse {
    // M1: restore not yet implemented — returns 501
    let _ = (st, ckpt_id, req);
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ApiError::new(
            "UNSUPPORTED_OPERATION_IN_MVP",
            "restore will be available in M3",
            false,
        )),
    )
        .into_response()
}

// ── POST /v1/checkpoints/:ckpt_id/fork ───────────────────────────────────────

async fn fork_checkpoint(
    State(st): State<AppState>,
    Path(ckpt_id): Path<Uuid>,
    Json(req): Json<RestoreRequest>,
) -> impl IntoResponse {
    let _ = (st, ckpt_id, req);
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ApiError::new(
            "UNSUPPORTED_OPERATION_IN_MVP",
            "fork will be available in M4",
            false,
        )),
    )
        .into_response()
}

// ── GET /v1/operations/:op_id ────────────────────────────────────────────────

async fn get_operation(
    State(st): State<AppState>,
    Path(op_id): Path<Uuid>,
) -> impl IntoResponse {
    match st.controller.db.get_operation(OperationId(op_id)).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("OPERATION_NOT_FOUND", "operation not found", false)),
        )
            .into_response(),
        Err(e) => anyhow_err_response(e).into_response(),
    }
}
