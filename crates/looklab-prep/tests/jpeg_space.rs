// SPDX-License-Identifier: AGPL-3.0-or-later
//! Colour-space detection at the public `decode_jpeg` boundary.

use jpeg_encoder::{ColorType, Encoder};
use looklab_prep::{PrepError, decode_jpeg};
use looklab_samples::OutputSpace;

const WIDTH: u16 = 16;
const HEIGHT: u16 = 12;

fn encoded_jpeg() -> Vec<u8> {
    let pixels = vec![128_u8; usize::from(WIDTH) * usize::from(HEIGHT) * 3];
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, 90)
        .encode(&pixels, WIDTH, HEIGHT, ColorType::Rgb)
        .expect("encodes JPEG");
    bytes
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let length = u16::try_from(payload.len() + 2).expect("segment length fits");
    let mut bytes = vec![0xFF, marker];
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn app2_icc_chunk(profile: &[u8], sequence: u8, total: u8) -> Vec<u8> {
    let mut payload = b"ICC_PROFILE\0".to_vec();
    payload.push(sequence);
    payload.push(total);
    payload.extend_from_slice(profile);
    segment(0xE2, &payload)
}

fn app2_icc(profile: &[u8]) -> Vec<u8> {
    app2_icc_chunk(profile, 1, 1)
}

fn split_icc(profile: &[u8], count: usize) -> Vec<Vec<u8>> {
    let total = u8::try_from(count).expect("chunk count fits");
    let chunk_len = profile.len().div_ceil(count);
    profile
        .chunks(chunk_len)
        .enumerate()
        .map(|(index, data)| {
            app2_icc_chunk(data, u8::try_from(index + 1).expect("sequence fits"), total)
        })
        .collect()
}

fn app1_exif_color_space(color_space: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42_u16.to_le_bytes());
    tiff.extend_from_slice(&8_u32.to_le_bytes());
    tiff.extend_from_slice(&1_u16.to_le_bytes());
    tiff.extend_from_slice(&0xA001_u16.to_le_bytes());
    tiff.extend_from_slice(&3_u16.to_le_bytes());
    tiff.extend_from_slice(&1_u32.to_le_bytes());
    tiff.extend_from_slice(&color_space.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0_u32.to_le_bytes());

    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    segment(0xE1, &payload)
}

fn with_segments(segments: &[Vec<u8>]) -> Vec<u8> {
    let jpeg = encoded_jpeg();
    let mut bytes = Vec::with_capacity(jpeg.len() + segments.iter().map(Vec::len).sum::<usize>());
    bytes.extend_from_slice(&jpeg[..2]);
    for segment in segments {
        bytes.extend_from_slice(segment);
    }
    bytes.extend_from_slice(&jpeg[2..]);
    bytes
}

// Generated with LittleCMS 2.19 for these tests. No rights reserved.
const SRGB_V2: &[u8] = include_bytes!("data/srgb-lcms-v2.icc");
const SRGB_V4: &[u8] = include_bytes!("data/srgb-lcms-v4.icc");
const ADOBE_RGB_V2: &[u8] = include_bytes!("data/rgb1998-lcms-v2.icc");
// Incidental UTF-16 Adobe RGB reference in the copyright.
const SRGB_V4_INCIDENTAL_ADOBE: &[u8] = include_bytes!("data/srgb-lcms-v4-incidental-adobe.icc");
// Incidental UTF-16 sRGB reference in the copyright.
const DISPLAY_P3_INCIDENTAL_SRGB: &[u8] =
    include_bytes!("data/display-p3-lcms-v4-incidental-srgb.icc");

fn tag_entry(profile: &[u8], signature: &[u8; 4]) -> usize {
    let count = u32::from_be_bytes(
        profile[128..132]
            .try_into()
            .expect("fixture has a tag count"),
    );
    (0..usize::try_from(count).expect("tag count fits"))
        .map(|index| 132 + index * 12)
        .find(|&entry| &profile[entry..entry + 4] == signature.as_slice())
        .expect("fixture has the requested tag")
}

