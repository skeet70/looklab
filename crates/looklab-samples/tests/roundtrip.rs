// SPDX-License-Identifier: AGPL-3.0-or-later
//! Round-trip and determinism properties of the sample blob.

mod common;

use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, RerenderGroupId,
    SampleFlags, SampleHeader, SampleSet, decode, encode,
};
use proptest::prelude::*;

fn flags() -> impl Strategy<Value = SampleFlags> {
    (0u8..=SampleFlags::KNOWN_BITS).prop_map(|bits| {
        SampleFlags::from_bits(bits).expect("prop strategy stays inside the defined bits")
    })
}

fn rgb() -> impl Strategy<Value = [f32; 3]> {
    prop::array::uniform(-0.5f32..=64.0f32)
}

fn set() -> impl Strategy<Value = SampleSet> {
    (
        1u32..=48,
        0u8..=1,
        prop::array::uniform(0.01f32..=64.0f32),
        0.0f32..=100_000.0f32,
        -16.0f32..=16.0f32,
        prop::option::of(prop::array::uniform::<_, 16>(any::<u8>())),
        0u32..=7,
        0u32..=7,
        prop::array::uniform::<_, 32>(any::<u8>()),
    )
        .prop_flat_map(
            |(
                n,
                output_space,
                wb_multipliers,
                cct_hint_k,
                exposure_hint_ev,
                rerender_group,
                prep_version,
                prior_transform_version,
                prep_params_hash,
            )| {
                let rows = n as usize;
                (
                    Just(SampleHeader {
                        n,
                        input_space: InputSpace::CameraRgbWbLinear,
                        output_space: if output_space == 0 {
                            OutputSpace::Srgb
                        } else {
                            OutputSpace::AdobeRgb
                        },
                        wb_multipliers,
                        cct_hint_k,
                        exposure_hint_ev,
                        rerender_group: rerender_group.map(RerenderGroupId::new),
                        prep_version: PrepVersion::new(prep_version),
                        prior_transform_version: PriorTransformVersion::new(
                            prior_transform_version,
                        ),
                        prep_params_hash: PrepParamsHash::new(prep_params_hash),
                    }),
                    (
                        prop::collection::vec(rgb(), rows..=rows),
                        prop::collection::vec(rgb(), rows..=rows),
                        prop::collection::vec(0.0001f32..=1.0e6f32, rows..=rows),
                        prop::collection::vec(flags(), rows..=rows),
                    ),
                )
                    .prop_map(|(header, (in_rgb, out_rgb, density_w, flags))| {
                        SampleSet::new(header, in_rgb, out_rgb, density_w, flags)
                            .expect("generated columns satisfy every limit")
                    })
            },
        )
}

proptest! {
    #[test]
    fn roundtrip_preserves_header_and_columns(set in set()) {
        let bytes = encode(&set).expect("valid sets encode");
        let decoded = decode(&bytes).expect("encoded bytes decode");
        prop_assert_eq!(&decoded, &set);
    }

    #[test]
    fn encode_is_deterministic(set in set()) {
        let first = encode(&set).expect("valid sets encode");
        let second = encode(&set).expect("valid sets encode");
        prop_assert_eq!(first, second);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(set) = decode(&bytes) {
            let again = decode(&encode(&set).expect("decoded sets re-encode"))
                .expect("re-encoded bytes decode");
            prop_assert_eq!(again, set);
        }
    }
}

#[test]
fn roundtrip_at_the_sample_count_limit() {
    let set = common::set(looklab_samples::MAX_SAMPLES as usize);
    let bytes = encode(&set).expect("a full-size set encodes");
    let decoded = decode(&bytes).expect("a full-size blob decodes");
    assert_eq!(decoded, set);
    assert_eq!(decoded.n(), looklab_samples::MAX_SAMPLES as usize);
}

#[test]
fn two_equal_sets_encode_to_the_same_bytes() {
    let a = common::set(9);
    let b = common::set(9);
    assert_eq!(encode(&a).expect("encodes"), encode(&b).expect("encodes"));
}

#[test]
fn rerender_group_survives_the_roundtrip() {
    let mut header = common::header(2);
    header.rerender_group = Some(RerenderGroupId::new([3; 16]));
    let set = SampleSet::new(
        header,
        vec![[0.1, 0.2, 0.3], [0.4, 0.5, 0.6]],
        vec![[0.2, 0.3, 0.4], [0.5, 0.6, 0.7]],
        vec![1.0, 2.0],
        vec![
            SampleFlags::CHART,
            SampleFlags::PATTERN | SampleFlags::NEAR_CLIP_OUTPUT,
        ],
    )
    .expect("valid columns");
    let decoded = decode(&encode(&set).expect("valid set encodes")).expect("encodes then decodes");
    assert_eq!(decoded, set);
}
