// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical proposal signing bytes, request bodies and LUT frames.

use looklab_schema::{FixtureDonation, ProposalKind};
use serde::Serialize;

use crate::error::FacadeError;

#[derive(Clone, Copy, Debug)]
pub struct ProposalSubmission<'a> {
    pub kind: ProposalKind,
    pub payload: &'a str,
    pub fixtures: &'a [FixtureDonation],
}

#[derive(Serialize)]
struct ProposalRequest<'a, S: Serialize> {
    kind: ProposalKind,
    payload: &'a str,
    fixtures: &'a [FixtureDonation],
    pow: &'a [u64],
    #[serde(flatten)]
    signed: &'a S,
}

/// # Errors
/// The signing body cannot be serialized.
pub fn proposal_signing_bytes(submission: &ProposalSubmission<'_>) -> Result<Vec<u8>, FacadeError> {
    Ok(serde_json::to_vec(&looklab_schema::ProposalSigningBody {
        kind: submission.kind,
        payload: submission.payload,
        fixtures: submission.fixtures,
    })?)
}

/// # Errors
/// The request body cannot be serialized.
pub fn proposal_request_bytes<S: Serialize>(
    submission: &ProposalSubmission<'_>,
    pow: &[u64],
    signed: &S,
) -> Result<Vec<u8>, FacadeError> {
    Ok(serde_json::to_vec(&ProposalRequest {
        kind: submission.kind,
        payload: submission.payload,
        fixtures: submission.fixtures,
        pow,
        signed,
    })?)
}

/// # Errors
/// The request body cannot be serialized, or the LUT frame is too large.
pub fn lut_proposal_request_bytes<S: Serialize>(
    submission: &ProposalSubmission<'_>,
    pow: &[u64],
    signed: &S,
    file: &[u8],
) -> Result<Vec<u8>, FacadeError> {
    let header = proposal_request_bytes(submission, pow, signed)?;
    looklab_schema::frame_lut_proposal(&header, file)
        .map_err(|error| FacadeError::Input(error.to_string()))
}

#[cfg(test)]
mod tests {
    use looklab_schema::{Date, FixtureLicense, Placement, Sha256Hex};
    use serde_json::{Value, json};

    use super::*;

    #[derive(serde::Serialize)]
    struct TestSigned {
        nonce: &'static str,
        credential_id: &'static str,
        authenticator_data: &'static str,
        client_data_json: &'static str,
        signature: &'static str,
    }

    fn signed() -> TestSigned {
        TestSigned {
            nonce: "nonce",
            credential_id: "credential",
            authenticator_data: "authenticator",
            client_data_json: "client",
            signature: "signature",
        }
    }

    fn donation() -> FixtureDonation {
        FixtureDonation {
            file: "sample.raf".to_owned(),
            description: "sample".to_owned(),
            make: "Fujifilm".to_owned(),
            model: "X-T5".to_owned(),
            sha256: Sha256Hex::parse(
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("digest"),
            size: 4,
            source_url: None,
            source: None,
            donator: None,
            donated_on: Date::parse("2026-01-01").expect("date"),
            license: FixtureLicense::Cc0_1_0,
            placement: Placement::Fetch,
        }
    }

    #[test]
    fn the_signing_body_matches_the_request_header() {
        let fixtures = [donation()];
        let submission = ProposalSubmission {
            kind: ProposalKind::Body,
            payload: "body: fujifilm/x-t5",
            fixtures: &fixtures,
        };
        let signing: Value =
            serde_json::from_slice(&proposal_signing_bytes(&submission).expect("signing"))
                .expect("json");
        let request: Value = serde_json::from_slice(
            &proposal_request_bytes(&submission, &[1, 2, 3], &signed()).expect("request"),
        )
        .expect("json");
        assert_eq!(signing["kind"], request["kind"]);
        assert_eq!(signing["payload"], request["payload"]);
        assert_eq!(signing["fixtures"], request["fixtures"]);
        assert_eq!(request["pow"], json!([1, 2, 3]));
        assert_eq!(request["nonce"], "nonce");
        assert!(request.get("signed").is_none());
    }

    #[test]
    fn a_changed_payload_changes_the_signing_bytes() {
        let submission = ProposalSubmission {
            kind: ProposalKind::Body,
            payload: "body: fujifilm/x-t5",
            fixtures: &[],
        };
        let changed = ProposalSubmission {
            payload: "body: fujifilm/x-t4",
            ..submission
        };
        assert_ne!(
            proposal_signing_bytes(&submission).expect("signing"),
            proposal_signing_bytes(&changed).expect("signing")
        );
    }

    #[test]
    fn the_lut_frame_preserves_the_file_bytes_and_header_binding() {
        let fixtures = [donation()];
        let submission = ProposalSubmission {
            kind: ProposalKind::LutLook,
            payload: "look: sample",
            fixtures: &fixtures,
        };
        let file = [0_u8, 0xff, 1, 2, 3, 0x80];
        let framed =
            lut_proposal_request_bytes(&submission, &[7], &signed(), &file).expect("frame");
        let (header, bytes) = looklab_schema::split_lut_proposal(&framed).expect("split");
        assert_eq!(bytes, file.as_slice());
        let header: Value = serde_json::from_slice(header).expect("json");
        assert_eq!(header["kind"], "lut_look");
        assert_eq!(header["payload"], submission.payload);
        assert_eq!(header["fixtures"], json!([donation()]));
        assert_eq!(header["pow"], json!([7]));
    }
}
