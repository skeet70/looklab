// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use looklab_trust::VersionId;
use looklab_upload::LookSlug;
use serde::Serialize;

use crate::admin::{self, AuditLogPage, EndorsementRecord, GrantRecord, ManifestRecord};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::state::AppState;

#[derive(Debug, Default)]
pub struct AuditQuery {
    after: i64,
    limit: i64,
}

fn parse_audit_query(uri: &Uri) -> ApiResult<AuditQuery> {
    let mut query = AuditQuery {
        after: 0,
        limit: 100,
    };
    let Some(raw) = uri.query() else {
        return Ok(query);
    };
    for pair in raw.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| ApiError::new(ErrorCode::MalformedEnvelope))?;
        let value: i64 = value
            .parse()
            .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
        match key {
            "after" => query.after = value.max(0),
            "limit" => query.limit = value,
            _ => return Err(ApiError::new(ErrorCode::MalformedEnvelope)),
        }
    }
    Ok(query)
}

pub async fn audit_log(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    let query = parse_audit_query(&uri)?;
    let page: AuditLogPage = admin::audit_log(&state.db, query.after, query.limit).await?;
    Ok(Json(page).into_response())
}

#[derive(Serialize)]
pub struct GrantList {
    grants: Vec<GrantRecord>,
}

pub async fn grants(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    Ok(Json(GrantList {
        grants: admin::list_grants(&state.db).await?,
    })
    .into_response())
}

#[derive(Serialize)]
pub struct EndorsementList {
    endorsements: Vec<EndorsementRecord>,
}

pub async fn endorsements(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    Ok(Json(EndorsementList {
        endorsements: admin::list_endorsements(&state.db).await?,
    })
    .into_response())
}

pub async fn manifest(
    State(state): State<Arc<AppState>>,
    Path((look, version)): Path<(String, String)>,
) -> ApiResult<Response> {
    let look = LookSlug::try_from(look).map_err(|_| ApiError::new(ErrorCode::NotFound))?;
    let version = version
        .strip_suffix(".json")
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound))?;
    let version = version
        .parse::<u32>()
        .map_err(|_| ApiError::new(ErrorCode::NotFound))?;
    let record: ManifestRecord =
        admin::manifest_record(&state.db, &look, VersionId::new(version)).await?;
    Ok(Json(record).into_response())
}
