// SPDX-License-Identifier: AGPL-3.0-or-later
//! Conformance and round-trip tests for the firmware card (docs/04 §8.6).
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::ucs::{UcsSpace, ucs_to_jmh};
use looklab_color::{Illuminant, Rgb, RgbSpace};
use looklab_export::card::{
    Card, CardError, CardMeta, CardOptions, CardStatus, GridSide, card_status, read_card,
    write_card,
};
use looklab_export::quantize::{f16_bits, f16_value};
use looklab_model::probes;
use looklab_model::{LookDocument, ProbeCovariance, ProbeScalars, UncertaintyData};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/card/conformance-v1")
}

fn load(relative: &str) -> Vec<u8> {
    std::fs::read(data_dir().join(relative)).expect("conformance file")
}

fn card() -> Card {
    read_card(&load("valid/card-33.llk")).expect("valid card")
}

#[test]
fn every_conformance_file_matches_its_expected_outcome() {
    let index: serde_json::Value = serde_json::from_slice(&load("index.json")).expect("index json");
    let files = index["files"].as_array().expect("files array");
    assert!(!files.is_empty());
    for entry in files {
        let path = entry["path"].as_str().expect("path");
        let bytes = load(path);
        let result = read_card(&bytes);
        match entry["outcome"].as_str().expect("outcome") {
            "valid" => assert!(result.is_ok(), "{path}: {:?}", result.err()),
            "rejected" => assert!(result.is_err(), "{path} was accepted as a card"),
            other => panic!("unknown outcome {other}"),
        }
    }
}

#[test]
fn the_writer_is_deterministic_for_one_version() {
    let document = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let reference = card();
    let meta = meta_of(reference.meta());
    let grid = reference.grid().map(|grid| grid.side());

    let first = write_card(&document, &meta, &CardOptions::with_grid(grid)).expect("card");
    let second = write_card(&document, &meta, &CardOptions::with_grid(grid)).expect("card");
    assert_eq!(first, second);
    assert_eq!(first, load("valid/card-33.llk"));

    let no_grid = write_card(&document, &meta, &CardOptions::with_grid(None)).expect("card");
    assert_eq!(no_grid, load("valid/card-no-grid.llk"));
}

#[test]
fn the_experimental_card_vector_round_trips() {
    let document =
        LookDocument::decode(&load("valid/look-experimental.looklab")).expect("document");
    assert_eq!(card_status(&document), Some(CardStatus::Experimental));
    let reference = read_card(&load("valid/card-experimental.llk")).expect("experimental card");
    assert_eq!(reference.status(), CardStatus::Experimental);
    let written = write_card(
        &document,
        &meta_of(reference.meta()),
        &CardOptions::with_grid(reference.grid().map(|grid| grid.side())),
    )
    .expect("card");
    assert_eq!(written, load("valid/card-experimental.llk"));
}

#[test]
fn the_experimental_card_flags_probes_by_the_experimental_rule() {
    let card = read_card(&load("valid/card-experimental.llk")).expect("experimental card");
    assert!(
        card.fit().iter().any(|record| record.supported),
        "an experimental card carries usable FIT records"
    );
    let total: f64 = card
        .fit()
        .iter()
        .map(|record| f64::from(record.weight))
        .sum();
    assert!((total - 1.0).abs() < 1.0e-5, "weights sum to {total}");
    for record in card.fit() {
        assert!(record.supported);
        assert!(record.sigma <= 1.5);
    }
}

#[test]
fn a_card_status_must_match_the_target_evidence() {
    assert_eq!(card().status(), CardStatus::Qualified);
    let qualified = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    assert_eq!(card_status(&qualified), Some(CardStatus::Qualified));
    let meta = meta_of(card().meta());
    assert_eq!(
        write_card(
            &qualified,
            &meta.clone().with_status(CardStatus::Experimental),
            &CardOptions::default(),
        ),
        Err(CardError::Meta(
            "card status does not match the target's evidence"
        ))
    );
    let experimental =
        LookDocument::decode(&load("valid/look-experimental.looklab")).expect("document");
    assert_eq!(
        write_card(&experimental, &meta, &CardOptions::default()),
        Err(CardError::Meta(
            "card status does not match the target's evidence"
        ))
    );
}

