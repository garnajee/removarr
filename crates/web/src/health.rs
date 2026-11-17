//! Liveness and readiness (architecture 8.5 item 5, budget B9). Neither probe scans, touches a
//! root or wakes a disk: `/healthz` touches nothing at all, `/readyz` runs one cheap query on the
//! database (config SSD) with a short timeout. Bodies carry no detail.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Readiness gives up after this long (the database is local; a slow answer is not ready).
pub const READY_TIMEOUT: Duration = Duration::from_secs(2);

fn plain(status: StatusCode, body: &'static str) -> Response {
    let mut r = (status, body).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `GET /healthz`: the process answers HTTP.
pub async fn healthz() -> Response {
    plain(StatusCode::OK, "ok")
}

/// `GET /readyz`: configuration loaded (the server only starts with a valid one) and the
/// database answers.
pub async fn readyz(State(s): State<AppState>) -> Response {
    match tokio::time::timeout(READY_TIMEOUT, s.readiness.ready()).await {
        Ok(Ok(())) => plain(StatusCode::OK, "ready"),
        Ok(Err(e)) => {
            tracing::warn!(class = ?e.class, "readiness check failed");
            plain(StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
        Err(_) => {
            tracing::warn!("readiness check timed out");
            plain(StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }
}
