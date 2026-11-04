// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local verification loop ([docs/04 §5](../../../docs/04-matching-and-export.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use looklab_match::Permalink;
use looklab_wasm::{MatchInputs, VerifyRequestMessage, decode_verify_reply, encode_verify_request};
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;

use crate::files::{LocalFile, read_input};
use crate::match_data;
use crate::pages::contribute::run_files;
use crate::ui;
use crate::webauthn::{decode_base64url, encode_base64url};
use crate::worker::{MatchWorker, default_worker_count};

#[component]
pub fn VerifyPage() -> impl IntoView {
    let query = use_query_map();
    let permalink_text = RwSignal::new(String::new());
    let inputs = RwSignal::new(None::<MatchInputs>);
    let files = RwSignal::new(Vec::<LocalFile>::new());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let outcome = RwSignal::new(None::<looklab_wasm::VerifyReply>);
    let error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        let encoded = query.get().get("permalink").unwrap_or_default();
        if let Some(bytes) = decode_base64url(&encoded)
            && let Ok(text) = String::from_utf8(bytes)
        {
            permalink_text.set(text);
        }
    });

    let load = move |_| {
        let text = permalink_text.get();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let permalink = Permalink::parse(&text).map_err(|error| error.to_string())?;
                match_data::inputs_from_permalink(&permalink).await
            }
            .await;
            match result {
                Ok(inputs_value) => {
                    inputs.set(Some(inputs_value));
                    message.set(Some(
                        "Now drop the re-rendered RAW+JPEG pairs. Processing stays local."
                            .to_owned(),
                    ));
                }
                Err(failure) => error.set(Some(failure)),
            }
            busy.set(false);
        });
    };

    let on_files = move |event: leptos::ev::Event| {
        let Some(input) = event
            .target()
            .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
        else {
            return;
        };
        spawn_local(async move {
            match read_input(&input).await {
                Ok(files_value) => files.set(files_value),
                Err(failure) => error.set(Some(failure)),
            }
        });
    };

    let run = move |_| {
        let Some(inputs_value) = inputs.get() else {
            error.set(Some("load a permalink first".to_owned()));
            return;
        };
        let files_value = files.get();
        if files_value.is_empty() {
            error.set(Some("drop the re-rendered files first".to_owned()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let pairing = looklab_wasm::inspect_files(
                    &files_value
                        .iter()
                        .map(|file| (file.name.clone(), file.bytes.clone()))
                        .collect::<Vec<_>>(),
                )
                .map_err(|error| error.to_string())?;
                let (groups, _, failures) =
                    run_files(&pairing, &files_value, default_worker_count()).await?;
                if groups.is_empty() {
                    let reason = failures
                        .first()
                        .map(|failure| failure.message.clone())
                        .unwrap_or_else(|| "no re-render could be prepared".to_owned());
                    return Err(reason);
                }
                let observed: Vec<Vec<u8>> = groups
                    .iter()
                    .flat_map(|group| group.blobs.iter().cloned())
                    .collect();
                let request = encode_verify_request(&VerifyRequestMessage {
                    inputs: inputs_value,
                    observed_blobs: observed,
                });
                let worker = MatchWorker::spawn()?;
                let reply = worker
                    .run_verify(&request)
                    .await
                    .map_err(|failure| failure.0)?;
                decode_verify_reply(&reply)
            }
            .await;
            match result {
                Ok(value) => {
                    message.set(Some(
                        "One verification iteration finished locally.".to_owned(),
                    ));
                    outcome.set(Some(value));
                }
                Err(failure) => error.set(Some(failure)),
            }
            busy.set(false);
        });
    };

    view! {
        <h1>"Verify"</h1>
        <p class="lede">
            "Re-render two to four RAWs at the recommended settings, then drop them here. "
            "The fit updates a local copy of the control-response model; nothing is uploaded."
        </p>
        <label for="permalink">"Permalink"</label>
        <textarea id="permalink" rows="3" prop:value=move || permalink_text.get()
            on:input=move |event| permalink_text.set(event_target_value(&event))></textarea>
        <button on:click=load disabled=move || busy.get()>"Load pinned versions"</button>
        {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        {move || error.get().map(|text| view! { <div class="error" role="alert">{text}</div> })}
        <section class="step" hidden=move || inputs.get().is_none()>
            <h2>"Re-rendered files"</h2>
            <input type="file" multiple accept=".raf,.dng,.jpg,.jpeg,.arw,.cr2,.nef,.orf,.rw2"
                on:change=on_files/>
            <button on:click=run disabled=move || busy.get()>"Run one iteration"</button>
        </section>
        {move || busy.get().then(ui::loading)}
        {move || outcome.get().map(|reply| view! {
            <section class="step">
                <h2>"Result"</h2>
                <p>
                    {format!("First suggestion {}: measured median ΔE {:.2} on {} sample(s).",
                        reply.first.sim, reply.first_measured.median, reply.first_measured.count)}
                </p>
                <p>
                    {format!("Second suggestion {}: predicted median ΔE {:.2}.",
                        reply.second.sim, reply.second.median_delta_e)}
                </p>
                {reply.reduction.map(|reduction| view! {
                    <p class="pass">{format!("Measured reduction {:.0}%.", reduction * 100.0)}</p>
                })}
                <p class="muted">
                    {format!("Updated control-response model version {}. Re-render at the second suggestion to measure it.",
                        reply.updated_crm_version)}
                </p>
                <h3>"Second settings card"</h3>
                <crate::pages::match_page::Card card=reply.card/>
                <p>
                    <a href="/contribute".to_string()>"Contribute the re-renders"</a>
                    " (this creates a passkey if you do not have one)."
                </p>
                <p class="muted">
                    "Pinned permalink: "
                    <a href=format!("/match/r/{}", encode_base64url(reply.permalink.as_bytes()))>
                        "reproduce"
                    </a>
                </p>
            </section>
        })}
    }
}
