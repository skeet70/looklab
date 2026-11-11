// SPDX-License-Identifier: AGPL-3.0-or-later

#[path = "../../looklab-meta/tests/support/mod.rs"]
mod metadata_fixtures;

use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_wasm::coverage::{CoverageMap, coverage_map, sample_points};
use looklab_wasm::{MetadataFile, inspect_metadata};
use metadata_fixtures::{FujiValue, Value, build_jpeg, build_makernote, build_tiff};

const FORBIDDEN_KEYS: &[&str] = &[
    "seed",
    "seeds",
    "sha256",
    "hash",
    "hashes",
    "makernote",
    "makernotes",
    "raw_makernote",
    "x",
    "y",
    "position",
    "positions",
    "coordinates",
    "source_sha256",
];

fn forbidden_key(key: &str) -> bool {
    FORBIDDEN_KEYS.contains(&key)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn assert_no_private_data(value: &serde_json::Value, path: &str) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                assert!(!forbidden_key(key), "{path}.{key} is inspector-visible");
                assert_no_private_data(child, &format!("{path}.{key}"));
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_private_data(child, &format!("{path}[{index}]"));
            }
        }
        serde_json::Value::String(text) => {
            assert!(!is_digest(text), "{path} carries a digest");
        }
        _ => {}
    }
}

fn render_jpeg() -> Vec<u8> {
    build_jpeg(&build_tiff(
        &[
            (0x010f, Value::Ascii("FUJIFILM".to_owned())),
            (0x0110, Value::Ascii("X-T4".to_owned())),
        ],
        &[
            (0x9003, Value::Ascii("2024:05:17 10:11:12".to_owned())),
            (
                0x927c,
                Value::Undefined(build_makernote(&[(0x1003, FujiValue::Short(0x0500))])),
            ),
        ],
        None,
    ))
}

fn sample_blob() -> Vec<u8> {
    let header = SampleHeader {
        n: 2,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 5000.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(looklab_prep::PREP_VERSION),
        prior_transform_version: PriorTransformVersion::new(3),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    };
    let set = SampleSet::new(
        header,
        vec![[0.2, 0.3, 0.4], [0.5, 0.4, 0.3]],
        vec![[0.5, 0.6, 0.7], [0.7, 0.6, 0.5]],
        vec![1.0, 2.0],
        vec![SampleFlags::NONE, SampleFlags::NONE],
    )
    .expect("sample set is valid");
    looklab_samples::encode(&set).expect("sample set encodes")
}

#[test]
fn the_pairing_inspector_carries_no_private_data() {
    let raw = metadata_fixtures::build_raf_with_firmware(&render_jpeg(), b"0100");
    let plan = inspect_metadata(&[
        MetadataFile::new("DSCF9000.RAF".to_owned(), &raw),
        MetadataFile::new("DSCF9000.JPG".to_owned(), &render_jpeg()),
    ])
    .expect("pairing plan");
    let json = serde_json::to_value(&plan).expect("plan serializes");
    assert_no_private_data(&json, "plan");
    assert_eq!(plan.groups.len(), 1);
}

#[test]
fn the_sample_inspector_carries_only_output_colour() {
    let blob = sample_blob();
    let map: CoverageMap = coverage_map(std::slice::from_ref(&blob)).expect("coverage map");
    let json = serde_json::to_value(&map).expect("coverage map serializes");
    assert_no_private_data(&json, "coverage");
    assert_eq!(map.images, 1);
    assert_eq!(map.samples, 2);

    let points = sample_points(&[blob], usize::MAX).expect("sample points");
    assert_eq!(points.len(), 2);
    let json = serde_json::to_value(&points).expect("sample points serialize");
    assert_no_private_data(&json, "points");
    let first = json[0].as_array().expect("a point is an array");
    assert_eq!(first.len(), 3);
    assert!(first.iter().all(serde_json::Value::is_number));
}
