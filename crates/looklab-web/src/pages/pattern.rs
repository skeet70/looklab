// SPDX-License-Identifier: AGPL-3.0-or-later
//! Full-screen seeded phone pattern ([docs/07 §3.1](../../../docs/07-contribution.md)).

use leptos::html::Canvas;
use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use looklab_prep::PatternSeed;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

static PUBLIC_PATTERN: LazyLock<PatternSeed> =
    LazyLock::new(|| PatternSeed::from_bytes(Sha256::digest(b"looklab-fixture-pattern-v1").into()));

pub(super) fn fixture_seed() -> &'static PatternSeed {
    &PUBLIC_PATTERN
}

#[component]
pub fn PatternPage() -> impl IntoView {
    let params = use_params_map();
    let canvas = NodeRef::<Canvas>::new();
    let error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        let seed = params.get().get("seed").unwrap_or_default();
        let Some(canvas) = canvas.get() else {
            return;
        };
        draw(&seed, &canvas, error);
        let seed_clone = seed.clone();
        let canvas_clone = canvas.clone();
        let callback = Closure::<dyn FnMut()>::new(move || {
            draw(&seed_clone, &canvas_clone, error);
        });
        if let Some(window) = web_sys::window() {
            window.set_onresize(Some(callback.as_ref().unchecked_ref()));
        }
        callback.forget();
    });

    view! {
        <div class="pattern-page">
            <canvas
                node_ref=canvas
                role="button"
                tabindex="0"
                aria-label="Measurement pattern. Tap or press Enter for fullscreen."
                on:click=move |_| request_fullscreen(canvas)
                on:keydown=move |event: leptos::ev::KeyboardEvent| {
                    if matches!(event.key().as_str(), "Enter" | " ") {
                        event.prevent_default();
                        request_fullscreen(canvas);
                    }
                }
            ></canvas>
            {move || error.get().map(|message| view! {
                <p class="error" role="alert">{message}</p>
            })}
        </div>
    }
}

fn request_fullscreen(canvas: NodeRef<Canvas>) {
    if let Some(canvas) = canvas.get() {
        let _ = canvas.request_fullscreen();
    }
}

fn draw(seed: &str, canvas: &HtmlCanvasElement, error: RwSignal<Option<String>>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let width = window
        .inner_width()
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as u32;
    let height = window
        .inner_height()
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as u32;
    if width == 0 || height == 0 {
        return;
    }
    match looklab_wasm::render_pattern(seed, width, height) {
        Ok(image) => {
            canvas.set_width(image.width);
            canvas.set_height(image.height);
            let context = canvas
                .get_context("2d")
                .ok()
                .flatten()
                .and_then(|context| context.dyn_into::<CanvasRenderingContext2d>().ok());
            let Some(context) = context else {
                error.set(Some("2D canvas is unavailable".to_owned()));
                return;
            };
            let Ok(image_data) = ImageData::new_with_u8_clamped_array_and_sh(
                wasm_bindgen::Clamped(&image.rgba),
                image.width,
                image.height,
            ) else {
                error.set(Some("could not build the pattern image".to_owned()));
                return;
            };
            if context.put_image_data(&image_data, 0.0, 0.0).is_err() {
                error.set(Some("could not draw the pattern".to_owned()));
            }
        }
        Err(failure) => error.set(Some(failure.to_string())),
    }
}
