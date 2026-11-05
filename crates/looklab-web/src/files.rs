// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local file reading and download ([docs/06 §7](../../../docs/06-privacy-and-payload.md)).

use js_sys::Uint8Array;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, HtmlInputElement, Url};

#[derive(Clone, Debug, PartialEq)]
pub struct LocalFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct DraftFileKey(u32);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DraftFile {
    pub name: String,
    pub key: DraftFileKey,
}

#[wasm_bindgen(inline_js = r#"
export class ContributionDraftStore {
    constructor() {
        this.queue = Promise.resolve();
        this.revision = 0;
        this.pending = null;
        this.timer = null;
        this.database = new Promise((resolve, reject) => {
            const request = indexedDB.open("looklab-contribution-drafts", 1);
            request.onupgradeneeded = () => request.result.createObjectStore("entries");
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
        });
        this.database.catch(() => {});
    }
    enqueue(operation) {
        const result = this.queue.then(operation);
        this.queue = result.catch(() => {});
        return result;
    }
    async transaction(mode, operation) {
        const database = await this.database;
        return new Promise((resolve, reject) => {
            const transaction = database.transaction("entries", mode);
            const request = operation(transaction.objectStore("entries"));
            let value;
            if (request) request.onsuccess = () => { value = request.result; };
            transaction.oncomplete = () => resolve(value);
            transaction.onerror = () => reject(transaction.error);
            transaction.onabort = () => reject(transaction.error);
        });
    }
    cancelPending() {
        this.revision += 1;
        clearTimeout(this.timer);
        this.pending = null;
    }
    replaceFiles(files) {
        this.cancelPending();
        return this.enqueue(() => this.transaction("readwrite", store => {
            store.clear();
            for (const [key, bytes] of files) store.put(bytes, ["file", key]);
        }));
    }
    schedule(text) {
        const revision = ++this.revision;
        this.pending = { text, revision };
        clearTimeout(this.timer);
        this.timer = setTimeout(() => this.flush(), 300);
    }
    flush() {
        clearTimeout(this.timer);
        const pending = this.pending;
        this.pending = null;
        if (!pending) return;
        this.enqueue(() => {
            if (pending.revision !== this.revision) return;
            return this.transaction("readwrite", store => store.put(pending.text, "draft"));
        }).catch(() => {});
    }
    prepared(text) {
        return this.enqueue(() => this.transaction("readwrite", store => store.put(text, "prepared")));
    }
    read(key) {
        return this.enqueue(() => this.transaction("readonly", store => store.get(key)));
    }
    settings() { return this.read("draft"); }
    samples() { return this.read("prepared"); }
    file(key) { return this.read(["file", key]); }
    clear() {
        this.cancelPending();
        return this.enqueue(() => this.transaction("readwrite", store => store.clear()));
    }
}
"#)]
extern "C" {
    pub(crate) type ContributionDraftStore;
    #[wasm_bindgen(constructor)]
    pub(crate) fn new() -> ContributionDraftStore;
    #[wasm_bindgen(method, js_name = replaceFiles)]
    pub(crate) fn replace_files(this: &ContributionDraftStore, files: &JsValue) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    pub(crate) fn schedule(this: &ContributionDraftStore, text: &str);
    #[wasm_bindgen(method)]
    pub(crate) fn flush(this: &ContributionDraftStore);
    #[wasm_bindgen(method)]
    pub(crate) fn prepared(this: &ContributionDraftStore, text: &str) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    pub(crate) fn settings(this: &ContributionDraftStore) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    pub(crate) fn samples(this: &ContributionDraftStore) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    pub(crate) fn file(this: &ContributionDraftStore, key: u32) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    pub(crate) fn clear(this: &ContributionDraftStore) -> js_sys::Promise;
}

impl ContributionDraftStore {
    pub(crate) fn stored_file(&self, file: &DraftFile) -> js_sys::Promise {
        self.file(file.key.0)
    }
}

pub(crate) fn draft_file_entries(
    files: &[LocalFile],
) -> Result<(Vec<DraftFile>, js_sys::Array), String> {
    let entries = js_sys::Array::new();
    let references = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let key =
                u32::try_from(index).map_err(|_| "Too many files for a local draft.".to_owned())?;
            let entry = js_sys::Array::new();
            entry.push(&JsValue::from(key));
            entry.push(&Uint8Array::from(file.bytes.as_slice()));
            entries.push(&entry);
            Ok(DraftFile {
                name: file.name.clone(),
                key: DraftFileKey(key),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((references, entries))
}

pub(crate) async fn read_draft_file(
    pending: js_sys::Promise,
    file: &DraftFile,
) -> Result<LocalFile, String> {
    let bytes = JsFuture::from(pending)
        .await
        .map_err(|error| format!("{error:?}"))?;
    if bytes.is_undefined() {
        return Err(
            "The local draft's files are no longer available. Add your originals again.".to_owned(),
        );
    }
    Ok(LocalFile {
        name: file.name.clone(),
        bytes: Uint8Array::new(&bytes).to_vec(),
    })
}

/// # Errors
/// Reports a file the browser cannot read.
pub async fn read_input(input: &HtmlInputElement) -> Result<Vec<LocalFile>, String> {
    let Some(list) = input.files() else {
        return Ok(Vec::new());
    };
    read_files(
        (0..list.length())
            .filter_map(|index| list.get(index))
            .collect(),
    )
    .await
}

pub(crate) async fn read_files(selected: Vec<web_sys::File>) -> Result<Vec<LocalFile>, String> {
    let mut files = Vec::with_capacity(selected.len());
    for file in selected {
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
