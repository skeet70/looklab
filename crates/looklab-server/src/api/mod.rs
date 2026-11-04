// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod passkey;
pub mod status;
pub mod upload;
pub mod withdraw;

use looklab_upload::Nonce;

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::state::AppState;
use crate::store::{self, ConsumeNonce};

pub(crate) async fn consume_nonce(state: &AppState, nonce: &Nonce) -> ApiResult<()> {
    match store::consume_nonce(&state.db, nonce).await? {
        ConsumeNonce::Ok => Ok(()),
        ConsumeNonce::Unknown => Err(ApiError::new(ErrorCode::NonceUnknown)),
        ConsumeNonce::Expired => Err(ApiError::new(ErrorCode::NonceExpired)),
        ConsumeNonce::Replayed => Err(ApiError::new(ErrorCode::NonceReplayed)),
    }
}
