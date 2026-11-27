use std::{collections::BTreeMap, sync::Arc};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{
    errors::{Result, SnapBoxError},
    metrics::{LatencyStats, OperationKind},
    pool::{PoolStatus, WarmPoolManager},
    sandbox::{CommandRequest, CommandResult, SandboxId, SandboxRecord},
};

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<WarmPoolManager>,
    pub shutdown_tx: Option<watch::Sender<bool>>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub backend: String,
    pub warm_sandboxes: usize,
    pub total_sandboxes: usize,
}

#[derive(Debug, Serialize)]
pub struct MetricsResponse {
    pub uptime_secs: u64,
    pub operations: BTreeMap<OperationKind, LatencyStats>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AllocateRequest {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SandboxDetailResponse {
    pub sandbox: SandboxRecord,
    pub workspace_apparent_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct ShutdownResponse {
    pub status: &'static str,
}

/// Builds the Axum 0.8 REST API router for snap-box.
pub fn build_router(
    pool: Arc<WarmPoolManager>,
    shutdown_tx: Option<watch::Sender<bool>>,
) -> Router {
    let state = AppState { pool, shutdown_tx };
    Router::new()
        .route("/health", get(health_handler))
        .route("/api/v1/pool", get(pool_status_handler))
        .route("/api/v1/metrics", get(metrics_handler))
        .route("/api/v1/daemon/shutdown", post(shutdown_handler))
        .route("/api/v1/sandboxes", get(list_sandboxes_handler))
        .route("/api/v1/sandboxes/allocate", post(allocate_sandbox_handler))
        .route(
            "/api/v1/sandboxes/{id}",
            get(get_sandbox_handler).delete(delete_sandbox_handler),
        )
        .route("/api/v1/sandboxes/{id}/exec", post(exec_sandbox_handler))
        .route(
            "/api/v1/sandboxes/{id}/release",
            post(release_sandbox_handler),
        )
        .route("/api/v1/sandboxes/{id}/reset", post(reset_sandbox_handler))
        .with_state(state)
}

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let status = state.pool.status();
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.pool.metrics().uptime_secs(),
        backend: status.backend.backend_name,
        warm_sandboxes: status.warm_count,
        total_sandboxes: status.total_sandboxes,
    })
}

async fn pool_status_handler(State(state): State<AppState>) -> Json<PoolStatus> {
    Json(state.pool.status())
}

async fn metrics_handler(State(state): State<AppState>) -> Json<MetricsResponse> {
    Json(MetricsResponse {
        uptime_secs: state.pool.metrics().uptime_secs(),
        operations: state.pool.metrics().snapshot(),
    })
}

async fn list_sandboxes_handler(State(state): State<AppState>) -> Json<Vec<SandboxRecord>> {
    Json(state.pool.list())
}

async fn allocate_sandbox_handler(
    State(state): State<AppState>,
    payload: Option<Json<AllocateRequest>>,
) -> Result<(StatusCode, Json<SandboxRecord>)> {
    let req = payload.map(|Json(v)| v).unwrap_or_default();
    if let Some(ttl) = req.ttl_secs {
        if ttl == 0 || ttl > 86_400 {
            return Err(SnapBoxError::Validation(
                "ttl_secs must be between 1 and 86400".into(),
            ));
        }
    }
    let rec = state.pool.allocate(req.owner, req.ttl_secs).await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

async fn get_sandbox_handler(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<Json<SandboxDetailResponse>> {
    let id = parse_sandbox_id(&id_str)?;
    let sandbox = state.pool.get(id)?;
    let workspace_apparent_bytes = state.pool.workspace_disk_usage(id).unwrap_or(0);
    Ok(Json(SandboxDetailResponse {
        sandbox,
        workspace_apparent_bytes,
    }))
}

async fn exec_sandbox_handler(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(req): Json<CommandRequest>,
) -> Result<Json<CommandResult>> {
    let id = parse_sandbox_id(&id_str)?;
    let res = state.pool.execute(id, req).await?;
    Ok(Json(res))
}

async fn release_sandbox_handler(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<Json<SandboxRecord>> {
    let id = parse_sandbox_id(&id_str)?;
    let rec = state.pool.release(id).await?;
    Ok(Json(rec))
}

async fn reset_sandbox_handler(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<Json<SandboxRecord>> {
    let id = parse_sandbox_id(&id_str)?;
    let rec = state.pool.reset_in_place(id).await?;
    Ok(Json(rec))
}

async fn delete_sandbox_handler(
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<Json<SandboxRecord>> {
    let id = parse_sandbox_id(&id_str)?;
    let rec = state.pool.destroy(id).await?;
    Ok(Json(rec))
}

async fn shutdown_handler(State(state): State<AppState>) -> Result<Json<ShutdownResponse>> {
    if let Some(tx) = &state.shutdown_tx {
        let _ = tx.send(true);
        Ok(Json(ShutdownResponse {
            status: "shutting_down",
        }))
    } else {
        Err(SnapBoxError::Daemon(
            "shutdown control unavailable in embedded router".into(),
        ))
    }
}

fn parse_sandbox_id(raw: &str) -> Result<SandboxId> {
    raw.parse::<SandboxId>()
        .map_err(|_| SnapBoxError::Validation(format!("invalid sandbox UUID '{raw}'")))
}
