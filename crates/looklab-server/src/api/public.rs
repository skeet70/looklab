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
use crate::ids::{EndorsementId, GrantId};
use crate::rate_limit::RateKey;
use crate::state::AppState;

fn public_read(state: &AppState) -> ApiResult<()> {
    if state.rate_limiter.check(&RateKey::PublicRead) {
        Ok(())
    } else {
        Err(ApiError::new(ErrorCode::RateLimited))
    }
}

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
    public_read(&state)?;
    let query = parse_audit_query(&uri)?;
    let page: AuditLogPage = admin::audit_log(&state.db, query.after, query.limit).await?;
    Ok(Json(page).into_response())
}

#[derive(Debug, Default)]
pub struct ListQuery {
    after: Option<String>,
    limit: i64,
}

fn parse_list_query(uri: &Uri) -> ApiResult<ListQuery> {
    let mut query = ListQuery {
        after: None,
        limit: 50,
    };
    let Some(raw) = uri.query() else {
        return Ok(query);
    };
    for pair in raw.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| ApiError::new(ErrorCode::MalformedEnvelope))?;
        match key {
            "after" if !value.is_empty() => query.after = Some(value.to_owned()),
            "limit" => {
                query.limit = value
                    .parse()
                    .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
            }
            _ => return Err(ApiError::new(ErrorCode::MalformedEnvelope)),
        }
    }
    Ok(query)
}

#[derive(Serialize)]
pub struct GrantList {
    grants: Vec<GrantRecord>,
    next_after: Option<GrantId>,
}

pub async fn grants(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    public_read(&state)?;
    let query = parse_list_query(&uri)?;
    let after = query
        .after
        .as_deref()
        .map(GrantId::try_from)
        .transpose()
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let page = admin::list_grants(&state.db, after.as_ref(), query.limit).await?;
    Ok(Json(GrantList {
        grants: page.grants,
        next_after: page.next_after,
    })
    .into_response())
}

#[derive(Serialize)]
pub struct EndorsementList {
    endorsements: Vec<EndorsementRecord>,
    next_after: Option<EndorsementId>,
}

pub async fn endorsements(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    public_read(&state)?;
    let query = parse_list_query(&uri)?;
    let after = query
        .after
        .as_deref()
        .map(EndorsementId::try_from)
        .transpose()
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let page = admin::list_endorsements(&state.db, after.as_ref(), query.limit).await?;
    Ok(Json(EndorsementList {
        endorsements: page.endorsements,
        next_after: page.next_after,
    })
    .into_response())
}

pub async fn manifest(
    State(state): State<Arc<AppState>>,
    Path((look, version)): Path<(String, String)>,
) -> ApiResult<Response> {
    public_read(&state)?;
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
