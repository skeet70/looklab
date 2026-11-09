// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::client_errors::{self, ClientErrorReport, MAX_REPORT_BYTES};
use crate::db;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::rate_limit::RateKey;
use crate::state::AppState;

/// `POST /api/client-errors`: accept one bounded diagnostic report.
pub async fn report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult<Response> {
    if !accepts_json(&headers) {
        return Err(ApiError::new(ErrorCode::UnsupportedMediaType));
    }
    if declared_length(&headers).is_some_and(|length| length > MAX_REPORT_BYTES) {
        return Err(ApiError::new(ErrorCode::PayloadTooLarge));
    }
    if !state.rate_limiter.check(&RateKey::ClientError) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let received_at = db::now_unix();
    let body = tokio::time::timeout(
        state.config.body_timeout,
        axum::body::to_bytes(request.into_body(), MAX_REPORT_BYTES),
    )
    .await
    .map_err(|_| ApiError::new(ErrorCode::RequestTimeout))?
    .map_err(|_| ApiError::new(ErrorCode::PayloadTooLarge))?;
    let report =
        ClientErrorReport::parse(&body).map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    client_errors::insert(&state.db, &report, received_at)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

fn declared_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}
