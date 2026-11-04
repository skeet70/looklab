// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native vs wasm32 bit-identity workload for the colour-space basics.
//!
//! Prints one `<function> <hex FNV-1a-64>` line per function over 10^5
//! fixed-seed inputs, plus one line per chart table. `cargo xtask bitid`
//! compares the native and `wasm32-wasip1` runs.

use looklab_color::Xyy;
use looklab_color::cat::{Cat, adapt, adaptation_matrix};
use looklab_color::chart::{colorchecker24, colorchecker24_d65};
use looklab_color::ciede2000::delta_e_ciede2000;
use looklab_color::lab::{Lab, lab_to_lchab, lab_to_xyz, lchab_to_lab, xyz_to_lab};
use looklab_color::spaces::RgbSpace;
use looklab_color::trc::{
    adobe_rgb_decode, adobe_rgb_encode, prophoto_decode, prophoto_encode, rec2020_decode,
    rec2020_encode, srgb_decode, srgb_encode,
};
use looklab_color::types::{Rgb, Xyz};
use looklab_color::xyy::{xy_to_xyz, xyy_to_xyz, xyz_to_xyy};

const SAMPLES: usize = 100_000;

type TransferFunction = (&'static str, fn(f64) -> f64);

struct Fnv1a(u64);

impl Fnv1a {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn push(&mut self, value: f64) {
        self.push_bytes(&value.to_le_bytes());
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self, low: f64, high: f64) -> f64 {
        const UNIT: f64 = 1.0 / 9_007_199_254_740_992.0;
        let unit = ((self.next_u64() >> 11) as f64) * UNIT;
        low + (high - low) * unit
    }
}

fn xyz(rng: &mut SplitMix64) -> Xyz {
    Xyz::new(
        rng.next_f64(-0.25, 2.0),
        rng.next_f64(-0.25, 2.0),
        rng.next_f64(-0.25, 2.0),
    )
}

fn rgb(rng: &mut SplitMix64) -> Rgb {
    Rgb::new(
        rng.next_f64(-0.5, 1.5),
        rng.next_f64(-0.5, 1.5),
        rng.next_f64(-0.5, 1.5),
    )
}

fn lab(rng: &mut SplitMix64) -> Lab {
    Lab::new(
        rng.next_f64(-20.0, 120.0),
        rng.next_f64(-150.0, 150.0),
        rng.next_f64(-150.0, 150.0),
    )
}

fn white(rng: &mut SplitMix64) -> Xyz {
    Xyz::new(
        rng.next_f64(0.5, 1.5),
        rng.next_f64(0.5, 1.5),
        rng.next_f64(0.5, 1.5),
    )
}

macro_rules! digest {
    ($name:expr, $seed:expr, |$rng:ident, $hash:ident| $body:block) => {{
        let mut $rng = SplitMix64::new($seed);
        let mut $hash = Fnv1a::new();
        for _ in 0..SAMPLES $body
        println!("{} {:016x}", $name, $hash.0);
    }};
}

fn space_digests() {
    for space in RgbSpace::ALL {
        let name = format!("{space:?}");
        digest!(format!("rgb_to_xyz_{name}"), 100, |rng, hash| {
            let out = space.rgb_linear_to_xyz(rgb(&mut rng));
            hash.push(out.x);
            hash.push(out.y);
            hash.push(out.z);
        });
        digest!(format!("xyz_to_rgb_{name}"), 200, |rng, hash| {
            let out = space.xyz_to_rgb_linear(xyz(&mut rng));
            hash.push(out.r);
            hash.push(out.g);
            hash.push(out.b);
        });
        digest!(format!("rgb_encoded_to_xyz_{name}"), 600, |rng, hash| {
            let encoded = Rgb::new(
                rng.next_f64(-0.25, 1.5),
                rng.next_f64(-0.25, 1.5),
                rng.next_f64(-0.25, 1.5),
            );
            let out = space.rgb_encoded_to_xyz(encoded);
            hash.push(out.x);
            hash.push(out.y);
            hash.push(out.z);
        });
        digest!(format!("xyz_to_rgb_encoded_{name}"), 700, |rng, hash| {
            let out = space.xyz_to_rgb_encoded(xyz(&mut rng));
            hash.push(out.r);
            hash.push(out.g);
            hash.push(out.b);
        });
    }
}

fn cat_digests() {
    for cat in [Cat::Cat02, Cat::Cat16, Cat::Bradford] {
        let name = format!("{cat:?}");
        digest!(format!("adaptation_matrix_{name}"), 300, |rng, hash| {
            let matrix = adaptation_matrix(cat, white(&mut rng), white(&mut rng));
            for row in matrix.0 {
                for value in row {
                    hash.push(value);
                }
            }
        });
        digest!(format!("adapt_{name}"), 400, |rng, hash| {
            let out = adapt(xyz(&mut rng), cat, white(&mut rng), white(&mut rng));
            hash.push(out.x);
            hash.push(out.y);
            hash.push(out.z);
        });
    }
}

fn chart_digests() {
    let mut hash = Fnv1a::new();
    for patch in colorchecker24() {
        hash.push_bytes(patch.name.as_bytes());
        hash.push(patch.xyz.x);
        hash.push(patch.xyz.y);
        hash.push(patch.xyz.z);
    }
    println!("colorchecker24 {:016x}", hash.0);

    let mut hash = Fnv1a::new();
    for patch in colorchecker24_d65() {
        hash.push_bytes(patch.name.as_bytes());
        hash.push(patch.xyz.x);
        hash.push(patch.xyz.y);
        hash.push(patch.xyz.z);
    }
    println!("colorchecker24_d65 {:016x}", hash.0);
}

fn main() {
    digest!("xyz_to_xyy", 1, |rng, hash| {
        let out = xyz_to_xyy(xyz(&mut rng));
        hash.push(out.x);
        hash.push(out.y);
        hash.push(out.big_y);
    });
    digest!("xyy_to_xyz", 2, |rng, hash| {
        let out = xyy_to_xyz(Xyy {
            x: rng.next_f64(-0.1, 1.1),
            y: rng.next_f64(-0.1, 1.1),
            big_y: rng.next_f64(-0.5, 1.5),
        });
        hash.push(out.x);
        hash.push(out.y);
        hash.push(out.z);
    });
    digest!("xy_to_xyz", 3, |rng, hash| {
        let out = xy_to_xyz((rng.next_f64(-0.1, 1.1), rng.next_f64(-0.1, 1.1)));
        hash.push(out.x);
        hash.push(out.y);
        hash.push(out.z);
    });

    let transfer_functions: [TransferFunction; 8] = [
        ("srgb_encode", srgb_encode),
        ("srgb_decode", srgb_decode),
        ("adobe_rgb_encode", adobe_rgb_encode),
        ("adobe_rgb_decode", adobe_rgb_decode),
        ("rec2020_encode", rec2020_encode),
        ("rec2020_decode", rec2020_decode),
        ("prophoto_encode", prophoto_encode),
        ("prophoto_decode", prophoto_decode),
    ];
    for (index, (name, function)) in transfer_functions.into_iter().enumerate() {
        let seed = 10 + index as u64;
        digest!(name, seed, |rng, hash| {
            hash.push(function(rng.next_f64(-0.5, 1.5)));
        });
    }

    space_digests();
    cat_digests();
    chart_digests();

    digest!("xyz_to_lab", 500, |rng, hash| {
        let out = xyz_to_lab(xyz(&mut rng), white(&mut rng));
        hash.push(out.l);
        hash.push(out.a);
        hash.push(out.b);
    });
    digest!("lab_to_xyz", 501, |rng, hash| {
        let out = lab_to_xyz(lab(&mut rng), white(&mut rng));
        hash.push(out.x);
        hash.push(out.y);
        hash.push(out.z);
    });
    digest!("lab_to_lchab", 502, |rng, hash| {
        let out = lab_to_lchab(lab(&mut rng));
        hash.push(out.l);
        hash.push(out.c);
        hash.push(out.h);
    });
    digest!("lchab_to_lab", 503, |rng, hash| {
        let out = lchab_to_lab(looklab_color::Lchab::new(
            rng.next_f64(-20.0, 120.0),
            rng.next_f64(0.0, 150.0),
            rng.next_f64(-720.0, 720.0),
        ));
        hash.push(out.l);
        hash.push(out.a);
        hash.push(out.b);
    });
    digest!("delta_e_ciede2000", 504, |rng, hash| {
        hash.push(delta_e_ciede2000(lab(&mut rng), lab(&mut rng)));
    });
}
