// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_prep_module::ModuleError;

#[derive(Debug, thiserror::Error)]
pub enum FacadeError {
    #[error("{0}")]
    Prep(#[from] looklab_prep::PrepError),
    #[error("metadata: {0}")]
    Meta(#[from] looklab_meta::MetaError),
    #[error("model: {0}")]
    Model(#[from] looklab_model::ModelError),
    #[error("match: {0}")]
    Match(#[from] looklab_match::MatchError),
    #[error("schema: {0:?}")]
    Schema(Vec<looklab_schema::ValidationError>),
    #[error("{0}")]
    Id(String),
    #[error("settings: {0}")]
    Settings(#[from] looklab_upload::SettingsError),
    #[error("the prep module failed: {0}")]
    Module(ModuleError),
    #[error(
        "prep module identity does not match this build \
         (module ABI {found_abi}, prep {found_prep}; \
          expected ABI {expected_abi}, prep {expected_prep})"
    )]
    ModuleVersion {
        found_abi: u32,
        found_prep: u32,
        expected_abi: u32,
        expected_prep: u32,
    },
    #[error("the prep module reply is malformed: {0}")]
    Reply(String),
    #[error("{0}")]
    Input(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
