// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical owner withdrawal request bodies.

use looklab_schema::BodyId;
use looklab_upload::ManualWithdrawalRequest;

fn withdrawal() -> ManualWithdrawalRequest {
    ManualWithdrawalRequest {
        nonce: "bm9uY2U".to_owned(),
        credential_id: "Y3JlZGVudGlhbA".to_owned(),
        authenticator_data: "YXV0aGVudGljYXRvcg".to_owned(),
        client_data_json: "Y2xpZW50".to_owned(),
        signature: "c2lnbmF0dXJl".to_owned(),
        camera: BodyId::new("fujifilm/x-t5").expect("a body id"),
    }
}

#[test]
fn withdrawal_request_roundtrips_through_its_canonical_bytes() {
    let request = withdrawal();
    let bytes = request.canonical_bytes();
    let decoded: ManualWithdrawalRequest =
        serde_json::from_slice(&bytes).expect("canonical bytes decode");
    assert_eq!(decoded, request);
    assert_eq!(decoded.canonical_bytes(), bytes);
}

#[test]
fn withdrawal_request_rejects_unknown_fields_and_malformed_camera_ids() {
    let request = withdrawal();

    let mut unknown = serde_json::to_value(&request).expect("request json");
    unknown["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ManualWithdrawalRequest>(unknown).is_err());

    let mut camera = serde_json::to_value(&request).expect("request json");
    camera["camera"] = serde_json::json!("not a body id");
    assert!(serde_json::from_value::<ManualWithdrawalRequest>(camera).is_err());
}