fn tag_offset(profile: &[u8], signature: &[u8; 4]) -> usize {
    let entry = tag_entry(profile, signature);
    usize::try_from(u32::from_be_bytes(
        profile[entry + 4..entry + 8]
            .try_into()
            .expect("fixture has a tag offset"),
    ))
    .expect("tag offset fits")
}

fn tag_size(profile: &[u8], signature: &[u8; 4]) -> usize {
    let entry = tag_entry(profile, signature);
    usize::try_from(u32::from_be_bytes(
        profile[entry + 8..entry + 12]
            .try_into()
            .expect("fixture has a tag size"),
    ))
    .expect("tag size fits")
}

fn with_unicode_description(profile: &[u8], description: &str) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let offset = tag_offset(&bytes, b"desc");
    let size = tag_size(&bytes, b"desc");
    let tag = &mut bytes[offset..offset + size];
    let unicode: Vec<u16> = description.encode_utf16().collect();
    assert!(
        20 + unicode.len() * 2 <= tag.len(),
        "description fits the fixture tag"
    );
    tag.fill(0);
    tag[..4].copy_from_slice(b"desc");
    tag[16..20].copy_from_slice(
        &u32::try_from(unicode.len())
            .expect("count fits")
            .to_be_bytes(),
    );
    for (index, unit) in unicode.iter().enumerate() {
        tag[20 + index * 2..20 + index * 2 + 2].copy_from_slice(&unit.to_be_bytes());
    }
    bytes
}

fn with_ascii_description(profile: &[u8], description: &str) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let offset = tag_offset(&bytes, b"desc");
    let size = tag_size(&bytes, b"desc");
    let tag = &mut bytes[offset..offset + size];
    let ascii = description.as_bytes();
    let ascii_count = ascii.len() + 1;
    assert!(
        12 + ascii_count + 8 <= tag.len(),
        "description fits the fixture tag"
    );
    tag.fill(0);
    tag[..4].copy_from_slice(b"desc");
    tag[8..12].copy_from_slice(
        &u32::try_from(ascii_count)
            .expect("count fits")
            .to_be_bytes(),
    );
    tag[12..12 + ascii.len()].copy_from_slice(ascii);
    bytes
}

fn without_description(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let entry = tag_entry(&bytes, b"desc");
    bytes[entry..entry + 4].copy_from_slice(b"zzzz");
    bytes
}

fn with_broken_description(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let entry = tag_entry(&bytes, b"desc");
    bytes[entry + 4..entry + 8].copy_from_slice(&u32::MAX.to_be_bytes());
    bytes
}

