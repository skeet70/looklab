// SPDX-License-Identifier: AGPL-3.0-or-later
//! Node wasm32 check that the committed fixtures match the native digests.
//!
//!     cargo test -p looklab-raw --target wasm32-unknown-unknown

#![cfg(target_arch = "wasm32")]

use looklab_raw::{RawDecoder, RawlerDecoder, digest_f32};
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test]
fn fujifilm_x_s10_matches_native_digest() {
    let bytes = include_bytes!("../../../fixtures/fujifilm-x-s10.raf");
    let img = RawlerDecoder.decode(bytes).expect("decode RAF");
    assert_eq!((img.width, img.height), (1042, 696));
    assert_eq!(digest_f32(&img.rgb), 0x14b1_973f_d0fd_c8ac);
}

#[wasm_bindgen_test]
fn canon_eos_60d_matches_native_digest() {
    let bytes = include_bytes!("../../../fixtures/canon-eos-60d.dng");
    let img = RawlerDecoder.decode(bytes).expect("decode DNG");
    assert_eq!((img.width, img.height), (480, 270));
    assert_eq!(digest_f32(&img.rgb), 0xc62f_5221_0dca_b14d);
}
