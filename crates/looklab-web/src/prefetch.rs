// SPDX-License-Identifier: AGPL-3.0-or-later
//! Background loading of the lazy route chunks.

use std::sync::atomic::{AtomicBool, Ordering};

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use crate::lazy_routes;

/// Pages that paint no photo and report no failure still prefetch once the page has settled.
const SETTLE_MS: i32 = 30_000;

#[derive(Clone, Copy)]
struct FirstPagePainted(RwSignal<bool>);

impl FirstPagePainted {
    fn arm(self) {
        if !self.0.get_untracked() {
            self.0.set(true);
        }
    }

    /// Defers arming past the frame that paints the photo.
    fn arm_after_paint(self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let arm = Closure::once_into_js(move || self.arm());
        let next = arm.clone();
        let frame = Closure::once_into_js(move || {
            if let Some(window) = web_sys::window()
                && let Some(arm) = next.dyn_ref::<js_sys::Function>()
            {
                let _ = window.request_animation_frame(arm);
            }
        });
        let _ = window.request_animation_frame(frame.unchecked_ref());
    }
}

pub(crate) fn provide() {
    let painted = RwSignal::new(false);
    provide_context(FirstPagePainted(painted));
    Effect::new(move |_| {
        if painted.get() {
            request_prefetch();
        }
    });
    if let Some(window) = web_sys::window() {
        let arm = Closure::once_into_js(move || painted.set(true));
        let _ = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(arm.unchecked_ref(), SETTLE_MS);
    }
}

pub(crate) fn first_photo_painted() {
    if let Some(painted) = use_context::<FirstPagePainted>() {
        painted.arm_after_paint();
    }
}

/// For routes that paint no photo.
pub(crate) fn route_rendered() {
    if let Some(painted) = use_context::<FirstPagePainted>() {
        painted.arm();
    }
}

fn request_prefetch() {
    static SCHEDULED: AtomicBool = AtomicBool::new(false);
    if SCHEDULED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::once_into_js(|| spawn_local(lazy_routes::preload_all()));
    let idle = js_sys::Reflect::get(&window, &JsValue::from_str("requestIdleCallback"))
        .ok()
        .and_then(|value| value.dyn_into::<js_sys::Function>().ok());
    match idle {
        Some(idle) => {
            let options = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &options,
                &JsValue::from_str("timeout"),
                &JsValue::from_f64(2000.0),
            );
            let _ = idle.call2(&JsValue::from(window), &callback, &options);
        }
        None => {
            let _ = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), 0);
        }
    }
}
