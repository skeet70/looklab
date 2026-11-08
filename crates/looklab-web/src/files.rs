// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local file reading and download ([docs/06 §7](../../../docs/06-privacy-and-payload.md)).

use js_sys::Uint8Array;
use looklab_prep::{FileKind, LocalImageKey, RawFormat};
use looklab_wasm::MetadataFile;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, BlobPropertyBag, File, HtmlAnchorElement, Url};

/// A selected file, backed by the browser's `File` or a stored draft `Blob`.
#[derive(Clone)]
pub struct LocalFile {
    name: String,
    blob: Blob,
    size: u64,
}

impl LocalFile {
    fn from_file(file: File) -> Self {
        let name = file.name();
        let blob: Blob = file.unchecked_into();
        let size = blob.size() as u64;
        Self { name, blob, size }
    }

    fn from_blob(name: String, blob: Blob) -> Self {
        let size = blob.size() as u64;
        Self { name, blob, size }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn is_jpeg(&self) -> bool {
        LocalImageKey::new(&self.name).is_some_and(|key| key.kind() == FileKind::Jpeg)
    }

    /// # Errors
    /// Reports a file the browser cannot read.
    pub async fn bytes(&self) -> Result<Vec<u8>, String> {
        let buffer = JsFuture::from(self.blob.array_buffer())
            .await
            .map_err(|error| format!("{}: {error:?}", self.name))?;
        Ok(Uint8Array::new(&buffer).to_vec())
    }

    fn blob(&self) -> &Blob {
        &self.blob
    }
}

pub(crate) fn selected_files(selected: Vec<File>) -> Vec<LocalFile> {
    selected.into_iter().map(LocalFile::from_file).collect()
}

pub(crate) fn camera_originals_accept_attribute() -> String {
    const JPEG: &str = ".jpg,.jpeg";
    let capacity = JPEG.len()
        + RawFormat::ALL
            .iter()
            .map(|format| format.extension().len() + 2)
            .sum::<usize>();
    let mut accept = String::with_capacity(capacity);
    for format in RawFormat::ALL {
        accept.push('.');
        accept.push_str(format.extension());
        accept.push(',');
    }
    accept.push_str(JPEG);
    accept
}

/// One file's bytes are live at a time.
pub(crate) async fn scan_metadata(files: &[LocalFile]) -> Result<Vec<MetadataFile>, String> {
    let mut scanned = Vec::with_capacity(files.len());
    for file in files {
        let bytes = file.bytes().await?;
        scanned.push(MetadataFile::new(file.name.clone(), &bytes));
    }
    Ok(scanned)
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
            transaction.onerror = () => {
                try { transaction.abort(); } catch {}
            };
            transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
        });
    }
    cancelPending() {
        this.revision += 1;
        clearTimeout(this.timer);
        this.pending = null;
    }
    replaceFiles(files) {
        this.cancelPending();
        return this.enqueue(async () => {
            await this.transaction("readwrite", store => store.clear());
            return this.transaction("readwrite", store => {
                for (const [key, file] of files) store.put(file, ["file", key]);
            });
        });
    }
    schedule(text, retainPrepared) {
        const revision = ++this.revision;
        this.pending = { text, revision, retainPrepared };
        clearTimeout(this.timer);
        if (retainPrepared) {
            this.timer = setTimeout(() => this.flush(), 300);
        } else {
            this.flush();
        }
    }
    flush() {
        clearTimeout(this.timer);
        const pending = this.pending;
        this.pending = null;
        if (!pending) return;
        this.enqueue(() => {
            if (pending.revision !== this.revision) return;
            return this.transaction("readwrite", store => {
                if (!pending.retainPrepared) store.delete("prepared");
                store.put(pending.text, "draft");
            });
        }).catch(() => {});
    }
    prepared(text) {
        this.flush();
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
    pub(crate) fn schedule(this: &ContributionDraftStore, text: &str, retain_prepared: bool);
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
            entry.push(&JsValue::from(file.blob().clone()));
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
    let value = JsFuture::from(pending)
        .await
        .map_err(|error| format!("{error:?}"))?;
    if value.is_undefined() || value.is_null() {
        return Err(
            "The local draft's files are no longer available. Add your originals again.".to_owned(),
        );
    }
    let blob = if value.is_instance_of::<Blob>() {
        value.unchecked_into::<Blob>()
    } else {
        let bytes = value
            .dyn_into::<Uint8Array>()
            .map_err(|_| "The local draft's files are no longer available.".to_owned())?;
        let parts = js_sys::Array::new();
        parts.push(&JsValue::from(bytes));
        let options = BlobPropertyBag::new();
        options.set_type("application/octet-stream");
        Blob::new_with_u8_array_sequence_and_options(&parts, &options)
            .map_err(|error| format!("{error:?}"))?
    };
    Ok(LocalFile::from_blob(file.name.clone(), blob))
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
