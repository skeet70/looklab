// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local file reading and download ([docs/06 §7](../../../docs/06-privacy-and-payload.md)).

use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, HtmlInputElement, Url};

#[derive(Clone, Debug, PartialEq)]
pub struct LocalFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// # Errors
/// Reports a file the browser cannot read.
pub async fn read_input(input: &HtmlInputElement) -> Result<Vec<LocalFile>, String> {
    let Some(list) = input.files() else {
        return Ok(Vec::new());
    };
    let mut files = Vec::with_capacity(list.length() as usize);
    for index in 0..list.length() {
        let Some(file) = list.get(index) else {
            continue;
        };
        let buffer = JsFuture::from(file.array_buffer())
            .await
            .map_err(|error| format!("{}: {error:?}", file.name()))?;
        let bytes = Uint8Array::new(&buffer).to_vec();
        files.push(LocalFile {
            name: file.name(),
            bytes,
        });
    }
    Ok(files)
}

/// # Errors
/// Reports a browser that cannot create a download.
pub fn download(name: &str, bytes: &[u8], media_type: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_owned())?;
    let document = window.document().ok_or_else(|| "no document".to_owned())?;
    let parts = js_sys::Array::new();
    parts.push(&Uint8Array::from(bytes));
    let options = BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .map_err(|error| format!("{error:?}"))?;
    let url = Url::create_object_url_with_blob(&blob).map_err(|error| format!("{error:?}"))?;
    let anchor = document
        .create_element("a")
        .map_err(|error| format!("{error:?}"))?
        .dyn_into::<HtmlAnchorElement>()
        .map_err(|_| "not an anchor".to_owned())?;
    anchor.set_href(&url);
    anchor.set_download(name);
    anchor.click();
    Url::revoke_object_url(&url).map_err(|error| format!("{error:?}"))
}
