#![allow(dead_code, unused_imports)]
mod attach;
mod routes;

use std::sync::Arc;
use anyhow::Result;
use axum::{http::StatusCode, response::{IntoResponse, Json}, Router};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::info;

use arbor_common::ApiError;
use arbor_controller::{Controller, ControllerConfig, Db, GrantRegistry, Scheduler, SnapshotService};
use arbor_egress_proxy::ProxyState;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiConfig {
    #[serde(default = "default_bind")]         bind:                 String,
    database_url: String,
    #[serde(default = "default_kernel")]       kernel_path:          String,
    #[serde(default = "default_runner_class")] default_runner_class: String,
    #[serde(default = "default_secret")]       attach_token_secret:  String,
    #[serde(default = "default_base_url")]     api_base_url:         String,
    #[serde(default = "default_images_dir")]   base_images_dir:      String,
    #[serde(default = "default_obj_store")]    object_store_url:     String,
    #[serde(default = "default_obj_prefix")]   object_store_prefix:  String,
    #[serde(default = "default_proxy_bind")]   proxy_bind:           String,
}
fn default_bind()         -> String { "0.0.0.0:8080".into() }
fn default_kernel()       -> String { "/var/lib/arbor/firecracker/vmlinux".into() }
fn default_runner_class() -> String { "fc-x86_64-v1".into() }
fn default_secret()       -> String { "change-me-in-production".into() }
fn default_base_url()     -> String { "http://localhost:8080".into() }
fn default_images_dir()   -> String { "/var/lib/arbor/images".into() }
fn default_obj_store()    -> String { "local:///var/lib/arbor/snapshots/store".into() }
fn default_obj_prefix()   -> String { "checkpoints".into() }
fn default_proxy_bind()   -> String { "0.0.0.0:3128".into() }

// ── AppState ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub controller: Arc<Controller>,
    pub proxy_state: ProxyState,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arbor_api=info,tower_http=info".into()),
        )
        .json()
        .init();

    let cfg: ApiConfig = config::Config::builder()
        .add_source(config::Environment::with_prefix("ARBOR").separator("__"))
        .build()?
        .try_deserialize()?;

    info!(bind = %cfg.bind, "arbor-api starting");

    // ── Database ──────────────────────────────────────────────────────────────
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&cfg.database_url)
        .await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;
    info!("migrations applied");

    // ── Prometheus metrics ────────────────────────────────────────────────────
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()?;

    // ── Shared grant registry (broker ↔ proxy) ────────────────────────────────
    let grants = Arc::new(GrantRegistry::new());

    // ── Egress proxy (runs as background task on :3128) ───────────────────────
    let proxy_state = ProxyState::new(Arc::clone(&grants));
    {
        let ps   = proxy_state.clone();
        let bind = cfg.proxy_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = arbor_egress_proxy::run_proxy(&bind, ps).await {
                tracing::error!(?e, "egress proxy failed");
            }
        });
        info!(bind = %cfg.proxy_bind, "egress proxy started");
    }

    // ── Controller ────────────────────────────────────────────────────────────
    let db        = Arc::new(Db::new(pool));
    let scheduler = Arc::new(Scheduler::new(Arc::clone(&db)));

    // Snapshot service — try S3 first, fall back to local
    // MVP: local filesystem snapshot store. Set ARBOR__OBJECT_STORE_URL=local:///path for dev.
    // For S3: rebuild with --features s3 and set AWS_* env vars.
    let snap_base = if cfg.object_store_url.starts_with("local://") {
        cfg.object_store_url.trim_start_matches("local://").to_string()
    } else {
        "/var/lib/arbor/snapshots/store".to_string()
    };
    let snapshot = Arc::new(SnapshotService::new_local(&snap_base)?);

    let ctrl_cfg = Arc::new(ControllerConfig {
        base_images_dir:     cfg.base_images_dir,
        kernel_path:         cfg.kernel_path,
        default_runner_class: cfg.default_runner_class,
        attach_token_secret: cfg.attach_token_secret,
        api_base_url:        cfg.api_base_url.clone(),
        object_store_prefix: cfg.object_store_prefix,
    });

    let controller = Arc::new(Controller::new(
        Arc::clone(&db), scheduler, ctrl_cfg, snapshot, Arc::clone(&grants),
    ));

    // ── Runner health sweep (M6) — mark stale runners unhealthy ───────────────
    {
        let db_clone = Arc::clone(&db);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                runner_health_sweep(&db_clone).await;
            }
        });
    }

    let state = AppState { controller, proxy_state };

    // ── Router ────────────────────────────────────────────────────────────────
    let app = Router::new()
        .merge(routes::workspaces::router())
        .merge(routes::sessions::router())
        .merge(routes::runners::router())
        .merge(attach::router())
        .route("/health",  axum::routing::get(health))
        .route("/metrics", axum::routing::get(move || async move {
            prometheus_handle.render()
        }))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(30)))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    info!("listening on {}", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Health + metrics ──────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "arbor-api" }))
}

/// Mark runners that haven't sent a heartbeat in >60s as unhealthy.
async fn runner_health_sweep(db: &Db) {
    let rows = sqlx::query(
        "UPDATE runner_nodes SET healthy = false
         WHERE healthy = true AND last_heartbeat < now() - interval '60 seconds'
         RETURNING id"
    )
    .fetch_all(db.pool())
    .await;

    match rows {
        Ok(r) if !r.is_empty() => {
            tracing::warn!("marked {} runner(s) unhealthy (missed heartbeat)", r.len());
            metrics::counter!("arbor.runner.unhealthy").increment(r.len() as u64);
        }
        _ => {}
    }
}

// ── Error helpers (used by route handlers) ────────────────────────────────────

pub fn arbor_err(e: arbor_common::ArborError) -> impl IntoResponse {
    let status = StatusCode::from_u16(e.http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ApiError::new(e.code(), &e.to_string(), e.retryable())))
}

pub fn internal_err(e: anyhow::Error) -> impl IntoResponse {
    tracing::error!(?e, "unhandled error");
    (StatusCode::INTERNAL_SERVER_ERROR,
     Json(ApiError::new("INTERNAL_ERROR", "an internal error occurred", true)))
}
