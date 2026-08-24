//! Liveness probe.

use axum::Json;
use serde_json::{Value, json};

/// `GET /health` — unauthenticated on purpose: an orchestrator probing
/// liveness holds no credentials, and a probe that can fail for auth reasons
/// reports the wrong thing.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
