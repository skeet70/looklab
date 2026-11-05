// SPDX-License-Identifier: AGPL-3.0-or-later

//! Conformance render tags of [docs/04 §8.6](../../../docs/04-matching-and-export.md).

use looklab_meta::parse_render_tag;
use serde::Deserialize;

#[derive(Deserialize)]
struct Vector {
    input: String,
    result: VectorResult,
}

#[derive(Deserialize)]
struct VectorResult {
    firmware_id: Option<String>,
    firmware_version: Option<String>,
    look_slug: Option<String>,
    look_version: Option<String>,
    cid_prefix: Option<String>,
    error: Option<String>,
}

fn vectors() -> Vec<Vector> {
    let bytes = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../data/card/conformance-v1/render-tags.json"
    ));
    serde_json::from_str(bytes).expect("render-tag vectors are JSON")
}

#[test]
fn parser_matches_the_conformance_vectors() {
    let vectors = vectors();
    let mut accepted = 0;
    let mut rejected = 0;
    for vector in vectors {
        match parse_render_tag(&vector.input) {
            Ok(tag) => {
                assert!(
                    vector.result.error.is_none(),
                    "accepted {:?} but the vector expects {}",
                    vector.input,
                    vector.result.error.as_deref().unwrap_or("")
                );
                accepted += 1;
                assert_eq!(
                    tag.firmware_id.as_str(),
                    vector.result.firmware_id.as_deref().unwrap_or("")
                );
                assert_eq!(
                    tag.firmware_version.as_str(),
                    vector.result.firmware_version.as_deref().unwrap_or("")
                );
                assert_eq!(
                    tag.look_slug.as_str(),
                    vector.result.look_slug.as_deref().unwrap_or("")
                );
                assert_eq!(
                    tag.look_version.as_str(),
                    vector.result.look_version.as_deref().unwrap_or("")
                );
                assert_eq!(
                    tag.cid_prefix_hex(),
                    vector.result.cid_prefix.as_deref().unwrap_or("")
                );
            }
            Err(_) => {
                assert!(
                    vector.result.error.is_some(),
                    "rejected {:?} but the vector expects a parsed tag",
                    vector.input
                );
                rejected += 1;
            }
        }
    }
    assert!(accepted > 0, "the conformance set has no valid tags");
    assert!(rejected > 0, "the conformance set has no invalid tags");
}
