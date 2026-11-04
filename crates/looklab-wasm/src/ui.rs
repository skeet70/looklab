// SPDX-License-Identifier: AGPL-3.0-or-later
//! JavaScript entry points for clients that are not the Leptos app.

use wasm_bindgen::prelude::*;

use crate::match_api::{MatchRequest, run_match};

#[wasm_bindgen]
pub fn facade_version() -> String {
    serde_json::json!({
        "format": "looklab-wasm-facade/1",
        "module_abi_version": crate::MODULE_ABI_VERSION,
        "optimizer": looklab_match::OPTIMIZER_VERSION,
    })
    .to_string()
}

/// RGBA pixels for a seeded phone pattern.
///
/// # Errors
/// Reports a seed or a size the pattern layout rejects.
#[wasm_bindgen]
pub fn pattern_rgba(seed: &str, width: u32, height: u32) -> Result<Vec<u8>, JsError> {
    crate::render_pattern(seed, width, height)
        .map(|image| image.rgba)
        .map_err(|error| JsError::new(&error.to_string()))
}

/// Pairing plan as JSON; `files` is `[name, Uint8Array]` pairs.
///
/// # Errors
/// Reports a file name or a payload the pairing code rejects.
#[wasm_bindgen]
pub fn pair_files_json(files: &js_sys::Array) -> Result<String, JsError> {
    let mut input = Vec::with_capacity(files.length() as usize);
    for value in files.iter() {
        let pair = value
            .dyn_into::<js_sys::Array>()
            .map_err(|_| JsError::new("each file is a [name, bytes] pair"))?;
        let name = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsError::new("the file name is a string"))?;
        let bytes = pair
            .get(1)
            .dyn_into::<js_sys::Uint8Array>()
            .map_err(|_| JsError::new("the file contents are a Uint8Array"))?
            .to_vec();
        input.push((name, bytes));
    }
    let plan = crate::inspect_files(&input).map_err(|error| JsError::new(&error.to_string()))?;
    serde_json::to_string(&plan).map_err(|error| JsError::new(&error.to_string()))
}

/// A match reply as JSON, from a postcard [`MatchRequest`].
///
/// # Errors
/// Reports a malformed request or a match failure.
#[wasm_bindgen]
pub fn match_reply_json(request: &[u8]) -> Result<String, JsError> {
    let request: MatchRequest =
        postcard::from_bytes(request).map_err(|error| JsError::new(&error.to_string()))?;
    let reply = run_match(&request).map_err(|error| JsError::new(&error.to_string()))?;
    serde_json::to_string(&reply).map_err(|error| JsError::new(&error.to_string()))
}
