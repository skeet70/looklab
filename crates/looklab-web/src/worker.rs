// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser side of the single-threaded workers ([docs/01](../../../docs/01-architecture.md)).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, Worker, WorkerOptions, WorkerType};

use crate::base::site_url;
use looklab_wasm::prep::ModuleFunction;
use looklab_wasm::{
    RECYCLE_AFTER_FILES, RECYCLE_MEMORY_FRACTION, WORKER_BUDGET_BYTES, WORKER_FN_MATCH,
    WORKER_FN_VERIFY, check_module_version, module_fn_id, version_request,
};

pub const FILE_TIMEOUT_MS: i32 = 60_000;

const MODULE_MISMATCH: &str = concat!(
    "This page and its preparation module are different versions. ",
    "Reload the page, then prepare your files again.",
);

const TIMEOUT_MARKER: &str = "__looklab_timeout__";
const TYPE: &str = "type";
const MESSAGE: &str = "message";
const TRAP: &str = "trap";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerMessageKind {
    Result,
    Error,
    ClientError,
    Other,
}

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
            Self::Timeout | Self::Trap => {
                "This file couldn't be decoded (decoder error).".to_owned()
            }
            Self::Module(message) => message.clone(),
        }
    }

    fn kills_worker(&self) -> bool {
        matches!(self, Self::Timeout | Self::Trap)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchFailure(pub String);

/// One worker and the single reply slot its `onmessage` handler resolves.
/// The worker posts `ready` before the first reply, so the handler outlives any one call.
struct WorkerLink {
    worker: Worker,
    pending: Rc<RefCell<Option<Function>>>,
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

impl WorkerLink {
    fn spawn(script: &str) -> Result<Self, String> {
        let options = WorkerOptions::new();
        options.set_type(WorkerType::Module);
        let worker = Worker::new_with_options(script, &options)
            .map_err(|error| format!("{script}: {error:?}"))?;

        let pending: Rc<RefCell<Option<Function>>> = Rc::new(RefCell::new(None));
        let message_slot = Rc::clone(&pending);
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            let kind = message_kind(&data);
            if kind == WorkerMessageKind::ClientError {
                forward_client_error(&data);
            } else if let Some(reply) = reply_from(kind, &data) {
                settle(&message_slot, reply);
            }
        });
        let error_slot = Rc::clone(&pending);
        let onerror =
            Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |event: web_sys::ErrorEvent| {
                if let Some(reply) = failure_reply(&event.message(), false) {
                    settle(&error_slot, reply);
                }
            });
        worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        Ok(Self {
            worker,
            pending,
            _onmessage: onmessage,
            _onerror: onerror,
        })
    }

    fn post(&self, fields: &[(&str, &str)]) -> Result<(), String> {
        let message = Object::new();
        for (key, value) in fields {
            Reflect::set(&message, &JsValue::from_str(key), &JsValue::from_str(value))
                .map_err(|error| format!("{error:?}"))?;
        }
        self.worker
            .post_message(&message.into())
            .map_err(|error| format!("worker message: {error:?}"))
    }

    async fn call(
        &self,
        function: u32,
        bytes: &[u8],
        timeout_ms: i32,
    ) -> Result<WorkerReply, WorkerFailure> {
        let promise = Promise::new(&mut |resolve, reject| {
            *self.pending.borrow_mut() = Some(resolve.clone());
            let message = Object::new();
            let _ = Reflect::set(
                &message,
                &JsValue::from_str(TYPE),
                &JsValue::from_str("call"),
            );
            let _ = Reflect::set(
                &message,
                &JsValue::from_str("fn"),
                &JsValue::from_f64(f64::from(function)),
            );
            let _ = Reflect::set(
                &message,
                &JsValue::from_str("bytes"),
                &Uint8Array::from(bytes),
            );
            if let Err(error) = self.worker.post_message(&message.into()) {
                self.pending.borrow_mut().take();
                let _ = reject.call1(&JsValue::NULL, &error);
            }
        });

        let deadline = Deadline::start(timeout_ms);
        let raced = Promise::race(&Array::of2(&promise, &deadline.promise));
        let outcome = JsFuture::from(raced).await;
        deadline.cancel();
        self.pending.borrow_mut().take();

        let value =
            outcome.map_err(|_| WorkerFailure::Module("worker request failed".to_owned()))?;
        if value.as_string().as_deref() == Some(TIMEOUT_MARKER) {
            return Err(WorkerFailure::Timeout);
        }
        if let Some(message) = field_string(&value, "error") {
            return Err(if field_bool(&value, TRAP) {
                WorkerFailure::Trap
            } else {
                WorkerFailure::Module(message)
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

    fn terminate(&self) {
        self.worker.terminate();
    }
}

impl Drop for WorkerLink {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

pub struct PrepWorker {
    link: WorkerLink,
    files: u64,
    memory: u64,
    dead: bool,
    verified: bool,
}

impl PrepWorker {
    /// # Errors
    /// Reports a browser that cannot start a module worker.
    pub fn spawn() -> Result<Self, String> {
        let script = site_url("/workers/prep-worker.js");
        let module = site_url("/generated/wasm/looklab_prep_module.wasm");
        let link = WorkerLink::spawn(&script)?;
        link.post(&[("type", "init"), ("url", &module), ("diagnostics", "1")])?;
        Ok(Self {
            link,
            files: 0,
            memory: 0,
            dead: false,
            verified: false,
        })
    }

    /// # Errors
    /// Reports a trap, timeout, error or a module built from different versions.
    pub async fn call(
        &mut self,
        function: u32,
        bytes: Vec<u8>,
    ) -> Result<WorkerReply, WorkerFailure> {
        if !self.verified {
            self.check_version().await?;
        }
        self.send(function, bytes).await
    }

    async fn check_version(&mut self) -> Result<(), WorkerFailure> {
        let request = version_request();
        let reply = match self
            .link
            .call(
                module_fn_id(ModuleFunction::Version),
                &request,
                FILE_TIMEOUT_MS,
            )
            .await
        {
            Ok(reply) => reply,
            Err(failure) => return Err(self.stop(failure)),
        };
        if check_module_version(&reply.bytes).is_err() {
            return Err(self.module_mismatch());
        }
        self.verified = true;
        self.memory = reply.memory;
        Ok(())
    }

    fn module_mismatch(&mut self) -> WorkerFailure {
        self.dead = true;
        self.link.terminate();
        WorkerFailure::Module(MODULE_MISMATCH.to_owned())
    }

    async fn send(&mut self, function: u32, bytes: Vec<u8>) -> Result<WorkerReply, WorkerFailure> {
        match self.link.call(function, &bytes, FILE_TIMEOUT_MS).await {
            Ok(reply) => {
                self.files += 1;
                self.memory = reply.memory;
                Ok(reply)
            }
            Err(failure) => Err(self.stop(failure)),
        }
    }

    fn stop(&mut self, failure: WorkerFailure) -> WorkerFailure {
        if failure.kills_worker() {
            self.dead = true;
            self.link.terminate();
        }
        failure
    }

    pub fn should_recycle(&self) -> bool {
        self.dead
            || self.files >= RECYCLE_AFTER_FILES
            || (self.memory as f64) > WORKER_BUDGET_BYTES as f64 * RECYCLE_MEMORY_FRACTION
    }

    pub fn memory(&self) -> u64 {
        self.memory
    }

    pub fn terminate(&self) {
        self.link.terminate();
    }
}

pub struct MatchWorker {
    link: WorkerLink,
}

impl MatchWorker {
    /// # Errors
    /// Reports a browser that cannot start a module worker.
    pub fn spawn() -> Result<Self, String> {
        let script = site_url("/workers/match-worker.js");
        let module = site_url("/generated/wasm/looklab_match.wasm");
        let link = WorkerLink::spawn(&script)?;
        link.post(&[("type", "init"), ("url", &module), ("diagnostics", "1")])?;
        Ok(Self { link })
    }

    /// # Errors
    /// Reports a worker that trapped, timed out or answered with an error.
    pub async fn run_match(&self, request: &[u8]) -> Result<Vec<u8>, MatchFailure> {
        self.link
            .call(WORKER_FN_MATCH, request, FILE_TIMEOUT_MS)
            .await
            .map(|reply| reply.bytes)
            .map_err(|error| MatchFailure(error.message()))
    }

    /// # Errors
    /// Reports a worker that trapped, timed out or answered with an error.
    pub async fn run_verify(&self, request: &[u8]) -> Result<Vec<u8>, MatchFailure> {
        self.link
            .call(WORKER_FN_VERIFY, request, FILE_TIMEOUT_MS)
            .await
            .map(|reply| reply.bytes)
            .map_err(|error| MatchFailure(error.message()))
    }
}

#[repr(u32)]
enum SceneOperation {
    Prepare = 1,
}

pub(crate) async fn prepare_scene(
    name: looklab_publish::SceneName,
    png: &[u8],
) -> Result<looklab_publish::PreparedScene, String> {
    let link = WorkerLink::spawn(&site_url("/workers/scene-worker.js"))?;
    link.post(&[
        ("type", "init"),
        (
            "url",
            &site_url("/generated/wasm/looklab_scene_module.wasm"),
        ),
        ("diagnostics", "1"),
    ])?;
    let reply = link
        .call(SceneOperation::Prepare as u32, png, FILE_TIMEOUT_MS)
        .await
        .map_err(|failure| match failure {
            WorkerFailure::Timeout => "Sample photo preparation timed out.".to_owned(),
            WorkerFailure::Trap => "The sample photo decoder stopped.".to_owned(),
            WorkerFailure::Module(message) => message,
        })?;
    crate::data::timed(crate::data::PreviewStage::SceneReceive, || {
        looklab_publish::PreparedScene::from_bytes(name, &reply.bytes)
    })
    .map_err(|error| error.to_string())
}

struct Deadline {
    promise: Promise,
    handle: Rc<Cell<i32>>,
}

impl Deadline {
    fn start(timeout_ms: i32) -> Self {
        let handle = Rc::new(Cell::new(0));
        let timer_handle = Rc::clone(&handle);
        let promise = Promise::new(&mut move |resolve, _reject| {
            let resolve = resolve.clone();
            let callback = Closure::once_into_js(move || {
                let _ = resolve.call1(&JsValue::NULL, &JsValue::from_str(TIMEOUT_MARKER));
            });
            if let Some(window) = web_sys::window()
                && let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.unchecked_ref::<Function>(),
                    timeout_ms,
                )
            {
                timer_handle.set(id);
            }
        });
        Self { promise, handle }
    }

    fn cancel(&self) {
        let id = self.handle.get();
        if id != 0
            && let Some(window) = web_sys::window()
        {
            window.clear_timeout_with_handle(id);
        }
    }
}

fn settle(slot: &Rc<RefCell<Option<Function>>>, reply: JsValue) {
    if let Some(resolve) = slot.borrow_mut().take() {
        let _ = resolve.call1(&JsValue::NULL, &reply);
    }
}

fn message_kind(value: &JsValue) -> WorkerMessageKind {
    match field_string(value, TYPE).as_deref() {
        Some("result") => WorkerMessageKind::Result,
        Some("error") => WorkerMessageKind::Error,
        Some("client-error") => WorkerMessageKind::ClientError,
        _ => WorkerMessageKind::Other,
    }
}

fn reply_from(kind: WorkerMessageKind, data: &JsValue) -> Option<JsValue> {
    match kind {
        WorkerMessageKind::Result => read_reply(data),
        WorkerMessageKind::Error => failure_reply(
            &field_string(data, MESSAGE).unwrap_or_else(|| "worker error".to_owned()),
            field_bool(data, TRAP),
        ),
        WorkerMessageKind::ClientError | WorkerMessageKind::Other => None,
    }
}

fn field_string(value: &JsValue, key: &str) -> Option<String> {
    Reflect::get(value, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_string())
}

fn field_bool(value: &JsValue, key: &str) -> bool {
    Reflect::get(value, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn forward_client_error(data: &JsValue) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(handler) = Reflect::get(&window, &JsValue::from_str("__looklabWorkerClientError"))
    else {
        return;
    };
    let Some(handler) = handler.dyn_ref::<Function>() else {
        return;
    };
    let _ = handler.call1(&JsValue::NULL, data);
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
    Reflect::set(
        &reply,
        &JsValue::from_str("memory"),
        &JsValue::from_f64(memory as f64),
    )
    .ok()?;
    Some(reply.into())
}

fn failure_reply(message: &str, trap: bool) -> Option<JsValue> {
    let reply = Object::new();
    Reflect::set(
        &reply,
        &JsValue::from_str("error"),
        &JsValue::from_str(message),
    )
    .ok()?;
    Reflect::set(&reply, &JsValue::from_str(TRAP), &JsValue::from_bool(trap)).ok()?;
    Some(reply.into())
}

/// `N` from the device-memory budget rule.
pub fn default_worker_count() -> u32 {
    let device_memory = ReflectedNavigator::device_memory();
    let total_budget = match device_memory {
        Some(gib) => (gib / 4.0).min(2.0),
        None => 1.0,
    };
    ((total_budget * 1024.0 * 1024.0 * 1024.0 / WORKER_BUDGET_BYTES as f64).floor() as u32)
        .clamp(1, 2)
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