#[test]
fn a_lut_defined_target_with_no_supported_probe_has_no_card_status() {
    let mut document =
        LookDocument::decode(&load("valid/look-experimental.looklab")).expect("document");
    document.file.look.kind = looklab_model::file::LookKind::LutDefined;
    assert_eq!(card_status(&document), None);
}

fn meta_of(meta: &looklab_export::card::Meta) -> CardMeta {
    let mut out = CardMeta::new(meta.name.clone(), meta.short_name.clone());
    if let Some(description) = &meta.description {
        out = out.with_description(description.clone());
    }
    if let Some(license) = &meta.license {
        out = out.with_license(license.clone());
    }
    out.with_status(meta.status)
}

#[test]
fn neutral_matches_direct_evaluation() {
    let document = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let card = card();
    assert_eq!(card.neutral().len(), 257);
    for (index, stored) in card.neutral().iter().enumerate() {
        let s = -12.0 + 18.0 * index as f64 / 256.0;
        let xyz = Illuminant::D65.xyz().scale(0.18 * libm::pow(2.0, s));
        let direct = output_rgb(&document, xyz);
        assert_eq!(*stored, direct, "neutral sample {index}");
    }
}

#[test]
fn fit_matches_direct_evaluation_and_weights() {
    let document = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let card = card();
    let probes = probes::v1();
    assert_eq!(card.fit().len(), probes.probes.len());
    let support = document
        .uncertainty
        .support
        .as_ref()
        .expect("published support");
    let sigma = document
        .uncertainty
        .probe_sigma_post
        .as_ref()
        .expect("published post-clip sigma");
    let mut total = 0.0_f32;
    let mut supported_count = 0;
    let mut unsupported_count = 0;
    for (index, (record, probe)) in card.fit().iter().zip(probes.probes.iter()).enumerate() {
        assert_eq!(record.probe as usize, index);
        let input = RgbSpace::Rec2020.xyz_to_rgb_linear(probe.xyz);
        let input = [input.r as f32, input.g as f32, input.b as f32];
        assert_eq!(record.input, input, "fit input {index}");
        let output = output_rgb(&document, probe.xyz);
        assert_eq!(record.output, output, "fit output {index}");

        let supported = support.values[index] == 1.0;
        assert_eq!(record.supported, supported, "fit support {index}");
        if supported {
            supported_count += 1;
            assert_eq!(record.sigma, sigma.values[index] as f32, "sigma {index}");
            total += record.weight;
        } else {
            unsupported_count += 1;
            assert_eq!(record.sigma, f32::INFINITY, "sigma {index}");
            assert_eq!(record.weight, 0.0);
        }
    }
    assert!(supported_count > 0 && unsupported_count > 0);
    assert!((total - 1.0).abs() < 1.0e-5, "weights sum to {total}");
}

#[test]
fn a_document_without_uncertainty_writes_unsupported_probes_with_an_infinite_sigma() {
    let document = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let bare = LookDocument::from_look(document.file.look.clone(), document.look.clone())
        .expect("document");
    assert!(bare.uncertainty.support.is_none());
    let meta = meta_of(card().meta());
    let bytes = write_card(&bare, &meta, &CardOptions::with_grid(None)).expect("card");
    let card = read_card(&bytes).expect("card reads");
    assert!(
        card.fit().iter().all(|record| !record.supported
            && record.weight == 0.0
            && record.sigma == f32::INFINITY)
    );
}

#[test]
fn grid_vectors_match_the_normative_interpolation() {
    let card = card();
    let vectors: serde_json::Value =
        serde_json::from_slice(&load("grid-vectors.json")).expect("vectors");
    assert_eq!(vectors["side"].as_u64(), Some(33));
    let vectors = vectors["vectors"].as_array().expect("vectors array");
    assert!(vectors.len() > 100);
    for vector in vectors {
        let input = triple(&vector["input"]);
        let expected = triple(&vector["output"]);
        let actual = card.eval_grid_shaped(input).expect("the card has a grid");
        for channel in 0..3 {
            assert_within_f16_ulp(actual[channel], expected[channel], 2.0);
        }
    }
}

