// SPDX-License-Identifier: AGPL-3.0-or-later
//! Client-side scene preview: apply a manifest-checked LUT to a canonical scene.

use std::sync::Arc;

use leptos::html::Canvas;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, render_preview};
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

#[component]
pub fn ScenePreview(
    lut: Arc<Vec<u8>>,
    scene: Arc<PreparedScene>,
    label: String,
    #[prop(optional)] caption: Option<String>,
) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    let timing = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let width = scene.width();
    let height = scene.height();
    let pixels = scene.pixels();

    Effect::new(move |_| {
        let Some(element) = canvas.get() else {
            return;
        };
        let start = window()
            .performance()
            .map(|performance| performance.now())
            .unwrap_or(0.0);
        match render_preview(&lut, &scene) {
            Ok(rgba) => {
                let elapsed = window()
                    .performance()
                    .map(|performance| performance.now() - start)
                    .unwrap_or(0.0);
                let Some(canvas) = element.dyn_ref::<HtmlCanvasElement>() else {
                    return;
                };
                canvas.set_width(width);
                canvas.set_height(height);
                let context = canvas
                    .get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|context| context.dyn_into::<CanvasRenderingContext2d>().ok());
                let Some(context) = context else {
                    error.set(Some("2D canvas is unavailable".to_owned()));
                    return;
                };
                let Ok(image) =
                    ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba), width, height)
                else {
                    error.set(Some("could not build the preview image".to_owned()));
                    return;
                };
                if context.put_image_data(&image, 0.0, 0.0).is_err() {
                    error.set(Some("could not draw the preview".to_owned()));
                    return;
                }
                timing.set(Some(format!(
                    "{:.0} ms for {:.2} MP ({}×{})",
                    elapsed,
                    pixels as f64 / 1_000_000.0,
                    width,
                    height
                )));
            }
            Err(cause) => error.set(Some(cause.to_string())),
        }
    });

    view! {
        <figure class="preview">
            <canvas node_ref=canvas role="img" aria-label=label.clone()></canvas>
            <figcaption>{caption.unwrap_or(label)}</figcaption>
            {move || error.get().map(|message| view! { <p class="error" role="alert">{message}</p> })}
            <p class="timing" aria-live="polite">
                {move || timing.get().unwrap_or_else(|| "Rendering…".to_owned())}
            </p>
        </figure>
    }
}
