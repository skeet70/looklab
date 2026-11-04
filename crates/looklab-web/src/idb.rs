// SPDX-License-Identifier: AGPL-3.0-or-later
//! Best-effort IndexedDB drafts ([docs/08 §4](../../../docs/08-ui.md)).

use js_sys::Promise;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode, IdbVersionChangeEvent,
};

const DATABASE: &str = "looklab";
const STORE: &str = "kv";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageKey(&'static str);

impl StorageKey {
    pub const CREDENTIAL: Self = Self("credential-id");
    pub const STATUS_TOKEN: Self = Self("status-token");
    pub const DRAFT: Self = Self("contribution-draft");
    pub const FIXTURE_CONSENT: Self = Self("fixture-consent");
}

/// # Errors
/// Reports an unavailable or failing IndexedDB.
pub async fn put(key: StorageKey, value: &str) -> Result<(), String> {
    let db = open().await?;
    let transaction = db
        .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite)
        .map_err(|error| format!("{error:?}"))?;
    let store = transaction
        .object_store(STORE)
        .map_err(|error| format!("{error:?}"))?;
    let request = store
        .put_with_key(&JsValue::from_str(value), &JsValue::from_str(key.0))
        .map_err(|error| format!("{error:?}"))?;
    JsFuture::from(request_success(&request))
        .await
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

/// # Errors
/// Reports an unavailable or failing IndexedDB.
pub async fn get(key: StorageKey) -> Result<Option<String>, String> {
    let db = open().await?;
    let transaction = db
        .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readonly)
        .map_err(|error| format!("{error:?}"))?;
    let store = transaction
        .object_store(STORE)
        .map_err(|error| format!("{error:?}"))?;
    let request = store
        .get(&JsValue::from_str(key.0))
        .map_err(|error| format!("{error:?}"))?;
    let value = JsFuture::from(request_success(&request))
        .await
        .map_err(|error| format!("{error:?}"))?;
    Ok(value.as_string())
}

async fn open() -> Result<IdbDatabase, String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_owned())?;
    let factory = window
        .indexed_db()
        .map_err(|error| format!("{error:?}"))?
        .ok_or_else(|| "IndexedDB is unavailable".to_owned())?;
    let request: IdbOpenDbRequest = factory
        .open(DATABASE)
        .map_err(|error| format!("{error:?}"))?;
    let upgrade = Closure::once_into_js(move |event: IdbVersionChangeEvent| {
        let Some(target) = event.target() else {
            return;
        };
        let Ok(request) = target.dyn_into::<IdbOpenDbRequest>() else {
            return;
        };
        let Ok(database) = request.result() else {
            return;
        };
        let Ok(database) = database.dyn_into::<IdbDatabase>() else {
            return;
        };
        let _ = database.create_object_store(STORE);
    });
    request.set_onupgradeneeded(Some(upgrade.unchecked_ref()));
    let database = JsFuture::from(request_success(&request))
        .await
        .map_err(|error| format!("{error:?}"))?;
    database
        .dyn_into::<IdbDatabase>()
        .map_err(|_| "IndexedDB did not return a database".to_owned())
}

fn request_success(request: &IdbRequest) -> Promise {
    let request = request.clone();
    Promise::new(&mut move |resolve, reject| {
        let result_request = request.clone();
        let onsuccess = Closure::once_into_js(move |_event: Event| {
            let value = result_request.result().unwrap_or(JsValue::UNDEFINED);
            let _ = resolve.call1(&JsValue::NULL, &value);
        });
        request.set_onsuccess(Some(onsuccess.unchecked_ref()));
        let onerror = Closure::once_into_js(move |_event: Event| {
            let _ = reject.call1(
                &JsValue::NULL,
                &JsValue::from_str("IndexedDB request failed"),
            );
        });
        request.set_onerror(Some(onerror.unchecked_ref()));
    })
}
