// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use looklab_schema::Body;
use serde::Serialize;

use crate::body_context::BodyContextError;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::rate_limit::RateKey;
use crate::state::AppState;

#[derive(Serialize)]
struct BodyContext {
    bodies: Vec<Body>,
}

impl From<BodyContextError> for ApiError {
    fn from(_error: BodyContextError) -> Self {
        Self::new(ErrorCode::Internal)
    }
}

pub async fn registered(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    if !state.rate_limiter.check(&RateKey::PublicRead) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let active = crate::store::active_snapshot_identity(&state.db).await?;
    let bodies = crate::body_context::registered_without_schema(
        &state.config.bodies_dir,
        &state.config.snapshot_dir,
        active.map(|(version, _)| version),
    )?;
    Ok(Json(BodyContext { bodies }).into_response())
}
