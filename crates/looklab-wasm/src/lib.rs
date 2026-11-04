// SPDX-License-Identifier: AGPL-3.0-or-later
//! wasm-bindgen facade for the browser client.
//!
//! Requests to the prep module are built here as postcard bytes; the worker
//! JavaScript only copies them.

pub mod coverage;
pub mod dto;
pub mod error;
pub mod match_api;
pub mod pairing;
pub mod pattern;
pub mod prep;
pub mod seedfile;

#[cfg(feature = "ui")]
mod ui;

#[cfg(all(target_arch = "wasm32", feature = "worker"))]
pub mod worker_abi;

pub use dto::{MatchReply, VerifyReply};
pub use error::FacadeError;
pub use looklab_prep_module::{
    AlignmentSummary, AssertionFields, PowParamsSpec, PreparedImageView,
};
pub use match_api::{
    MatchInputs, MatchRequest, VerifyRequestMessage, decode_match_reply, decode_verify_reply,
    run_match, run_verify,
};
pub use match_api::{encode_match_request, encode_verify_request};
pub use pairing::{PairGroup, PairingPlan, UnpairedEntry, inspect_files};
pub use pattern::{PatternImage, render_pattern};
pub use prep::{
    GroupSettings, IntentChoice, PrepSummary, build_manifest_request, decode_build_manifest_reply,
    decode_encode_reply, decode_parse_reply, decode_pow_reply, decode_prepare_reply,
    encode_envelope_request, module_fn_id, parse_envelope_request, prepare_request, serial_ascii,
    serial_ascii_for_file, solve_pow_request, version_request,
};
pub use seedfile::build_seed_file;

pub const MODULE_ABI_VERSION: u32 = looklab_prep_module::MODULE_ABI_VERSION;

pub const WORKER_FN_VERSION: u32 = 1;
pub const WORKER_FN_MATCH: u32 = 2;
pub const WORKER_FN_VERIFY: u32 = 3;

// docs/01 "Worker recycling".
pub const RECYCLE_AFTER_FILES: u64 = 20;
pub const WORKER_BUDGET_BYTES: u64 = 1 << 30;
pub const RECYCLE_MEMORY_FRACTION: f64 = 0.75;
