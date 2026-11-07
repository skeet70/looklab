// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every rejection reason the sample reader and constructor can produce.

mod common;

use looklab_samples::{
    BlobKind, Error, Field, InputSpace, OutputSpace, PrepParamsHash, PrepVersion,
    PriorTransformVersion, SampleFlags, SampleHeader, SampleSet, decode, encode,
};
use looklab_samples::{MAX_SAMPLES, MAX_WINDOW_BYTES};

use common::{compress, container, raw_body, sample_blob, sample_header_bytes, valid_row};

fn header_bytes_with(n: u32) -> Vec<u8> {
    sample_header_bytes(n, 0, 0, [1.0, 1.0, 1.0], 5000.0, 0.0)
}

fn patch_prefix(bytes: &mut [u8], offset: usize, value: &[u8]) {
    bytes[offset..offset + value.len()].copy_from_slice(value);
}

#[test]
fn empty_input_is_too_short() {
    assert!(matches!(decode(&[]), Err(Error::TooShort { len: 0 })));
}

#[test]
fn bad_magic_is_reported_with_the_bytes_found() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    bytes[5] = b'X';
    match decode(&bytes) {
        Err(Error::BadMagic { found, expected }) => {
            assert_eq!(expected, *b"LLSAMP");
            assert_eq!(found, *b"LLSAMX");
        }
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn unknown_version_is_rejected() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    patch_prefix(&mut bytes, 6, &2u16.to_le_bytes());
    assert!(matches!(decode(&bytes), Err(Error::UnsupportedVersion(2))));
}

#[test]
fn header_length_over_the_limit_is_rejected() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    patch_prefix(&mut bytes, 8, &5000u32.to_le_bytes());
    assert!(matches!(
        decode(&bytes),
        Err(Error::HeaderTooLong { len: 5000, .. })
    ));
}

#[test]
fn header_length_past_the_input_is_rejected() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    patch_prefix(&mut bytes, 8, &4000u32.to_le_bytes());
    assert!(matches!(
        decode(&bytes),
        Err(Error::HeaderBeyondInput { len: 4000, .. })
    ));
}

#[test]
fn a_header_length_of_u32_max_is_rejected_before_the_offset_arithmetic() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    patch_prefix(&mut bytes, 8, &u32::MAX.to_le_bytes());
    assert!(matches!(
        decode(&bytes),
        Err(Error::HeaderTooLong { len: u32::MAX, .. })
    ));
}

#[test]
fn trailing_bytes_inside_the_header_are_rejected() {
    let mut header = header_bytes_with(1);
    header.push(0);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(decode(&bytes), Err(Error::TrailingHeaderBytes)));
}

#[test]
fn unknown_input_space_is_rejected() {
    let header = sample_header_bytes(1, 3, 0, [1.0, 1.0, 1.0], 5000.0, 0.0);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(decode(&bytes), Err(Error::UnknownInputSpace(3))));
}

#[test]
fn unknown_output_space_is_rejected() {
    let header = sample_header_bytes(1, 0, 7, [1.0, 1.0, 1.0], 5000.0, 0.0);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(decode(&bytes), Err(Error::UnknownOutputSpace(7))));
}

#[test]
fn zero_sample_count_is_rejected() {
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(0),
        &compress(&[]),
    );
    assert!(matches!(
        decode(&bytes),
        Err(Error::CountOutOfRange {
            kind: BlobKind::Samples,
            n: 0,
            ..
        })
    ));
}

#[test]
fn oversized_sample_count_is_rejected() {
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(MAX_SAMPLES + 1),
        &compress(&[]),
    );
    assert!(matches!(
        decode(&bytes),
        Err(Error::CountOutOfRange {
            kind: BlobKind::Samples,
            n,
            ..
        }) if n == MAX_SAMPLES + 1
    ));
}

