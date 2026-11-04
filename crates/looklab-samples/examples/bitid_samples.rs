// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native-vs-wasm bit-identity workload for the sample blob, read by
//! `cargo xtask bitid`.
//!
//! Prints `<name> <hex FNV-1a-64>` per line: `encode` over the encoded bytes,
//! `decode` over the decoded column bit patterns.

use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, RerenderGroupId,
    SampleFlags, SampleHeader, SampleSet, decode, encode,
};

const N: usize = 4096;

fn fnv1a64(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn push_f32(hash: &mut u64, value: f32) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn xorshift(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn workload() -> SampleSet {
    let mut state: u32 = 0x1234_5678;
    let mut unit = || (xorshift(&mut state) >> 8) as f32 / ((1u32 << 24) as f32);
    let in_rgb = (0..N)
        .map(|_| [unit() * 2.0, unit(), unit() * 0.25])
        .collect();
    let out_rgb = (0..N).map(|_| [unit(), unit(), unit()]).collect();
    let density_w = (0..N).map(|_| 0.5 + unit()).collect();
    let flags = (0..N)
        .map(|i| SampleFlags::from_bits((i % 8) as u8).expect("0..=7 is a valid flag set"))
        .collect();
    let header = SampleHeader {
        n: N as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.75, 1.0, 1.25],
        cct_hint_k: 5200.0,
        exposure_hint_ev: -0.33,
        rerender_group: Some(RerenderGroupId::new([9; 16])),
        prep_version: PrepVersion::new(1),
        prior_transform_version: PriorTransformVersion::new(2),
        prep_params_hash: PrepParamsHash::new([4; 32]),
    };
    SampleSet::new(header, in_rgb, out_rgb, density_w, flags).expect("workload columns are valid")
}

fn main() {
    let set = workload();
    let bytes = encode(&set).expect("workload encodes");
    println!("encode {:016x}", fnv1a64(bytes.iter().copied()));

    let decoded = decode(&bytes).expect("encoded workload decodes");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for rgb in decoded.in_rgb().iter().chain(decoded.out_rgb()) {
        for value in rgb {
            push_f32(&mut hash, *value);
        }
    }
    for value in decoded.density_w() {
        push_f32(&mut hash, *value);
    }
    for flag in decoded.flags() {
        hash ^= u64::from(flag.bits());
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    println!("decode {hash:016x}");
}
