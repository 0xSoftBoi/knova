//! Liveness probe.

use axum::Json;
use serde_json::{Value, json};

/// `GET /health` — deliberately outside the internal-token layer.
///
/// An orchestrator probing liveness is not the gateway and holds no shared
/// secret. A probe that can fail for authorization reasons reports the wrong
/// thing, and would take a healthy pod out of service on a token rotation.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