#[test]
fn short_body_is_rejected() {
    let bytes = sample_blob(2, &valid_row(0.25));
    match decode(&bytes) {
        Err(Error::BodySizeMismatch { actual, expected }) => {
            assert_eq!(actual, looklab_samples::ROW_BYTES);
            assert_eq!(expected, 2 * looklab_samples::ROW_BYTES);
        }
        other => panic!("expected BodySizeMismatch, got {other:?}"),
    }
}

#[test]
fn zip_bomb_is_capped_before_allocation() {
    let bomb = vec![0u8; 4 << 20];
    let bytes = sample_blob(1, &bomb);
    let cap = looklab_samples::ROW_BYTES + looklab_samples::DECOMPRESS_HEADROOM;
    match decode(&bytes) {
        Err(Error::BodySizeMismatch { actual, expected }) => {
            assert_eq!(expected, looklab_samples::ROW_BYTES);
            assert_eq!(actual, cap);
        }
        other => panic!("expected BodySizeMismatch, got {other:?}"),
    }
}

#[test]
fn a_compressed_block_that_expands_past_the_cap_is_cut_off() {
    use std::io::Read;

    let plaintext = vec![0u8; 128 * 1024];
    let frame = compress(&plaintext);
    let mut expanded = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(&frame[..])
        .expect("the seed frame has a valid header")
        .read_to_end(&mut expanded)
        .expect("the seed frame decodes");
    assert_eq!(expanded.len(), plaintext.len());

    let blocks = &frame[common::frame_header_len(&frame)..];
    let bomb = |window_descriptor: u8| {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0b1000_0000, window_descriptor];
        frame.extend_from_slice(&(looklab_samples::ROW_BYTES as u32).to_le_bytes());
        frame.extend_from_slice(blocks);
        frame
    };
    let wrap = |body: &[u8]| {
        container(
            looklab_samples::SAMPLE_MAGIC,
            looklab_samples::SAMPLE_VERSION,
            &header_bytes_with(1),
            body,
        )
    };
    let cap = looklab_samples::ROW_BYTES + looklab_samples::DECOMPRESS_HEADROOM;

    // A 1 KiB window: the block would expand to 128 KiB inside the decoder, so
    // ruzstd refuses it before materialising any of it.
    assert!(matches!(
        decode(&wrap(&bomb(0x00))),
        Err(Error::InvalidBody)
    ));

    // A 128 KiB window: the block is within the decoder's bound, but far past
    // the reader's cap, which stops the collection.
    match decode(&wrap(&bomb(0x38))) {
        Err(Error::BodySizeMismatch { actual, expected }) => {
            assert_eq!(expected, looklab_samples::ROW_BYTES);
            assert_eq!(actual, cap);
        }
        other => panic!("expected BodySizeMismatch, got {other:?}"),
    }
}

#[test]
fn trailing_bytes_after_the_frame_are_rejected() {
    let mut bytes = encode(&common::set(2)).expect("valid set encodes");
    bytes.extend_from_slice(b"junk");
    assert!(matches!(decode(&bytes), Err(Error::TrailingBodyBytes)));
}

#[test]
fn nan_in_the_input_column_is_rejected() {
    let raw = raw_body(&[[f32::NAN, 0.5, 0.5]], &[[0.1, 0.2, 0.3]], &[1.0], &[0]);
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::NonFinite {
            field: Field::InRgb,
            index: 0
        })
    ));
}

#[test]
fn infinity_in_the_output_column_is_rejected() {
    let raw = raw_body(
        &[[0.1, 0.2, 0.3]],
        &[[0.1, 0.2, f32::INFINITY]],
        &[1.0],
        &[0],
    );
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::NonFinite {
            field: Field::OutRgb,
            index: 0
        })
    ));
}

#[test]
fn rgb_above_the_ceiling_is_rejected() {
    let raw = raw_body(&[[70.0, 0.5, 0.5]], &[[0.1, 0.2, 0.3]], &[1.0], &[0]);
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::ValueOutOfRange {
            field: Field::InRgb,
            index: 0,
            ..
        })
    ));
}

