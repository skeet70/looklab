// SPDX-License-Identifier: AGPL-3.0-or-later
//! Blob builders for the integration tests; the container is written by hand
//! so tests can produce inputs `encode` never emits.
#![allow(dead_code)]

use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use ruzstd::encoding::{CompressionLevel, FrameCompressor};

pub fn compress(raw: &[u8]) -> Vec<u8> {
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_source(raw);
    let mut out = Vec::new();
    compressor.set_drain(&mut out);
    compressor.compress();
    out
}

pub fn header(n: u32) -> SampleHeader {
    SampleHeader {
        n,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.5, 1.0, 1.2],
        cct_hint_k: 5200.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(1),
        prior_transform_version: PriorTransformVersion::new(3),
        prep_params_hash: PrepParamsHash::new([7; 32]),
    }
}

pub fn set(n: usize) -> SampleSet {
    let in_rgb = (0..n)
        .map(|i| {
            [
                (i % 251) as f32 * 0.25,
                (i % 97) as f32 * 0.01,
                (i % 13) as f32 * 0.05,
            ]
        })
        .collect();
    let out_rgb = (0..n)
        .map(|i| {
            [
                (i % 89) as f32 * 0.01,
                (i % 41) as f32 * 0.02,
                (i % 67) as f32 * 0.01,
            ]
        })
        .collect();
    let density_w = vec![0.5; n];
    let flags = (0..n)
        .map(|i| SampleFlags::from_bits((i % 8) as u8).expect("0..=7 is a valid flag set"))
        .collect();
    SampleSet::new(header(n as u32), in_rgb, out_rgb, density_w, flags)
        .expect("hand-built columns are valid")
}

pub fn raw_body(
    in_rgb: &[[f32; 3]],
    out_rgb: &[[f32; 3]],
    density_w: &[f32],
    flags: &[u8],
) -> Vec<u8> {
    let mut raw = Vec::new();
    for rgb in in_rgb.iter().chain(out_rgb) {
        for value in rgb {
            raw.extend_from_slice(&value.to_le_bytes());
        }
    }
    for w in density_w {
        raw.extend_from_slice(&w.to_le_bytes());
    }
    raw.extend_from_slice(flags);
    raw
}

pub fn valid_row(seed: f32) -> Vec<u8> {
    raw_body(&[[seed, 0.5, 0.25]], &[[0.2, 0.6, seed]], &[1.0], &[0])
}

pub fn container(magic: [u8; 6], version: u16, header_bytes: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(body);
    out
}

pub fn sample_header_bytes(
    n: u32,
    input_space: u32,
    output_space: u32,
    wb_multipliers: [f32; 3],
    cct_hint_k: f32,
    exposure_hint_ev: f32,
) -> Vec<u8> {
    postcard::to_allocvec(&(
        n,
        input_space,
        output_space,
        wb_multipliers,
        cct_hint_k,
        exposure_hint_ev,
        Option::<[u8; 16]>::None,
        1u32,
        1u32,
        [0u8; 32],
    ))
    .expect("postcard encodes a fixed tuple")
}

pub fn default_header_bytes(n: u32) -> Vec<u8> {
    sample_header_bytes(n, 0, 0, [1.0, 1.0, 1.0], 5000.0, 0.0)
}

pub fn sample_blob(n: u32, raw: &[u8]) -> Vec<u8> {
    container(
        looklab_samples::SAMPLE_MAGIC,
        looklab_samples::SAMPLE_VERSION,
        &default_header_bytes(n),
        &compress(raw),
    )
}