#[test]
fn grid_nodes_are_the_direct_evaluation_at_the_shaped_coordinate() {
    // docs/04 §8.1 shaper inverse, with the constants it fixes.
    fn unshape(coordinate: f64) -> f64 {
        const PIVOT: f64 = 0.18;
        const HIGH_EV: f64 = 6.0;
        const LOW_EV: f64 = 12.0;
        let coordinate = coordinate.clamp(0.0, 1.0);
        if coordinate >= 0.5 {
            PIVOT
                * (libm::pow(
                    2.0,
                    (coordinate - 0.5) * 2.0 * libm::log2(1.0 + libm::pow(2.0, HIGH_EV)),
                ) - 1.0)
        } else {
            -PIVOT
                * (libm::pow(
                    2.0,
                    (0.5 - coordinate) * 2.0 * libm::log2(1.0 + libm::pow(2.0, LOW_EV)),
                ) - 1.0)
        }
    }

    let document = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let card = card();
    let grid = card.grid().expect("grid");
    assert_eq!(grid.side(), GridSide::S33);
    let side = grid.side().side() as usize;
    let scale = 1.0 / (side - 1) as f64;
    let coordinates = [0, 1, 4, 8, 12, 16, 20, 24, 28, 31, 32];
    for r in coordinates {
        for g in coordinates {
            for b in coordinates {
                let linear = Rgb::new(
                    unshape(r as f64 * scale),
                    unshape(g as f64 * scale),
                    unshape(b as f64 * scale),
                );
                let expected = output_rgb(&document, RgbSpace::Rec2020.rgb_linear_to_xyz(linear));
                let stored = grid.values()[r + side * (g + side * b)];
                for channel in 0..3 {
                    let value = if expected[channel].is_finite() {
                        expected[channel].clamp(-65504.0, 65504.0)
                    } else {
                        0.0
                    };
                    assert_eq!(
                        f16_bits(value),
                        f16_bits(stored[channel]),
                        "node ({r}, {g}, {b}) channel {channel}"
                    );
                }
            }
        }
    }
}

#[test]
fn an_unknown_section_is_ignored_and_a_missing_grid_yields_none() {
    let with_unknown = read_card(&load("valid/unknown-section.llk")).expect("unknown section");
    assert!(
        with_unknown
            .sections()
            .iter()
            .any(|section| section.kind == 77)
    );
    let no_grid = read_card(&load("valid/card-no-grid.llk")).expect("no grid");
    assert!(no_grid.grid().is_none());
    assert!(no_grid.eval_grid([0.18, 0.18, 0.18]).is_none());
}

fn triple(value: &serde_json::Value) -> [f64; 3] {
    let array = value.as_array().expect("triple");
    [
        array[0].as_f64().expect("number"),
        array[1].as_f64().expect("number"),
        array[2].as_f64().expect("number"),
    ]
}

fn assert_within_f16_ulp(actual: f64, expected: f64, ulps: f64) {
    let tolerance = ulps * f16_ulp(expected);
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} differs from {expected} by more than {ulps} f16 ulp"
    );
}

fn f16_ulp(value: f64) -> f64 {
    let bits = f16_bits(value as f32);
    let current = f64::from(f16_value(bits));
    let next = f64::from(f16_value(bits.wrapping_add(1)));
    let previous = f64::from(f16_value(bits.wrapping_sub(1)));
    let up = if next.is_finite() {
        next - current
    } else {
        current - previous
    };
    up.abs().max(current - previous).max(f64::MIN_POSITIVE)
}

fn output_rgb(document: &LookDocument, xyz: looklab_color::Xyz) -> [f32; 3] {
    let ucs = document.look.eval(xyz);
    let model = Cam16Model::new(&ViewingConditions::reference());
    let output = model.jmh_to_xyz(ucs_to_jmh(ucs, UcsSpace::Ucs));
    let rgb = RgbSpace::Rec2020.xyz_to_rgb_linear(output);
    [rgb.r as f32, rgb.g as f32, rgb.b as f32]
}

