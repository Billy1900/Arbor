#![allow(dead_code, unused_variables, unused_imports)]
mod attach;
mod routes;

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    Router,
};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::info;

use arbor_common::ApiError;
use arbor_controller::{Controller, ControllerConfig, Db, Scheduler};

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiConfig {
    #[serde(default = "default_bind")]
    bind: String,
    database_url: String,
    #[serde(default = "default_kernel")]
    kernel_path: String,
    #[serde(default = "default_runner_class")]
    default_runner_class: String,
    #[serde(default = "default_attach_secret")]
    attach_token_secret: String,
    #[serde(default = "default_base_url")]
    api_base_url: String,
    #[serde(default = "default_images_dir")]
    base_images_dir: String,
}

fn default_bind() -> String { "0.0.0.0:8080".into() }
fn default_kernel() -> String { "/var/lib/arbor/firecracker/vmlinux".into() }
fn default_runner_class() -> String { "fc-x86_64-v1".into() }
fn default_attach_secret() -> String { "change-me-in-production".into() }
fn default_base_url() -> String { "http://localhost:8080".into() }
fn default_images_dir() -> String { "/var/lib/arbor/images".into() }

// ── App state ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub controller: Arc<Controller>,
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arbor_api=info,tower_http=debug".into()),
        )
        .json()
        .init();

    let cfg: ApiConfig = config::Config::builder()
        .add_source(config::Environment::with_prefix("ARBOR").separator("__"))
        .build()?
        .try_deserialize()?;

    info!(bind = %cfg.bind, "arbor-api starting");

    // DB pool
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&cfg.database_url)
        .await?;

    // Run migrations
    sqlx::migrate!("../../migrations").run(&pool).await?;
    info!("migrations applied");

    let db = Arc::new(Db::new(pool));
    let scheduler = Arc::new(Scheduler::new(Arc::clone(&db)));
    let ctrl_cfg = Arc::new(ControllerConfig {
        base_images_dir: cfg.base_images_dir,
        kernel_path: cfg.kernel_path,
        default_runner_class: cfg.default_runner_class,
        attach_token_secret: cfg.attach_token_secret,
        api_base_url: cfg.api_base_url,
    });
    let controller = Arc::new(Controller::new(db, scheduler, ctrl_cfg));
    let state = AppState { controller };

    let app = Router::new()
        .merge(routes::workspaces::router())
        .merge(routes::sessions::router())
        .merge(attach::router())
        .route("/health", axum::routing::get(health))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(30)))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    info!("listening on {}", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Health ───────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "arbor-api" }))
}

// ── Error conversion ─────────────────────────────────────────────────────────

pub fn arbor_err_response(e: arbor_common::ArborError) -> impl IntoResponse {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = ApiError::new(e.code(), &e.to_string(), e.retryable());
    (status, Json(body))
}

pub fn anyhow_err_response(e: anyhow::Error) -> impl IntoResponse {
    tracing::error!(?e, "unhandled error");
    let body = ApiError::new("INTERNAL_ERROR", "An internal error occurred", true);
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
}
