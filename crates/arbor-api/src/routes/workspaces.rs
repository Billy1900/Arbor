#![allow(unused_imports)]
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post, put},
    Router,
};
use uuid::Uuid;

use arbor_common::*;
use crate::{internal_err, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/workspaces",                              post(create_workspace))
        .route("/v1/workspaces/:ws_id",                       get(get_workspace))
        .route("/v1/workspaces/:ws_id/exec",                  post(exec_session))
        .route("/v1/workspaces/:ws_id/checkpoints",           post(create_checkpoint))
        .route("/v1/workspaces/:ws_id/checkpoints",           get(list_checkpoints))
        .route("/v1/workspaces/:ws_id:terminate",             post(terminate_workspace))
        .route("/v1/workspaces/:ws_id/secrets/grants/:grant_id", put(upsert_grant))
        .route("/v1/workspaces/:ws_id/secrets/grants/:grant_id", delete(revoke_grant))
        .route("/v1/checkpoints/:ckpt_id/restore",            post(restore_checkpoint))
        .route("/v1/checkpoints/:ckpt_id/fork",               post(fork_checkpoint))
        .route("/v1/operations/:op_id",                       get(get_operation))
}

// ── POST /v1/workspaces ───────────────────────────────────────────────────────
async fn create_workspace(State(st): State<AppState>, Json(req): Json<CreateWorkspaceRequest>)
    -> impl IntoResponse
{
    let ctrl = std::sync::Arc::clone(&st.controller);
    match ctrl.create_workspace(req).await {
        Ok((ws, op)) => (StatusCode::ACCEPTED, Json(serde_json::json!({
            "workspace_id": ws.id, "operation_id": op.id, "state": ws.state
        }))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── GET /v1/workspaces/:ws_id ─────────────────────────────────────────────────
async fn get_workspace(State(st): State<AppState>, Path(id): Path<Uuid>) -> impl IntoResponse {
    match st.controller.db.get_workspace(WorkspaceId(id)).await {
        Ok(Some(ws)) => Json(ws).into_response(),
        Ok(None)     => (StatusCode::NOT_FOUND, Json(ApiError::new("WORKSPACE_NOT_FOUND","not found",false))).into_response(),
        Err(e)       => internal_err(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id/exec ──────────────────────────────────────────
async fn exec_session(State(st): State<AppState>, Path(ws_id): Path<Uuid>, Json(req): Json<ExecRequest>)
    -> impl IntoResponse
{
    match st.controller.exec_session(WorkspaceId(ws_id), req).await {
        Ok(sess) => (StatusCode::ACCEPTED, Json(ExecResponse {
            session_id: sess.id,
            status:     sess.status,
            attachable: sess.pty,
        })).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id/checkpoints ────────────────────────────────────
async fn create_checkpoint(State(st): State<AppState>, Path(ws_id): Path<Uuid>, Json(req): Json<CheckpointRequest>)
    -> impl IntoResponse
{
    let ctrl = std::sync::Arc::clone(&st.controller);
    match ctrl.create_checkpoint(WorkspaceId(ws_id), req).await {
        Ok((ckpt, op)) => (StatusCode::ACCEPTED, Json(CheckpointResponse {
            checkpoint_id: ckpt.id,
            operation_id:  op.id,
            state:         ckpt.state,
        })).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── GET /v1/workspaces/:ws_id/checkpoints ─────────────────────────────────────
async fn list_checkpoints(State(st): State<AppState>, Path(ws_id): Path<Uuid>) -> impl IntoResponse {
    match st.controller.db.list_checkpoints_for_workspace(WorkspaceId(ws_id)).await {
        Ok(v)  => Json(v).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── POST /v1/workspaces/:ws_id:terminate ─────────────────────────────────────
async fn terminate_workspace(State(st): State<AppState>, Path(ws_id): Path<Uuid>) -> impl IntoResponse {
    let ctrl = std::sync::Arc::clone(&st.controller);
    match ctrl.terminate_workspace(WorkspaceId(ws_id)).await {
        Ok(op) => (StatusCode::ACCEPTED, Json(serde_json::json!({"operation_id": op.id, "status":"pending"}))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── POST /v1/checkpoints/:ckpt_id/restore ────────────────────────────────────
async fn restore_checkpoint(State(st): State<AppState>, Path(ckpt_id): Path<Uuid>, Json(req): Json<RestoreRequest>)
    -> impl IntoResponse
{
    let ctrl = std::sync::Arc::clone(&st.controller);
    match ctrl.restore_checkpoint(CheckpointId(ckpt_id), req).await {
        Ok((ws, op)) => (StatusCode::ACCEPTED, Json(serde_json::json!({
            "workspace_id": ws.id, "operation_id": op.id, "state": ws.state
        }))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── POST /v1/checkpoints/:ckpt_id/fork ───────────────────────────────────────
async fn fork_checkpoint(State(st): State<AppState>, Path(ckpt_id): Path<Uuid>, Json(req): Json<RestoreRequest>)
    -> impl IntoResponse
{
    let ctrl = std::sync::Arc::clone(&st.controller);
    match ctrl.fork_checkpoint(CheckpointId(ckpt_id), req).await {
        Ok((ws, op)) => (StatusCode::ACCEPTED, Json(serde_json::json!({
            "workspace_id": ws.id, "operation_id": op.id, "state": ws.state,
            "message": "workspace enters quarantine; reseal hooks running"
        }))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── GET /v1/operations/:op_id ─────────────────────────────────────────────────
async fn get_operation(State(st): State<AppState>, Path(op_id): Path<Uuid>) -> impl IntoResponse {
    match st.controller.db.get_operation(OperationId(op_id)).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None)    => (StatusCode::NOT_FOUND, Json(ApiError::new("OPERATION_NOT_FOUND","not found",false))).into_response(),
        Err(e)      => internal_err(e).into_response(),
    }
}

// ── PUT /v1/workspaces/:ws_id/secrets/grants/:grant_id ───────────────────────
async fn upsert_grant(
    State(st):  State<AppState>,
    Path((ws_id, grant_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    use arbor_egress_proxy::{ProxyGrant, InjectKind};
    use chrono::Utc;

    let provider:     String     = body["provider"].as_str().unwrap_or("").to_string();
    let vault_ref:    String     = body["vault_ref"].as_str().unwrap_or("").to_string();
    let allowed:      Vec<String> = body["allowed_hosts"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let ttl: u64 = body["ttl_seconds"].as_u64().unwrap_or(3600);
    let now = Utc::now();

    // Resolve credential from environment
    use arbor_controller::reseal::{EnvSecretResolver, SecretResolver};
    let resolver = EnvSecretResolver;
    let credential = match resolver.resolve(&vault_ref).await {
        Ok(v)  => v,
        Err(e) => return (StatusCode::BAD_REQUEST,
            Json(ApiError::new("SECRET_POLICY_DENIED", &e.to_string(), false))).into_response(),
    };

    // Push to live egress proxy registry
    st.proxy_state.registry.upsert(ProxyGrant {
        workspace_id: WorkspaceId(ws_id),
        provider: provider.clone(),
        allowed_hosts: allowed.clone(),
        credential_value: credential,
        inject_kind: InjectKind::AuthorizationHeader,
    });

    // Persist in DB
    let grant = SecretGrant {
        id:           GrantId(grant_id),
        workspace_id: WorkspaceId(ws_id),
        provider,
        mode:         SecretMode::BrokeredProxy,
        vault_ref,
        allowed_hosts: allowed,
        ttl_seconds:   ttl,
        active:        true,
        expires_at:    Some(now + chrono::Duration::seconds(ttl as i64)),
        created_at:    now,
    };
    match st.controller.db.upsert_secret_grant(&grant).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status":"active","grant_id": grant_id}))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── DELETE /v1/workspaces/:ws_id/secrets/grants/:grant_id ────────────────────
async fn revoke_grant(
    State(st): State<AppState>,
    Path((ws_id, grant_id)): Path<(Uuid, Uuid)>,
) -> impl IntoResponse {
    st.proxy_state.registry.revoke(WorkspaceId(ws_id), ""); // revoke all providers for safety
    match st.controller.db.revoke_grant(WorkspaceId(ws_id), GrantId(grant_id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}