#[test]
fn rgb_below_the_floor_is_rejected() {
    let raw = raw_body(&[[-0.6, 0.5, 0.5]], &[[0.1, 0.2, 0.3]], &[1.0], &[0]);
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::ValueOutOfRange {
            field: Field::InRgb,
            index: 0,
            ..
        })
    ));
}

#[test]
fn zero_density_weight_is_rejected() {
    let raw = raw_body(&[[0.1, 0.2, 0.3]], &[[0.1, 0.2, 0.3]], &[0.0], &[0]);
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::NotPositive {
            field: Field::DensityW,
            index: 0
        })
    ));
}

#[test]
fn negative_density_weight_is_rejected() {
    let raw = raw_body(&[[0.1, 0.2, 0.3]], &[[0.1, 0.2, 0.3]], &[-1.0e-30], &[0]);
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::NotPositive {
            field: Field::DensityW,
            index: 0
        })
    ));
}

#[test]
fn subnormal_density_weights_are_accepted() {
    let smallest = f32::from_bits(1);
    let raw = raw_body(&[[0.1, 0.2, 0.3]], &[[0.1, 0.2, 0.3]], &[smallest], &[0]);
    let decoded = decode(&sample_blob(1, &raw)).expect("a positive subnormal weight is in range");
    assert_eq!(decoded.density_w()[0].to_bits(), smallest.to_bits());
}

#[test]
fn the_density_ceiling_is_inclusive() {
    let raw = raw_body(
        &[[0.1, 0.2, 0.3]],
        &[[0.1, 0.2, 0.3]],
        &[looklab_samples::MAX_DENSITY_WEIGHT],
        &[0],
    );
    let decoded = decode(&sample_blob(1, &raw)).expect("the ceiling itself is allowed");
    assert_eq!(decoded.density_w()[0], looklab_samples::MAX_DENSITY_WEIGHT);
}

#[test]
fn density_weight_above_the_cap_is_rejected() {
    let raw = raw_body(
        &[[0.1, 0.2, 0.3]],
        &[[0.1, 0.2, 0.3]],
        &[looklab_samples::MAX_DENSITY_WEIGHT * 2.0],
        &[0],
    );
    let bytes = sample_blob(1, &raw);
    assert!(matches!(
        decode(&bytes),
        Err(Error::ValueOutOfRange {
            field: Field::DensityW,
            ..
        })
    ));
}

#[test]
fn undefined_flag_bits_are_rejected() {
    let raw = raw_body(&[[0.1, 0.2, 0.3]], &[[0.1, 0.2, 0.3]], &[1.0], &[0x08]);
    let bytes = sample_blob(1, &raw);
    match decode(&bytes) {
        Err(Error::UnknownFlagBits { index, bits }) => {
            assert_eq!(index, 0);
            assert_eq!(bits, 0x08);
        }
        other => panic!("expected UnknownFlagBits, got {other:?}"),
    }
}

#[test]
fn an_oversized_zstd_window_is_rejected_before_decoding() {
    let descriptor = 0x00;
    let window_descriptor = 24u8 << 3;
    let body = [0x28, 0xb5, 0x2f, 0xfd, descriptor, window_descriptor];
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(1),
        &body,
    );
    match decode(&bytes) {
        Err(Error::WindowTooLarge { window, max }) => {
            assert_eq!(window, 1u64 << 34);
            assert_eq!(max, MAX_WINDOW_BYTES);
        }
        other => panic!("expected WindowTooLarge, got {other:?}"),
    }
}

#[test]
fn a_declared_content_size_that_disagrees_is_rejected() {
    let body = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 40];
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(1),
        &body,
    );
    match decode(&bytes) {
        Err(Error::FrameContentSizeMismatch { declared, expected }) => {
            assert_eq!(declared, 40);
            assert_eq!(expected, looklab_samples::ROW_BYTES as u64);
        }
        other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
    }
}

