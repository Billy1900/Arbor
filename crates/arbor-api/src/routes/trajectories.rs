/// M10 Trajectory Tracer — HTTP routes.
///
/// GET  /v1/trajectories/:tid               → TrajectoryRow
/// GET  /v1/trajectories/:tid/events        → Vec<TraceEventRow>  (?limit=50&after=0)
/// GET  /v1/trajectories/:tid/export        → NDJSON stream (chunked)
/// POST /v1/trajectories/:tid/replay        → fork from the nearest checkpoint before step N
///
/// POST /v1/rollouts                        → create rollout + fork N workspaces
/// GET  /v1/rollouts/:rid                   → RolloutSummary
/// GET  /v1/rollouts/:rid/diff?a=tid&b=tid  → TrajDiff
#[allow(unused_imports)]
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use arbor_common::*;
use crate::{internal_err, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        // Trajectory
        .route("/v1/trajectories/:tid",        get(get_trajectory))
        .route("/v1/trajectories/:tid/events", get(list_events))
        .route("/v1/trajectories/:tid/export", get(export_trajectory))
        .route("/v1/trajectories/:tid/replay", post(replay_trajectory))
        // Workspace → latest trajectory shortcut
        .route("/v1/workspaces/:ws_id/trajectory", get(get_workspace_trajectory))
        // Rollout
        .route("/v1/rollouts",          post(create_rollout))
        .route("/v1/rollouts/:rid",     get(get_rollout))
        .route("/v1/rollouts/:rid/diff",get(diff_rollout_attempts))
}

// ── GET /v1/workspaces/:ws_id/trajectory ─────────────────────────────────────

async fn get_workspace_trajectory(
    State(st): State<AppState>,
    Path(ws_id): Path<Uuid>,
) -> impl IntoResponse {
    match st.tracer.get_trajectory_for_workspace(WorkspaceId(ws_id)).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None)      => (StatusCode::NOT_FOUND, Json(ApiError::new("TRAJECTORY_NOT_FOUND","no trajectory for this workspace",false))).into_response(),
        Err(e)        => internal_err(anyhow::anyhow!(e)).into_response(),
    }
}

// ── GET /v1/trajectories/:tid ─────────────────────────────────────────────────

async fn get_trajectory(
    State(st): State<AppState>,
    Path(tid): Path<Uuid>,
) -> impl IntoResponse {
    match st.tracer.get_trajectory(TrajectoryId(tid)).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None)      => (StatusCode::NOT_FOUND, Json(ApiError::new("TRAJECTORY_NOT_FOUND","not found",false))).into_response(),
        Err(e)        => internal_err(anyhow::anyhow!(e)).into_response(),
    }
}

// ── GET /v1/trajectories/:tid/events ─────────────────────────────────────────

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default = "default_after")]
    after: i64,
}
fn default_limit() -> i64 { 50 }
fn default_after() -> i64 { -1 }

async fn list_events(
    State(st): State<AppState>,
    Path(tid):  Path<Uuid>,
    Query(q):   Query<EventsQuery>,
    let limit = q.limit.clamp(1, 200);
    let after = q.after.max(-1);
    match st.tracer.list_events(TrajectoryId(tid), limit, after).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e)   => internal_err(anyhow::anyhow!(e)).into_response(),
    }
}

// ── GET /v1/trajectories/:tid/export ─────────────────────────────────────────
// Streams NDJSON. Each line is a TraceEventRow.

