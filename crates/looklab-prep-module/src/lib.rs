// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reproducible preparation module: plain C ABI, no imports, no randomness.
//! [`abi::ll_call`] takes `postcard(Result<Response, ModuleError>)`.

pub mod abi;
mod error;
mod ops;
mod types;

pub use error::ModuleError;
pub use looklab_prep::PREP_VERSION;
pub use types::{
    AlignmentSummary, AssertionFields, BlobSummary, BuildManifestRequest, BuildManifestResponse,
    BuildSendManifestRequest, BuildSendManifestResponse, EncodeEnvelopeRequest,
    EncodeEnvelopeResponse, ManifestImageInput, ParseEnvelopeRequest, ParseEnvelopeResponse,
    PowParamsSpec, PrepareRequest, PrepareResponse, PreparedImageView, PriorTransformSpec,
    Response, SolvePowRequest, SolvePowResponse, VersionRequest, VersionResponse,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MODULE_ABI_VERSION: u32 = 1;

const BUILD_ID_TABLE: &[u8] = concat!(
    "looklab-prep-module/",
    env!("CARGO_PKG_VERSION"),
    "\0ll_alloc\0ll_free\0ll_call\0",
    "version\0prepare\0build_manifest\0build_send_manifest\0encode_envelope\0solve_pow\0parse_envelope\0",
    "abi=1\0prep=2",
)
.as_bytes();

pub fn build_id() -> [u8; 32] {
    let digest = Sha256::digest(BUILD_ID_TABLE);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum Function {
    Version = 1,
    Prepare = 2,
    BuildManifest = 3,
    EncodeEnvelope = 4,
    SolvePow = 5,
    ParseEnvelope = 6,
    BuildSendManifest = 7,
}

impl Function {
    pub const fn id(self) -> u32 {
        self as u32
    }

    pub const fn from_id(fn_id: u32) -> Option<Self> {
        Some(match fn_id {
            1 => Self::Version,
            2 => Self::Prepare,
            3 => Self::BuildManifest,
            4 => Self::EncodeEnvelope,
            5 => Self::SolvePow,
            6 => Self::ParseEnvelope,
            7 => Self::BuildSendManifest,
            _ => return None,
        })
    }
}

pub fn version() -> VersionResponse {
    VersionResponse {
        module_abi_version: MODULE_ABI_VERSION,
        prep_version: PREP_VERSION,
        crate_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_id: build_id(),
    }
}

/// Runs one function on a postcard request and returns the postcard result.
pub fn dispatch(fn_id: u32, input: &[u8]) -> Vec<u8> {
    let result = match Function::from_id(fn_id) {
        None => Err(ModuleError::UnknownFunction { fn_id }),
        Some(Function::Version) => request::<VersionRequest>(Function::Version, input)
            .map(|_| Response::Version(version())),
        Some(Function::Prepare) => request::<ops::PrepareInput<'_>>(Function::Prepare, input)
            .and_then(|request| ops::prepare(request).map(Response::Prepare)),
        Some(Function::BuildManifest) => {
            request::<BuildManifestRequest>(Function::BuildManifest, input)
                .and_then(|request| ops::build_manifest(request).map(Response::BuildManifest))
        }
        Some(Function::BuildSendManifest) => {
            request::<BuildSendManifestRequest>(Function::BuildSendManifest, input).and_then(
                |request| ops::build_send_manifest(request).map(Response::BuildSendManifest),
            )
        }
        Some(Function::EncodeEnvelope) => {
            request::<EncodeEnvelopeRequest>(Function::EncodeEnvelope, input)
                .and_then(|request| ops::encode_envelope(request).map(Response::EncodeEnvelope))
        }
        Some(Function::SolvePow) => request::<SolvePowRequest>(Function::SolvePow, input)
            .and_then(|request| ops::solve_pow(request).map(Response::SolvePow)),
        Some(Function::ParseEnvelope) => {
            request::<ParseEnvelopeRequest>(Function::ParseEnvelope, input).and_then(|request| {
                ops::parse_envelope(request)
                    .map(|response| Response::ParseEnvelope(Box::new(response)))
            })
        }
    };
    // A postcard encoding of these fixed structs has no failure mode.
    postcard::to_allocvec(&result).expect("module responses always encode")
}

fn request<'a, T: Deserialize<'a>>(function: Function, input: &'a [u8]) -> Result<T, ModuleError> {
    let (request, rest) =
        postcard::take_from_bytes(input).map_err(|error| ModuleError::Request {
            function,
            message: error.to_string(),
        })?;
    if !rest.is_empty() {
        return Err(ModuleError::Request {
            function,
            message: format!("{} trailing bytes", rest.len()),
        });
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_round_trips_through_dispatch() {
        let input = postcard::to_allocvec(&VersionRequest).expect("version request encodes");
        let output = dispatch(Function::Version.id(), &input);
        let response: Result<Response, ModuleError> =
            postcard::from_bytes(&output).expect("version response decodes");
        assert_eq!(
            response.expect("version succeeds"),
            Response::Version(version())
        );
    }

    #[test]
    fn unknown_function_is_an_error_response() {
        let output = dispatch(99, &[]);
        let response: Result<Response, ModuleError> =
            postcard::from_bytes(&output).expect("error response decodes");
        assert!(matches!(
            response,
            Err(ModuleError::UnknownFunction { fn_id: 99 })
        ));
    }

    #[test]
    fn build_id_ignores_the_host() {
        assert_eq!(build_id(), build_id());
        assert_ne!(build_id(), [0u8; 32]);
    }
}