#[test]
fn a_body_that_is_not_a_zstd_frame_is_rejected() {
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(1),
        b"no zstd here",
    );
    assert!(matches!(decode(&bytes), Err(Error::InvalidFrameHeader)));
}

#[test]
fn a_truncated_frame_is_rejected() {
    let body = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x38];
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header_bytes_with(1),
        &body,
    );
    assert!(matches!(decode(&bytes), Err(Error::InvalidBody)));
}

#[test]
fn an_empty_header_is_not_a_postcard_struct() {
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &[],
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(decode(&bytes), Err(Error::HeaderCodec(_))));
}

#[test]
fn header_hints_outside_their_ranges_are_rejected() {
    let header = sample_header_bytes(1, 0, 0, [1.0, 1.0, 1.0], -1.0, 0.0);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(
        decode(&bytes),
        Err(Error::HeaderValueOutOfRange {
            field: "cct_hint_k",
            ..
        })
    ));
}

#[test]
fn non_positive_wb_multipliers_are_rejected() {
    let header = sample_header_bytes(1, 0, 0, [0.0, 1.0, 1.0], 5000.0, 0.0);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(
        decode(&bytes),
        Err(Error::HeaderValueOutOfRange {
            field: "wb_multipliers",
            ..
        })
    ));
}

#[test]
fn a_non_finite_exposure_hint_is_rejected() {
    let header = sample_header_bytes(1, 0, 0, [1.0, 1.0, 1.0], 5000.0, f32::NAN);
    let bytes = container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &header,
        &compress(&valid_row(0.25)),
    );
    assert!(matches!(
        decode(&bytes),
        Err(Error::NonFiniteHeader {
            field: "exposure_hint_ev"
        })
    ));
}

fn header(n: u32) -> SampleHeader {
    SampleHeader {
        n,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 5000.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(1),
        prior_transform_version: PriorTransformVersion::new(1),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    }
}

#[test]
fn a_column_of_the_wrong_length_is_rejected_by_the_constructor() {
    let result = SampleSet::new(
        header(2),
        vec![[0.1, 0.2, 0.3]],
        vec![[0.1, 0.2, 0.3], [0.1, 0.2, 0.3]],
        vec![1.0, 1.0],
        vec![SampleFlags::NONE, SampleFlags::NONE],
    );
    assert!(matches!(
        result,
        Err(Error::ColumnLengthMismatch {
            field: Field::InRgb,
            len: 1,
            n: 2
        })
    ));
}

#[test]
fn a_flags_column_of_the_wrong_length_is_rejected_by_the_constructor() {
    let result = SampleSet::new(
        header(2),
        vec![[0.1, 0.2, 0.3]; 2],
        vec![[0.1, 0.2, 0.3]; 2],
        vec![1.0; 2],
        vec![SampleFlags::NONE],
    );
    assert!(matches!(
        result,
        Err(Error::ColumnLengthMismatch {
            field: Field::Flags,
            len: 1,
            n: 2
        })
    ));
}

#[test]
fn out_of_range_values_are_rejected_by_the_constructor() {
    let result = SampleSet::new(
        header(1),
        vec![[0.1, 0.2, 0.3]],
        vec![[0.1, 0.2, 0.3]],
        vec![f32::NAN],
        vec![SampleFlags::NONE],
    );
    assert!(matches!(
        result,
        Err(Error::NonFinite {
            field: Field::DensityW,
            index: 0
        })
    ));
}

#[test]
fn undefined_flag_bits_have_no_constructor() {
    assert!(SampleFlags::from_bits(0b1000).is_none());
    assert_eq!(
        SampleFlags::from_bits(0b0111).expect("defined bits"),
        SampleFlags::CHART | SampleFlags::PATTERN | SampleFlags::NEAR_CLIP_OUTPUT
    );
}
