// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native/wasm bit-identity workload for CAM16 and CAM16-UCS.
//!
//! Prints one line per function: `<function> <hex FNV-1a-64 of the output f64 bit
//! patterns in order>`. `cargo xtask bitid` compares the native run with a
//! `wasm32-wasip1` run under wasmtime.

use looklab_color::Xyz;
use looklab_color::cam16::{Cam16Model, Chroma, Jmh, ViewingConditions};
use looklab_color::ucs::{UcsSpace, jmh_to_ucs, ucs_to_jmh};

const SAMPLES: usize = 100_000;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn mix(&mut self, value: f64) {
        for byte in value.to_bits().to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn main() {
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut rng = SplitMix64(0x5eed_ca16_0000_0001);

    let mut forward = Fnv::new();
    let mut inverse = Fnv::new();
    let mut ucs = Fnv::new();
    let mut ucs_inverse = Fnv::new();
    let mut derivative = Fnv::new();

    for _ in 0..SAMPLES {
        let xyz = Xyz::new(
            rng.range(0.001, 1.2),
            rng.range(0.001, 1.2),
            rng.range(0.001, 1.2),
        );
        let cam = model.xyz_to_cam16(xyz);
        for value in [cam.j, cam.c, cam.h, cam.m, cam.s, cam.q] {
            forward.mix(value);
        }

        let j = rng.range(0.5, 99.0);
        let m = rng.range(0.0, 90.0);
        let c = rng.range(0.0, 90.0);
        let h = rng.range(0.0, 360.0);
        for value in model.cam16_to_xyz(j, Chroma::M(m), h).to_array() {
            inverse.mix(value);
        }
        for value in model.cam16_to_xyz(j, Chroma::C(c), h).to_array() {
            inverse.mix(value);
        }

        let jmh = Jmh { j, m, h };
        for space in [UcsSpace::Ucs, UcsSpace::Lcd, UcsSpace::Scd] {
            let point = jmh_to_ucs(jmh, space);
            for value in point.to_array() {
                ucs.mix(value);
            }
            let back = ucs_to_jmh(point, space);
            for value in [back.j, back.m, back.h] {
                ucs_inverse.mix(value);
            }
        }

        let chromaticity = Xyz::new(xyz.x / xyz.y, 1.0, xyz.z / xyz.y);
        let (j_prime, slope) = model.ucs_j_and_dj_dy(chromaticity, rng.range(0.01, 1.5));
        derivative.mix(j_prime);
        derivative.mix(slope);
    }

    println!("xyz_to_cam16 {:016x}", forward.finish());
    println!("cam16_to_xyz {:016x}", inverse.finish());
    println!("jmh_to_ucs {:016x}", ucs.finish());
    println!("ucs_to_jmh {:016x}", ucs_inverse.finish());
    println!("ucs_j_and_dj_dy {:016x}", derivative.finish());
}