async fn export_trajectory(
    State(st): State<AppState>,
    Path(tid):  Path<Uuid>,
) -> impl IntoResponse {
    // Check trajectory exists first
    match st.tracer.get_trajectory(TrajectoryId(tid)).await {
        Ok(None) => return (StatusCode::NOT_FOUND, "trajectory not found").into_response(),
        Err(e)   => return internal_err(anyhow::anyhow!(e)).into_response(),
        Ok(Some(_)) => {}
    }

    // Stream NDJSON via a channel-backed Body
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);
    let tracer = std::sync::Arc::clone(&st.tracer);

    tokio::spawn(async move {
        let tid = TrajectoryId(tid);
        let mut after_seq: i64 = -1;
        loop {
            match tracer.list_events(tid, 200, after_seq).await {
                Err(_) => break,
                Ok(batch) => {
                    if batch.is_empty() { break; }
                    after_seq = batch.last().unwrap().step_seq;
                    for row in &batch {
                        if let Ok(mut line) = serde_json::to_vec(row) {
                            line.push(b'\n');
                            if tx.send(Ok(axum::body::Bytes::from(line))).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::TRANSFER_ENCODING, "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
        .into_response()
}

// ── POST /v1/trajectories/:tid/replay ────────────────────────────────────────

#[derive(Deserialize)]
struct ReplayRequest {
    from_step: i64,
    workspace_name: Option<String>,
}

async fn replay_trajectory(
    State(st): State<AppState>,
    Path(tid):  Path<Uuid>,
    Json(req):  Json<ReplayRequest>,
) -> impl IntoResponse {
    let tid = TrajectoryId(tid);

    // Find the nearest checkpoint before from_step
    let ckpt_id = match st.tracer.find_replay_checkpoint(tid, req.from_step).await {
        Ok(Some(id)) => id,
        Ok(None)     => return (StatusCode::UNPROCESSABLE_ENTITY, Json(ApiError::new(
            "NO_REPLAY_CHECKPOINT",
            "no checkpoint found at or before the requested step",
            false,
        ))).into_response(),
        Err(e) => return internal_err(anyhow::anyhow!(e)).into_response(),
    };

    // Fork from that checkpoint
    let ctrl = std::sync::Arc::clone(&st.controller);
    let restore_req = RestoreRequest {
        target: "new_workspace".into(),
        workspace_name: req.workspace_name
            .or_else(|| Some(format!("replay-{}-step{}", &tid.to_string()[..8], req.from_step))),
        post_restore: PostRestoreConfig { quarantine: true, identity_reseal: true },
        rollout_id: None,
    };

    match ctrl.fork_checkpoint(ckpt_id, restore_req).await {
        Ok((ws, op)) => (StatusCode::ACCEPTED, Json(serde_json::json!({
            "workspace_id":  ws.id,
            "operation_id":  op.id,
            "replayed_from_step": req.from_step,
            "source_checkpoint_id": ckpt_id,
        }))).into_response(),
        Err(e) => internal_err(e).into_response(),
    }
}

// ── POST /v1/rollouts ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateRolloutRequest {
    source_checkpoint_id: Uuid,
    reward_command:        String,
    fork_count:            u32,
    #[serde(default)]
    workspace_name_prefix: Option<String>,
}

#[derive(Serialize)]
struct CreateRolloutResponse {
    rollout_id:     RolloutId,
    workspace_ids:  Vec<WorkspaceId>,
    operation_ids:  Vec<OperationId>,
}

async fn create_rollout(
    State(st): State<AppState>,
    Json(req):  Json<CreateRolloutRequest>,
) -> impl IntoResponse {
    let ckpt_id = CheckpointId(req.source_checkpoint_id);
    let fork_count = req.fork_count.min(32) as i32; // hard cap at 32 forks

    // Create rollout row
    let rollout_id = match st.tracer.create_rollout(ckpt_id, &req.reward_command, fork_count).await {
        Ok(rid) => rid,
        Err(e)  => return internal_err(anyhow::anyhow!(e)).into_response(),
    };

    // Fork N workspaces in parallel
    let ctrl = std::sync::Arc::clone(&st.controller);
    let prefix = req.workspace_name_prefix.unwrap_or_else(|| format!("rollout-{}", &rollout_id.to_string()[..8]));

    let forks: Vec<_> = (0..fork_count)
        .map(|i| {
            let ctrl = std::sync::Arc::clone(&ctrl);
            let name = format!("{}-{}", prefix, i);
            let restore_req = RestoreRequest {
                target: "new_workspace".into(),
                workspace_name: Some(name),
                post_restore: PostRestoreConfig { quarantine: true, identity_reseal: true },
                rollout_id: Some(rollout_id),
            };
            async move { ctrl.fork_checkpoint(ckpt_id, restore_req).await }
        })
        .collect();

    let results = futures::future::join_all(forks).await;

    let mut workspace_ids  = Vec::new();
    let mut operation_ids  = Vec::new();
    let mut errors         = Vec::new();

    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok((ws, op)) => {
                workspace_ids.push(ws.id);
                operation_ids.push(op.id);
            }
            Err(e) => errors.push(format!("fork {i}: {e}")),
        }
    }

    if workspace_ids.is_empty() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError::new(
            "ALL_FORKS_FAILED",
            &format!("all forks failed: {:?}", errors),
            true,
        ))).into_response();
    }

    (StatusCode::ACCEPTED, Json(CreateRolloutResponse {
        rollout_id,
        workspace_ids,
        operation_ids,
    })).into_response()
}

// ── GET /v1/rollouts/:rid ─────────────────────────────────────────────────────

async fn get_rollout(
    State(st): State<AppState>,
    Path(rid):  Path<Uuid>,
) -> impl IntoResponse {
    match st.tracer.get_rollout(RolloutId(rid)).await {
        Ok(Some(summary)) => Json(summary).into_response(),
        Ok(None)          => (StatusCode::NOT_FOUND, Json(ApiError::new("ROLLOUT_NOT_FOUND","not found",false))).into_response(),
        Err(e)            => internal_err(anyhow::anyhow!(e)).into_response(),
    }
}

// ── GET /v1/rollouts/:rid/diff ────────────────────────────────────────────────

#[derive(Deserialize)]
struct DiffQuery {
    a: Uuid,
    b: Uuid,
}

async fn diff_rollout_attempts(
    State(st): State<AppState>,
    Path(rid): Path<Uuid>,
    Query(q):   Query<DiffQuery>,
) -> impl IntoResponse {
    let rid = RolloutId(rid);
    let a = TrajectoryId(q.a);
    let b = TrajectoryId(q.b);

    let ta = match st.tracer.get_trajectory(a).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(ApiError::new(
                "TRAJECTORY_NOT_FOUND",
                "trajectory a not found",
                false,
            )))
                .into_response();
        }
        Err(e) => return internal_err(anyhow::anyhow!(e)).into_response(),
    };

    let tb = match st.tracer.get_trajectory(b).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(ApiError::new(
                "TRAJECTORY_NOT_FOUND",
                "trajectory b not found",
                false,
            )))
                .into_response();
        }
        Err(e) => return internal_err(anyhow::anyhow!(e)).into_response(),
    };

    if ta.rollout_id != Some(rid) || tb.rollout_id != Some(rid) {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(ApiError::new(
            "TRAJECTORY_NOT_IN_ROLLOUT",
            "both trajectories must belong to the rollout in the URL",
            false,
        )))
            .into_response();
    }

    match st.tracer.diff_trajectories(a, b).await {
        Ok(diff) => Json(diff).into_response(),
        Err(e)   => internal_err(anyhow::anyhow!(e)).into_response(),
    }
}
