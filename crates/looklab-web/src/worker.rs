// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser side of the single-threaded workers ([docs/01](../../../docs/01-architecture.md)).

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, Worker, WorkerOptions, WorkerType};

use looklab_wasm::{
    RECYCLE_AFTER_FILES, RECYCLE_MEMORY_FRACTION, WORKER_BUDGET_BYTES, WORKER_FN_MATCH,
    WORKER_FN_VERIFY,
};

pub const FILE_TIMEOUT_MS: i32 = 60_000;

#[derive(Clone, Debug, PartialEq)]
pub struct WorkerReply {
    pub bytes: Vec<u8>,
    pub memory: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerFailure {
    Timeout,
    Trap,
    Module(String),
}

impl WorkerFailure {
    pub fn message(&self) -> String {
        match self {
            Self::Timeout => "This file couldn't be decoded (decoder error).".to_owned(),
            Self::Trap => "This file couldn't be decoded (decoder error).".to_owned(),
            Self::Module(message) => message.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchFailure(pub String);

pub struct PrepWorker {
    worker: Worker,
    files: u64,
    memory: u64,
}

impl PrepWorker {
    /// # Errors
    /// Reports a browser that cannot start a module worker.
    pub fn spawn() -> Result<Self, String> {
        let worker = spawn_worker("/workers/prep-worker.js")?;
        send(
            &worker,
            &[
                ("type", "init".into()),
                (
                    "url",
                    "/generated/wasm/looklab_prep_module.wasm".into(),
                ),
            ],
        )?;
        Ok(Self {
            worker,
            files: 0,
            memory: 0,
        })
    }

    /// # Errors
    /// Reports a worker that trapped, timed out or answered with an error.
    pub async fn call(&mut self, function: u32, bytes: Vec<u8>) -> Result<WorkerReply, WorkerFailure> {
        let reply = call_worker(&self.worker, function, bytes, FILE_TIMEOUT_MS).await?;
        self.files += 1;
        self.memory = reply.memory;
        Ok(reply)
    }

    pub fn should_recycle(&self) -> bool {
        self.files >= RECYCLE_AFTER_FILES
            || (self.memory as f64) > WORKER_BUDGET_BYTES as f64 * RECYCLE_MEMORY_FRACTION
    }

    pub fn memory(&self) -> u64 {
        self.memory
    }

    pub fn terminate(&self) {
        self.worker.terminate();
    }
}

impl Drop for PrepWorker {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

pub struct MatchWorker {
    worker: Worker,
}

impl MatchWorker {
    /// # Errors
    /// Reports a browser that cannot start a module worker.
    pub fn spawn() -> Result<Self, String> {
        let worker = spawn_worker("/workers/match-worker.js")?;
        send(
            &worker,
            &[
                ("type", "init".into()),
                ("url", "/generated/wasm/looklab_match.wasm".into()),
            ],
        )?;
        Ok(Self { worker })
    }

    /// # Errors
    /// Reports a worker that trapped, timed out or answered with an error.
    pub async fn run_match(&self, request: &[u8]) -> Result<Vec<u8>, MatchFailure> {
        call_worker(&self.worker, WORKER_FN_MATCH, request.to_vec(), FILE_TIMEOUT_MS)
            .await
            .map(|reply| reply.bytes)
            .map_err(|error| MatchFailure(error.message()))
    }

    /// # Errors
    /// Reports a worker that trapped, timed out or answered with an error.
    pub async fn run_verify(&self, request: &[u8]) -> Result<Vec<u8>, MatchFailure> {
        call_worker(&self.worker, WORKER_FN_VERIFY, request.to_vec(), FILE_TIMEOUT_MS)
            .await
            .map(|reply| reply.bytes)
            .map_err(|error| MatchFailure(error.message()))
    }
}

impl Drop for MatchWorker {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

fn spawn_worker(script: &str) -> Result<Worker, String> {
    let options = WorkerOptions::new();
    options.set_type(WorkerType::Module);
    Worker::new_with_options(script, &options).map_err(|error| format!("{script}: {error:?}"))
}

fn send(worker: &Worker, fields: &[(&str, String)]) -> Result<(), String> {
    let message = Object::new();
    for (key, value) in fields {
        Reflect::set(&message, &JsValue::from_str(key), &JsValue::from_str(value))
            .map_err(|error| format!("{error:?}"))?;
    }
    worker
        .post_message(&message.into())
        .map_err(|error| format!("worker message: {error:?}"))
}

async fn call_worker(
    worker: &Worker,
    function: u32,
    bytes: Vec<u8>,
    timeout_ms: i32,
) -> Result<WorkerReply, WorkerFailure> {
    let worker = worker.clone();
    let promise = Promise::new(&mut move |resolve, reject| {
        let resolve_message = resolve.clone();
        let onmessage = Closure::once_into_js(move |event: MessageEvent| {
            let data = event.data();
            let kind = Reflect::get(&data, &JsValue::from_str("type"))
                .ok()
                .and_then(|value| value.as_string());
            match kind.as_deref() {
                Some("result") => {
                    if let Some(reply) = read_reply(&data) {
                        let _ = resolve_message.call1(&JsValue::NULL, &reply);
                    }
                }
                Some("error") => {
                    let message = Reflect::get(&data, &JsValue::from_str("message"))
                        .ok()
                        .and_then(|value| value.as_string())
                        .unwrap_or_else(|| "worker error".to_owned());
                    let trap = Reflect::get(&data, &JsValue::from_str("trap"))
                        .ok()
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    if let Some(reply) = failure_reply(&message, trap) {
                        let _ = resolve_message.call1(&JsValue::NULL, &reply);
                    }
                }
                _ => {}
            }
        });
        worker.set_onmessage(Some(onmessage.unchecked_ref()));
        let resolve_error = resolve.clone();
        let worker_for_error = worker.clone();
        let onerror = Closure::once_into_js(move |event: web_sys::ErrorEvent| {
            let reply = failure_reply(&event.message(), false);
            if let Some(reply) = reply {
                let _ = resolve_error.call1(&JsValue::NULL, &reply);
            }
            worker_for_error.set_onerror(None);
        });
        worker.set_onerror(Some(onerror.unchecked_ref()));

        let message = Object::new();
        let _ = Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str("call"));
        let _ = Reflect::set(&message, &JsValue::from_str("fn"), &JsValue::from_f64(f64::from(function)));
        let _ = Reflect::set(&message, &JsValue::from_str("id"), &JsValue::from_f64(1.0));
        let array = Uint8Array::from(bytes.as_slice());
        let _ = Reflect::set(&message, &JsValue::from_str("bytes"), &array);
        if let Err(error) = worker.post_message(&message.into()) {
            let _ = reject.call1(&JsValue::NULL, &error);
        }
    });

    let timeout = timeout_promise(timeout_ms);
    let raced = Promise::race(&Array::of2(&promise, &timeout));
    let value = JsFuture::from(raced)
        .await
        .map_err(|_| WorkerFailure::Module("worker request failed".to_owned()))?;
    if value.as_string().as_deref() == Some("__looklab_timeout__") {
        return Err(WorkerFailure::Timeout);
    }
    let error = Reflect::get(&value, &JsValue::from_str("error"))
        .ok()
        .filter(|error| !error.is_undefined() && !error.is_null())
        .and_then(|error| error.as_string());
    if let Some(error) = error {
        let trap = Reflect::get(&value, &JsValue::from_str("trap"))
            .ok()
            .and_then(|trap| trap.as_bool())
            .unwrap_or(false);
        return Err(if trap {
            WorkerFailure::Trap
        } else {
            WorkerFailure::Module(error)
        });
    }
    let bytes = Reflect::get(&value, &JsValue::from_str("bytes"))
        .ok()
        .and_then(|bytes| bytes.dyn_into::<Uint8Array>().ok())
        .ok_or_else(|| WorkerFailure::Module("the worker reply has no bytes".to_owned()))?
        .to_vec();
    let memory = Reflect::get(&value, &JsValue::from_str("memory"))
        .ok()
        .and_then(|memory| memory.as_f64())
        .unwrap_or(0.0) as u64;
    Ok(WorkerReply { bytes, memory })
}

fn read_reply(value: &JsValue) -> Option<JsValue> {
    let bytes = Reflect::get(value, &JsValue::from_str("bytes")).ok()?;
    let bytes = bytes.dyn_into::<Uint8Array>().ok()?;
    let memory = Reflect::get(value, &JsValue::from_str("memory"))
        .ok()
        .and_then(|memory| memory.as_f64())
        .unwrap_or(0.0) as u64;
    let reply = Object::new();
    Reflect::set(&reply, &JsValue::from_str("bytes"), &bytes).ok()?;
    Reflect::set(&reply, &JsValue::from_str("memory"), &JsValue::from_f64(memory as f64)).ok()?;
    Some(reply.into())
}

fn failure_reply(message: &str, trap: bool) -> Option<JsValue> {
    let reply = Object::new();
    Reflect::set(&reply, &JsValue::from_str("error"), &JsValue::from_str(message)).ok()?;
    Reflect::set(&reply, &JsValue::from_str("trap"), &JsValue::from_bool(trap)).ok()?;
    Some(reply.into())
}

fn timeout_promise(timeout_ms: i32) -> Promise {
    Promise::new(&mut move |resolve, _reject| {
        let resolve = resolve.clone();
        let callback = Closure::once_into_js(move || {
            let _ = resolve.call1(&JsValue::NULL, &JsValue::from_str("__looklab_timeout__"));
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.unchecked_ref::<Function>(),
                timeout_ms,
            );
        }
    })
}

/// `N` from the device-memory budget rule.
pub fn default_worker_count() -> u32 {
    let device_memory = ReflectedNavigator::device_memory();
    let total_budget = match device_memory {
        Some(gib) => (gib / 4.0).min(2.0),
        None => 1.0,
    };
    ((total_budget * 1024.0 * 1024.0 * 1024.0 / WORKER_BUDGET_BYTES as f64).floor() as u32).clamp(1, 2)
}

/// The ceiling for a user-raised worker count.
pub fn max_worker_count() -> u32 {
    let concurrency = ReflectedNavigator::hardware_concurrency().max(2);
    (concurrency - 1).min(4)
}

pub fn worker_functions() -> (u32, u32) {
    (WORKER_FN_MATCH, WORKER_FN_VERIFY)
}

struct ReflectedNavigator;

impl ReflectedNavigator {
    fn device_memory() -> Option<f64> {
        let navigator = web_sys::window()?.navigator();
        Reflect::get(&navigator, &JsValue::from_str("deviceMemory"))
            .ok()
            .and_then(|value| value.as_f64())
            .filter(|value| *value > 0.0)
    }

    fn hardware_concurrency() -> u32 {
        web_sys::window()
            .map(|window| window.navigator().hardware_concurrency() as u32)
            .unwrap_or(2)
    }
}