fn with_broken_record_count(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let offset = tag_offset(&bytes, b"desc");
    bytes[offset + 8..offset + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    bytes
}

fn with_broken_signature(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    bytes[36..40].copy_from_slice(b"xxxx");
    bytes
}

fn with_broken_color_space(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    bytes[16..20].copy_from_slice(b"CMYK");
    bytes
}

fn with_trailing_bytes(profile: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    bytes.push(0);
    bytes
}

fn append_description_tag(profile: &[u8], tag: &[u8]) -> Vec<u8> {
    let mut bytes = profile.to_vec();
    let entry = tag_entry(&bytes, b"desc");
    let tag_offset = bytes.len();
    bytes.extend_from_slice(tag);
    bytes[entry + 4..entry + 8].copy_from_slice(
        &u32::try_from(tag_offset)
            .expect("tag offset fits")
            .to_be_bytes(),
    );
    bytes[entry + 8..entry + 12].copy_from_slice(
        &u32::try_from(tag.len())
            .expect("tag size fits")
            .to_be_bytes(),
    );
    let size = u32::try_from(bytes.len()).expect("profile size fits");
    bytes[0..4].copy_from_slice(&size.to_be_bytes());
    bytes
}

fn with_mluc(profile: &[u8], descriptions: &[&str]) -> Vec<u8> {
    let record_table = 16 + descriptions.len() * 12;
    let strings: Vec<Vec<u16>> = descriptions
        .iter()
        .map(|description| description.encode_utf16().collect())
        .collect();
    let tag_size = record_table + strings.iter().map(|string| string.len() * 2).sum::<usize>();
    let mut tag = vec![0_u8; tag_size];
    tag[..4].copy_from_slice(b"mluc");
    tag[8..12].copy_from_slice(
        &u32::try_from(descriptions.len())
            .expect("record count fits")
            .to_be_bytes(),
    );
    tag[12..16].copy_from_slice(&12_u32.to_be_bytes());
    let mut string_offset = record_table;
    for (index, string) in strings.iter().enumerate() {
        let record = 16 + index * 12;
        tag[record..record + 2].copy_from_slice(b"en");
        tag[record + 2..record + 4].copy_from_slice(b"US");
        tag[record + 4..record + 8].copy_from_slice(
            &u32::try_from(string.len() * 2)
                .expect("string length fits")
                .to_be_bytes(),
        );
        tag[record + 8..record + 12].copy_from_slice(
            &u32::try_from(string_offset)
                .expect("string offset fits")
                .to_be_bytes(),
        );
        for (unit_index, unit) in string.iter().enumerate() {
            tag[string_offset + unit_index * 2..string_offset + unit_index * 2 + 2]
                .copy_from_slice(&unit.to_be_bytes());
        }
        string_offset += string.len() * 2;
    }
    append_description_tag(profile, &tag)
}

fn with_shared_mluc(profile: &[u8], records: usize, text: &str) -> Vec<u8> {
    let string: Vec<u16> = text.encode_utf16().collect();
    let string_bytes = string.len() * 2;
    let string_offset = 16 + records * 12;
    let mut tag = vec![0_u8; string_offset + string_bytes];
    tag[..4].copy_from_slice(b"mluc");
    tag[8..12].copy_from_slice(
        &u32::try_from(records)
            .expect("record count fits")
            .to_be_bytes(),
    );
    tag[12..16].copy_from_slice(&12_u32.to_be_bytes());
    for index in 0..records {
        let record = 16 + index * 12;
        tag[record..record + 2].copy_from_slice(b"en");
        tag[record + 2..record + 4].copy_from_slice(b"US");
        tag[record + 4..record + 8].copy_from_slice(
            &u32::try_from(string_bytes)
                .expect("string length fits")
                .to_be_bytes(),
        );
        tag[record + 8..record + 12].copy_from_slice(
            &u32::try_from(string_offset)
                .expect("string offset fits")
                .to_be_bytes(),
        );
    }
    for (index, unit) in string.iter().enumerate() {
        tag[string_offset + index * 2..string_offset + index * 2 + 2]
            .copy_from_slice(&unit.to_be_bytes());
    }
    append_description_tag(profile, &tag)
}

fn app2_icc_chunks(profile: &[u8]) -> Vec<Vec<u8>> {
    let count = profile.len().div_ceil(60_000).max(1);
    split_icc(profile, count)
}

#[test]
fn an_explicit_unsupported_icc_profile_is_an_error() {
    for color_space in [1, 2] {
        let bytes = with_segments(&[
            app2_icc(DISPLAY_P3_INCIDENTAL_SRGB),
            app1_exif_color_space(color_space),
        ]);
        let error = decode_jpeg(&bytes).expect_err("unsupported ICC profile must not decode");
        assert!(
            matches!(error, PrepError::UnsupportedColorProfile),
            "Display P3 with EXIF colour space {color_space} must be rejected as unsupported"
        );
    }
}

#[test]
fn an_unsupported_profile_takes_precedence_over_a_decode_failure() {
    let mut bytes = vec![0xFF, 0xD8];
    bytes.extend_from_slice(&app2_icc(DISPLAY_P3_INCIDENTAL_SRGB));
    bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);

    let error = decode_jpeg(&bytes).expect_err("unsupported ICC profile must not decode");
    assert!(matches!(error, PrepError::UnsupportedColorProfile));
}

