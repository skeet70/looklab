// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native/wasm bit-identity workload for CAM16 and CAM16-UCS.
//!
//! Prints one line per function: `<function> <hex FNV-1a-64 of the output f64 bit
//! patterns in order>`. `cargo xtask bitid` compares the native run with a
//! `wasm32-wasip1` run under wasmtime.

use looklab_color::Xyz;
use looklab_color::cam16::{Cam16Model, Chroma, Jmh, Surround, ViewingConditions};
use looklab_color::ucs::{UcsSpace, delta_e_lcd, delta_e_scd, delta_e_ucs, jmh_to_ucs, ucs_to_jmh};

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

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct Lines {
    xyz_to_cam16: Fnv,
    xyz_to_cam16_dim: Fnv,
    cam16_to_xyz: Fnv,
    cam16_to_xyz_dim: Fnv,
    jmh_to_ucs_ucs: Fnv,
    jmh_to_ucs_lcd: Fnv,
    jmh_to_ucs_scd: Fnv,
    ucs_to_jmh_ucs: Fnv,
    ucs_to_jmh_lcd: Fnv,
    ucs_to_jmh_scd: Fnv,
    delta_e_ucs: Fnv,
    delta_e_lcd: Fnv,
    delta_e_scd: Fnv,
    ucs_j_and_dj_dy: Fnv,
}

impl Lines {
    fn print(&self) {
        println!("xyz_to_cam16 {:016x}", self.xyz_to_cam16.finish());
        println!("xyz_to_cam16_dim {:016x}", self.xyz_to_cam16_dim.finish());
        println!("cam16_to_xyz {:016x}", self.cam16_to_xyz.finish());
        println!("cam16_to_xyz_dim {:016x}", self.cam16_to_xyz_dim.finish());
        println!("jmh_to_ucs_ucs {:016x}", self.jmh_to_ucs_ucs.finish());
        println!("jmh_to_ucs_lcd {:016x}", self.jmh_to_ucs_lcd.finish());
        println!("jmh_to_ucs_scd {:016x}", self.jmh_to_ucs_scd.finish());
        println!("ucs_to_jmh_ucs {:016x}", self.ucs_to_jmh_ucs.finish());
        println!("ucs_to_jmh_lcd {:016x}", self.ucs_to_jmh_lcd.finish());
        println!("ucs_to_jmh_scd {:016x}", self.ucs_to_jmh_scd.finish());
        println!("delta_e_ucs {:016x}", self.delta_e_ucs.finish());
        println!("delta_e_lcd {:016x}", self.delta_e_lcd.finish());
        println!("delta_e_scd {:016x}", self.delta_e_scd.finish());
        println!("ucs_j_and_dj_dy {:016x}", self.ucs_j_and_dj_dy.finish());
    }
}

fn mix_ucs_space(
    space: UcsSpace,
    jmh: Jmh,
    other: Jmh,
    forward: &mut Fnv,
    inverse: &mut Fnv,
    difference: &mut Fnv,
) {
    let point = jmh_to_ucs(jmh, space);
    for value in point.to_array() {
        forward.mix(value);
    }
    let back = ucs_to_jmh(point, space);
    for value in [back.j, back.m, back.h] {
        inverse.mix(value);
    }
    let second = jmh_to_ucs(other, space);
    let delta = match space {
        UcsSpace::Ucs => delta_e_ucs(point, second),
        UcsSpace::Lcd => delta_e_lcd(point, second),
        UcsSpace::Scd => delta_e_scd(point, second),
    };
    difference.mix(delta);
}

fn main() {
    let reference = Cam16Model::new(&ViewingConditions::reference());
    let dim = Cam16Model::new(&ViewingConditions {
        l_a: 100.0,
        y_b: 20.0,
        surround: Surround::Dim,
        discount_illuminant: false,
        ..ViewingConditions::reference()
    });
    let mut lines = Lines::default();
    let mut rng = SplitMix64(0x5eed_ca16_0000_0001);

    for _ in 0..SAMPLES {
        // Kept above the negative achromatic response of blue-dominant stimuli.
        let xyz = Xyz::new(
            rng.range(0.2, 1.2),
            rng.range(0.2, 1.2),
            rng.range(0.2, 1.2),
        );
        let cam = reference.xyz_to_cam16(xyz);
        for value in [cam.j, cam.c, cam.h, cam.m, cam.s, cam.q] {
            lines.xyz_to_cam16.mix(value);
        }
        let cam_dim = dim.xyz_to_cam16(xyz);
        for value in [
            cam_dim.j, cam_dim.c, cam_dim.h, cam_dim.m, cam_dim.s, cam_dim.q,
        ] {
            lines.xyz_to_cam16_dim.mix(value);
        }

        let j = rng.range(0.5, 99.0);
        let m = rng.range(0.0, 90.0);
        let c = rng.range(0.0, 90.0);
        let h = rng.range(0.0, 360.0);
        for value in reference.cam16_to_xyz(j, Chroma::M(m), h).to_array() {
            lines.cam16_to_xyz.mix(value);
        }
        for value in reference.cam16_to_xyz(j, Chroma::C(c), h).to_array() {
            lines.cam16_to_xyz.mix(value);
        }
        for value in dim.cam16_to_xyz(j, Chroma::M(m), h).to_array() {
            lines.cam16_to_xyz_dim.mix(value);
        }
        for value in dim.cam16_to_xyz(j, Chroma::C(c), h).to_array() {
            lines.cam16_to_xyz_dim.mix(value);
        }

        let jmh = Jmh { j, m, h };
        let other = Jmh {
            j: rng.range(0.5, 99.0),
            m: rng.range(0.0, 90.0),
            h: rng.range(0.0, 360.0),
        };
        mix_ucs_space(
            UcsSpace::Ucs,
            jmh,
            other,
            &mut lines.jmh_to_ucs_ucs,
            &mut lines.ucs_to_jmh_ucs,
            &mut lines.delta_e_ucs,
        );
        mix_ucs_space(
            UcsSpace::Lcd,
            jmh,
            other,
            &mut lines.jmh_to_ucs_lcd,
            &mut lines.ucs_to_jmh_lcd,
            &mut lines.delta_e_lcd,
        );
        mix_ucs_space(
            UcsSpace::Scd,
            jmh,
            other,
            &mut lines.jmh_to_ucs_scd,
            &mut lines.ucs_to_jmh_scd,
            &mut lines.delta_e_scd,
        );

        let chromaticity = Xyz::new(xyz.x / xyz.y, 1.0, xyz.z / xyz.y);
        let (j_prime, slope) = reference.ucs_j_and_dj_dy(chromaticity, rng.range(0.01, 1.5));
        lines.ucs_j_and_dj_dy.mix(j_prime);
        lines.ucs_j_and_dj_dy.mix(slope);
    }

    lines.print();
}
