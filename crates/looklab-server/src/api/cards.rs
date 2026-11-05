// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use looklab_upload::LookSlug;
use serde::Serialize;
use sqlx::Row;

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::rate_limit::RateKey;
use crate::state::AppState;

#[derive(Debug, Serialize)]
struct CardVerification {
    firmware_id: looklab_upload::FirmwareId,
    firmware_version: looklab_upload::FirmwareVersion,
    look: LookSlug,
    look_version: String,
    images: u64,
    median_de: f64,
    p95_de: f64,
}

#[derive(Debug, Serialize)]
struct VerificationList {
    verifications: Vec<CardVerification>,
}

/// `GET /api/cards/verification`: per-firmware card-render verification.
pub async fn verifications(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    if !state.rate_limiter.check(&RateKey::PublicRead) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let rows = sqlx::query(
        "SELECT firmware_id, firmware_version, look_slug, look_version,
                images, median_de, p95_de
         FROM card_verifications
         ORDER BY firmware_id, firmware_version, look_slug, look_version",
    )
    .fetch_all(&state.db)
    .await?;
    let mut verifications = Vec::with_capacity(rows.len());
    for row in &rows {
        let images: i64 = row.get("images");
        let firmware_id: String = row.get("firmware_id");
        let firmware_version: String = row.get("firmware_version");
        let look: String = row.get("look_slug");
        verifications.push(CardVerification {
            firmware_id: looklab_upload::FirmwareId::new(&firmware_id)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            firmware_version: looklab_upload::FirmwareVersion::new(&firmware_version)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            look: LookSlug::try_from(look).map_err(|_| ApiError::new(ErrorCode::Internal))?,
            look_version: row.get("look_version"),
            images: u64::try_from(images).unwrap_or(0),
            median_de: row.get("median_de"),
            p95_de: row.get("p95_de"),
        });
    }
    Ok(Json(VerificationList { verifications }).into_response())
}