#[test]
fn missing_and_recognized_profiles_keep_their_space() {
    let untagged = decode_jpeg(&encoded_jpeg()).expect("untagged JPEG decodes");
    assert_eq!(untagged.space, OutputSpace::Srgb);
    assert!(untagged.space_assumed);

    let exif_srgb =
        decode_jpeg(&with_segments(&[app1_exif_color_space(1)])).expect("EXIF sRGB decodes");
    assert_eq!(exif_srgb.space, OutputSpace::Srgb);
    assert!(!exif_srgb.space_assumed);

    let exif_adobe =
        decode_jpeg(&with_segments(&[app1_exif_color_space(2)])).expect("EXIF Adobe RGB decodes");
    assert_eq!(exif_adobe.space, OutputSpace::AdobeRgb);
    assert!(!exif_adobe.space_assumed);

    let v4_srgb = decode_jpeg(&with_segments(&[
        app2_icc(SRGB_V4),
        app1_exif_color_space(1),
    ]))
    .expect("v4 ICC sRGB with EXIF sRGB decodes");
    assert_eq!(v4_srgb.space, OutputSpace::Srgb);
    assert!(!v4_srgb.space_assumed);
}

#[test]
fn a_recognized_profile_overrides_contradictory_exif() {
    for profile in [SRGB_V2, SRGB_V4] {
        let decoded = decode_jpeg(&with_segments(&[
            app2_icc(profile),
            app1_exif_color_space(2),
        ]))
        .expect("ICC sRGB decodes");
        assert_eq!(decoded.space, OutputSpace::Srgb);
        assert!(!decoded.space_assumed);
    }

    let decoded = decode_jpeg(&with_segments(&[
        app2_icc(ADOBE_RGB_V2),
        app1_exif_color_space(1),
    ]))
    .expect("ICC Adobe RGB decodes");
    assert_eq!(decoded.space, OutputSpace::AdobeRgb);
    assert!(!decoded.space_assumed);
}

#[test]
fn a_v2_unicode_description_is_recognized_without_an_ascii_field() {
    let profile = with_unicode_description(SRGB_V2, "sRGB built-in");
    let decoded = decode_jpeg(&with_segments(&[
        app2_icc(&profile),
        app1_exif_color_space(2),
    ]))
    .expect("v2 Unicode description decodes");
    assert_eq!(decoded.space, OutputSpace::Srgb);
    assert!(!decoded.space_assumed);
}

#[test]
fn a_supported_description_wins_over_incidental_names_in_other_tags() {
    let decoded = decode_jpeg(&with_segments(&[
        app2_icc(SRGB_V4_INCIDENTAL_ADOBE),
        app1_exif_color_space(2),
    ]))
    .expect("sRGB description wins over an incidental Adobe RGB copyright");
    assert_eq!(decoded.space, OutputSpace::Srgb);
    assert!(!decoded.space_assumed);
}

#[test]
fn numbered_chunks_assemble_in_sequence_order() {
    for (profile, expected) in [
        (SRGB_V4, OutputSpace::Srgb),
        (ADOBE_RGB_V2, OutputSpace::AdobeRgb),
    ] {
        let chunks = split_icc(profile, 3);
        assert_eq!(chunks.len(), 3);
        let reversed: Vec<Vec<u8>> = chunks.iter().rev().cloned().collect();
        let decoded = decode_jpeg(&with_segments(&reversed)).expect("chunked ICC profile decodes");
        assert_eq!(decoded.space, expected);
        assert!(!decoded.space_assumed);
    }
}

#[test]
fn a_profile_without_a_description_is_an_error() {
    let profile = without_description(SRGB_V2);
    let error = decode_jpeg(&with_segments(&[
        app2_icc(&profile),
        app1_exif_color_space(1),
    ]))
    .expect_err("a profile without a description must not decode");
    assert!(matches!(error, PrepError::UnsupportedColorProfile));
}

