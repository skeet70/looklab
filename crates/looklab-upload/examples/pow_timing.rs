// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(clippy::disallowed_types)]

use std::time::Instant;

use looklab_upload::{Challenge, ManifestHash, Nonce, PowParams, solve, verify};

fn main() {
    let params = PowParams::default();
    let challenge = Challenge::new(
        &Nonce::from_bytes([0; 32]),
        &ManifestHash::from_bytes([0; 32]),
    );
    for images in [1u32, 10] {
        let start = Instant::now();
        let solutions = solve(&challenge, images, &params, 0).expect("default params solve");
        let elapsed = start.elapsed();
        verify(&challenge, images, &params, &solutions).expect("fresh solutions verify");
        println!("{images} image(s): solved in {elapsed:?}");
    }
}