#[test]
fn a_grid_side_other_than_33_or_65_is_rejected() {
    assert!(matches!(
        read_card(&load("malformed/grid-side-34.llk")),
        Err(CardError::GridSide(34))
    ));
    assert_eq!(card().grid().map(|grid| grid.side()), Some(GridSide::S33));
}

#[test]
fn the_content_id_vector_reaches_content_id_verification() {
    assert!(matches!(
        read_card(&load("malformed/bad-content-id.llk")),
        Err(CardError::ContentId)
    ));
}

#[test]
fn a_non_ascii_description_is_rejected() {
    let mut bytes = load("valid/card-no-grid.llk");
    let meta = section(&bytes, 1);
    let mut cursor = meta.0;
    let end = meta.0 + meta.1;
    while cursor < end {
        let key = bytes[cursor];
        let length = u16::from_le_bytes([bytes[cursor + 1], bytes[cursor + 2]]) as usize;
        let start = cursor + 3;
        if key == 5 {
            assert!(length >= 2);
            bytes[start] = 0xc3;
            bytes[start + 1] = 0xa9;
            break;
        }
        cursor = start + length;
    }
    fix_section_crc(&mut bytes, 1);
    fix_content_id(&mut bytes);
    fix_header_crc(&mut bytes);
    assert!(matches!(
        read_card(&bytes),
        Err(CardError::Meta("description"))
    ));
}

fn section(bytes: &[u8], kind: u32) -> (usize, usize) {
    let count = usize::from(u16::from_le_bytes([bytes[12], bytes[13]]));
    for index in 0..count {
        let base = 64 + index * 16;
        if read_u32(bytes, base) == kind {
            return (
                read_u32(bytes, base + 4) as usize,
                read_u32(bytes, base + 8) as usize,
            );
        }
    }
    panic!("section {kind}");
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn table_end(bytes: &[u8]) -> usize {
    64 + 16 * usize::from(u16::from_le_bytes([bytes[12], bytes[13]]))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn fix_section_crc(bytes: &mut [u8], kind: u32) {
    let count = usize::from(u16::from_le_bytes([bytes[12], bytes[13]]));
    for index in 0..count {
        let base = 64 + index * 16;
        if read_u32(bytes, base) == kind {
            let offset = read_u32(bytes, base + 4) as usize;
            let length = read_u32(bytes, base + 8) as usize;
            write_u32(bytes, base + 12, crc32(&bytes[offset..offset + length]));
            return;
        }
    }
    panic!("section {kind}");
}

fn fix_content_id(bytes: &mut [u8]) {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(&bytes[table_end(bytes)..]);
    bytes[20..52].copy_from_slice(&digest);
}

fn fix_header_crc(bytes: &mut [u8]) {
    let mut crc = 0xffff_ffff_u32;
    let mut update = |data: &[u8]| {
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    };
    update(&bytes[..16]);
    update(&[0, 0, 0, 0]);
    update(&bytes[20..table_end(bytes)]);
    write_u32(bytes, 16, !crc);
}

#[test]
fn a_covariance_only_sigma_is_the_square_root_of_the_trace() {
    let source = LookDocument::decode(&load("valid/look.looklab")).expect("document");
    let mut document =
        LookDocument::from_look(source.file.look.clone(), source.look.clone()).expect("document");
    let count = probes::V1_PROBE_COUNT;
    let mut values = vec![0.0_f64; count * 9];
    for probe in 0..count {
        values[probe * 9] = 4.0;
        values[probe * 9 + 4] = 4.0;
        values[probe * 9 + 8] = 4.0;
    }
    document = document
        .with_uncertainty(UncertaintyData {
            support: Some(ProbeScalars::new(vec![1.0; count]).expect("support")),
            probe_cov_post: Some(ProbeCovariance::new(count, values).expect("covariance")),
            ..UncertaintyData::default()
        })
        .expect("uncertainty");
    let meta = meta_of(card().meta());
    let bytes = write_card(&document, &meta, &CardOptions::with_grid(None)).expect("card");
    let card = read_card(&bytes).expect("card reads");
    let expected = 12.0_f64.sqrt() as f32;
    assert!(
        card.fit()
            .iter()
            .all(|record| record.supported && record.sigma == expected)
    );
}