#[test]
fn malformed_icc_payloads_are_rejected() {
    let chunks = split_icc(SRGB_V4, 2);
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![chunks[0].clone()],
        vec![chunks[0].clone(), chunks[0].clone()],
        vec![chunks[0].clone(), app2_icc_chunk(b"x", 2, 3)],
        vec![app2_icc_chunk(SRGB_V4, 0, 1)],
        vec![app2_icc_chunk(SRGB_V4, 2, 1)],
        vec![app2_icc_chunk(SRGB_V4, 1, 0)],
        vec![app2_icc_chunk(&[], 1, 1)],
        vec![segment(0xE2, b"ICC_PROFILE\0")],
        vec![app2_icc(&SRGB_V2[..200])],
        vec![app2_icc(&with_broken_description(SRGB_V2))],
        vec![app2_icc(&with_broken_record_count(SRGB_V4))],
    ];
    for mut segments in cases {
        segments.push(app1_exif_color_space(1));
        let error = decode_jpeg(&with_segments(&segments))
            .expect_err("a malformed ICC payload must not fall back to EXIF");
        assert!(matches!(error, PrepError::UnsupportedColorProfile));
    }
}

#[test]
fn a_corrupt_header_or_trailing_bytes_is_an_error() {
    for profile in [
        with_broken_signature(SRGB_V2),
        with_broken_color_space(SRGB_V2),
        with_trailing_bytes(SRGB_V2),
    ] {
        let error = decode_jpeg(&with_segments(&[
            app2_icc(&profile),
            app1_exif_color_space(1),
        ]))
        .expect_err("a corrupt ICC header must not decode");
        assert!(matches!(error, PrepError::UnsupportedColorProfile));
    }
}

#[test]
fn conflicting_or_unknown_descriptions_are_rejected() {
    for descriptions in [
        vec!["sRGB", "Adobe RGB"],
        vec!["sRGB", "Display P3"],
        vec!["sRGB", "Profil sRGB"],
        vec!["sRGBfoo"],
        vec!["sRGBé"],
        vec!["AdobeRGBish"],
    ] {
        let profile = with_mluc(SRGB_V4, &descriptions);
        let error = decode_jpeg(&with_segments(&[
            app2_icc(&profile),
            app1_exif_color_space(1),
        ]))
        .expect_err("a conflicting or unknown description must not decode");
        assert!(matches!(error, PrepError::UnsupportedColorProfile));
    }

    let profile = with_mluc(SRGB_V4, &["sRGB", "sRGB built-in"]);
    let decoded = decode_jpeg(&with_segments(&[
        app2_icc(&profile),
        app1_exif_color_space(2),
    ]))
    .expect("matching sRGB descriptions decode");
    assert_eq!(decoded.space, OutputSpace::Srgb);
    assert!(!decoded.space_assumed);
}

#[test]
fn non_separator_suffixes_are_rejected_in_ascii_descriptions() {
    for description in ["sRGBfoo", "sRGBé", "AdobeRGBish"] {
        let profile = with_ascii_description(SRGB_V2, description);
        let error = decode_jpeg(&with_segments(&[
            app2_icc(&profile),
            app1_exif_color_space(1),
        ]))
        .expect_err("a description with a non-separator suffix must not decode");
        assert!(matches!(error, PrepError::UnsupportedColorProfile));
    }
}

#[test]
fn shared_mluc_strings_are_bounded() {
    let text = format!("sRGB {}", "A".repeat(1_000_000));
    let profile = with_shared_mluc(SRGB_V4, 10_000, &text);
    let decoded = decode_jpeg(&with_segments(&app2_icc_chunks(&profile)))
        .expect("a profile with shared mluc ranges decodes");
    assert_eq!(decoded.space, OutputSpace::Srgb);
    assert!(!decoded.space_assumed);
}
