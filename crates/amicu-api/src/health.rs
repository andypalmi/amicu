//! Unauthenticated liveness/readiness probes for the Cloud Run contract
//!
//! `GET /healthz` never touches a dependency: a `200` means only that the
//! process is up and serving. `GET /readyz` additionally proves the shared
//! Postgres pool (when one is configured) answers a trivial query, so Cloud
//! Run / the Google LB pull a replica out of rotation the moment its database
//! path is unusable, instead of routing live traffic at it. Neither route is
//! authenticated or rate-limited - a load balancer's health checker carries no
//! bearer token and must not be throttled - and they are deliberately kept out
//! of [`crate::api_openapi`] / the committed `openapi.json` artifact: they are
//! infra plumbing, not part of the client-facing API surface, so wiring them
//! in never forces a web codegen regen.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use amicu_store::PgPool;

/// Health/readiness state: the shared Postgres pool, or `None` on the
/// dev/single-node in-memory path (see the amicu-server crate docs).
#[derive(Clone)]
pub struct HealthApiState {
    pool: Option<PgPool>,
    /// Upper bound `GET /readyz` waits for [`amicu_store::health_check`]
    /// before reporting `503`, independent of the pool's own acquire
    /// timeout - see [`readyz`]'s docs.
    readyz_timeout: Duration,
}

impl HealthApiState {
    /// `pool = Some(..)` on the Postgres path (readiness probes it with
    /// [`amicu_store::health_check`], bounded by `readyz_timeout`); `None` on
    /// the in-memory dev path, where readiness is trivially `200` (there is
    /// no dependency to probe, so `readyz_timeout` is unused).
    #[must_use]
    pub fn new(pool: Option<PgPool>, readyz_timeout: Duration) -> Self {
        Self {
            pool,
            readyz_timeout,
        }
    }
}

/// `GET /healthz` response body.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// `GET /readyz` response body.
#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    /// `"postgres"` when readiness probed a live pool, `"in-memory"` on the
    /// dev/single-node path (no dependency to probe).
    database: &'static str,
}

/// Liveness probe: always `200`, no dependency check. Proves only that the
/// process accepted the connection and is running request handlers at all.
async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Readiness probe: `200` once the Postgres pool answers `SELECT 1` (or
/// trivially, on the in-memory dev path, where there is nothing to probe);
/// `503` if a configured pool does not answer, OR does not answer within
/// `state.readyz_timeout` - a short, dedicated bound (default 2s, see
/// `amicu-server`'s `AMICU_READYZ_TIMEOUT_MS`) independent of the pool's own
/// (typically much longer) connection-acquire timeout, so a saturated pool
/// fails the probe fast instead of leaving the load balancer's health check
/// hanging.
async fn readyz(State(state): State<HealthApiState>) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return (
            StatusCode::OK,
            Json(ReadyResponse {
                status: "ok",
                database: "in-memory",
            }),
        )
            .into_response();
    };

    match tokio::time::timeout(state.readyz_timeout, amicu_store::health_check(pool)).await {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(ReadyResponse {
                status: "ok",
                database: "postgres",
            }),
        )
            .into_response(),
        // Either the query itself failed, or it didn't finish within
        // `readyz_timeout` (the `Err` from `timeout`, discarded) - both mean
        // "not ready", so both map to the same 503 body.
        Ok(Err(_)) | Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyResponse {
                status: "error",
                database: "postgres",
            }),
        )
            .into_response(),
    }
}

/// Build the health/readiness router: unauthenticated, unthrottled, no body
/// limit needed (both routes are bodyless `GET`s). Merge this onto
/// [`crate::api_router`]'s result.
pub fn health_router(state: HealthApiState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}
