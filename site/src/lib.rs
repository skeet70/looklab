// SPDX-License-Identifier: AGPL-3.0-or-later

#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    looklab_web::mount_with_root(env!("LOOKLAB_SITE_ROOT"));
}
