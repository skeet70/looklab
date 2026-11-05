// SPDX-License-Identifier: AGPL-3.0-or-later
//! The proposal form ([docs/08 §4](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_schema::{MENU_QUESTION, ProposalKind, ProposalView, validate_proposal};
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;

use crate::proposal_form::{self, FixtureFile};
use crate::{data, files};

const EXAMPLE_FILES: &str = "\
classic_chrome FilmMode=0x0700
provia FilmMode=0x0000
acros FilmMode=0x0500 GrainSize=0x0002";

const BODY: &str = include_str!("../../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../../data/bodies/fujifilm-x-t5.yaml");
const INTERFACE: &str = include_str!("../../../../data/interfaces/example-firmware-sim-v1.yaml");

fn example(kind: ProposalKind) -> &'static str {
    match kind {
        ProposalKind::Body => BODY,
        ProposalKind::SchemaChange => SCHEMA,
        ProposalKind::InterfaceProfile => INTERFACE,
        ProposalKind::Mapping => {
            "\
body: fujifilm/x-t5
epoch: 2
sim_key: reala_ace
look: reala-ace
firmware_from: \"2.00\"
makernote: { tag: FilmMode, value: 0x0700 }
"
        }
        ProposalKind::LutLook => {
            "\
look: my-lut
name: My LUT
license: { own_work: true }
attribution: Your name here
"
        }
        ProposalKind::EpochSplit => {
            "\
body: fujifilm/x-t5
from_epoch: 1
new_epoch: 2
split_at: \"2.00\"
"
        }
        ProposalKind::LookSplit => {
            "\
look: classic-chrome
new_look: classic-chrome-mono
name: Classic Chrome Mono
"
        }
        ProposalKind::GateOverride => {
            "\
look: classic-chrome
version: 3
justification: new root session
"
        }
    }
}

fn body_identity(kind: ProposalKind, payload: &str) -> Option<(String, String)> {
    match validate_proposal(kind, payload).ok()? {
        looklab_schema::ValidatedPayload::Body(body) => Some((body.make, body.model)),
        _ => None,
    }
}

#[component]
pub fn ProposePage() -> impl IntoView {
    let kind = RwSignal::new(ProposalKind::Mapping);
    let payload = RwSignal::new(example(ProposalKind::Mapping).to_owned());
    let files_text = RwSignal::new(EXAMPLE_FILES.to_owned());
    let fixture_files = RwSignal::new(Vec::<FixtureFile>::new());
    let consented = RwSignal::new(Vec::<bool>::new());
    let fixture_make = RwSignal::new("Fujifilm".to_owned());
    let fixture_model = RwSignal::new("X-T5".to_owned());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let submitted = RwSignal::new(None::<ProposalView>);

    let preview = move || match validate_proposal(kind.get(), &payload.get()) {
        Ok(validated) => {
            let summary = validated.summary();
            let details = summary
                .details
                .iter()
                .map(|detail| view! { <li>{detail.clone()}</li> })
                .collect_view();
            view! {
                <div class="valid">
                    <p class="muted">"The browser validator accepts this payload."</p>
                    <h3>{summary.headline}</h3>
                    <ul>{details}</ul>
                </div>
            }
            .into_any()
        }
        Err(errors) => {
            let items = errors
                .iter()
                .map(|error| view! { <li>{error.to_string()}</li> })
                .collect_view();
            view! {
                <div class="error" role="alert">
                    <strong>"The browser validator rejects this payload."</strong>
                    <ul>{items}</ul>
                </div>
            }
            .into_any()
        }
    };

    let inference = move || match proposal_form::parse_labeled_files(&files_text.get()) {
        Ok(inference) => {
            let question = MENU_QUESTION.to_owned();
            let rows = inference
                .candidates
                .iter()
                .map(|candidate| {
                    let pairs = candidate
                        .pairs
                        .iter()
                        .map(|(label, value)| format!("{label}: {:04x}", value.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    view! {
                        <tr>
                            <th scope="row">{candidate.tag.to_string()}</th>
                            <td>{if candidate.consistent { "yes" } else { "no" }}</td>
                            <td>{format!("{:.0}%", candidate.confidence * 100.0)}</td>
                            <td>{pairs}</td>
                        </tr>
                    }
                })
                .collect_view();
            let best = inference.best().map(|candidate| {
                format!("Use {} = the value that differs per answer.", candidate.tag)
            });
            view! {
                <p class="muted">{question}</p>
                <table class="data-table">
                    <caption>"Candidate tags"</caption>
                    <thead>
                        <tr>
                            <th scope="col">"Tag"</th>
                            <th scope="col">"Separates the answers"</th>
                            <th scope="col">"Confidence"</th>
                            <th scope="col">"Values"</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
                {best.unwrap_or_else(|| "No single tag separates the answers yet.".to_owned())}
            }
            .into_any()
        }
        Err(error) => view! { <p class="error">{error}</p> }.into_any(),
    };

    let on_fixtures = move |event: leptos::ev::Event| {
        let Some(input) = event
            .target()
            .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
        else {
            return;
        };
        spawn_local(async move {
            match files::read_input(&input).await {
                Ok(read) => {
                    let mut parsed = Vec::with_capacity(read.len());
                    for file in read {
                        match FixtureFile::new(file.name, file.bytes) {
                            Ok(file) => parsed.push(file),
                            Err(error) => {
                                message.set(Some(error));
                                return;
                            }
                        }
                    }
                    let flags = vec![false; parsed.len()];
                    fixture_files.set(parsed);
                    consented.set(flags);
                }
                Err(error) => message.set(Some(error)),
            }
        });
    };

    let submit = move |_| {
        if busy.get() {
            return;
        }
        let current_kind = kind.get();
        let current_payload = payload.get();
        if let Err(errors) = validate_proposal(current_kind, &current_payload) {
            let text = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            message.set(Some(format!(
                "The browser validator rejects this payload: {text}"
            )));
            return;
        }
        let selected = fixture_files.get();
        let flags = consented.get();
        let identity = body_identity(current_kind, &current_payload);
        let fallback_make = fixture_make.get();
        let fallback_model = fixture_model.get();
        busy.set(true);
        message.set(None);
        submitted.set(None);
        spawn_local(async move {
            let (make, model) = identity.unwrap_or((fallback_make, fallback_model));
            let mut donations = Vec::new();
            for (index, file) in selected.iter().enumerate() {
                if !flags.get(index).copied().unwrap_or(false) {
                    continue;
                }
                match proposal_form::donation(file, &make, &model) {
                    Ok(donation) => donations.push(donation),
                    Err(error) => {
                        message.set(Some(error));
                        busy.set(false);
                        return;
                    }
                }
            }
            let outcome = data::submit_proposal(current_kind, &current_payload, &donations).await;
            match outcome {
                Ok(reply) => submitted.set(Some(reply.view)),
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    view! {
        <h1>"Propose a change"</h1>
        <p>
            "Proposals are shown in menu terms. The same validator that CI runs checks the payload here
            in the browser, so errors are caught before submission."
        </p>
        <section aria-labelledby="payload">
            <h2 id="payload">"The change"</h2>
            <label for="kind">"What are you proposing?"</label>
            <select
                id="kind"
                on:change=move |event| {
                    let value = event_target_value(&event);
                    if let Ok(next) = value.parse::<ProposalKind>() {
                        kind.set(next);
                        payload.set(example(next).to_owned());
                    }
                }
            >
                {ProposalKind::ALL
                    .into_iter()
                    .map(|candidate| {
                        view! {
                            <option
                                value=candidate.as_str()
                                prop:selected=move || kind.get() == candidate
                            >
                                {candidate.menu_label()}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
            <label for="payload-yaml">"Payload (YAML, validated locally)"</label>
            <textarea
                id="payload-yaml"
                rows="16"
                cols="80"
                prop:value=move || payload.get()
                on:input=move |event| payload.set(event_target_value(&event))
            ></textarea>
            <h3>"Menu-level summary"</h3>
            {preview}
        </section>
        <section aria-labelledby="mapping">
            <h2 id="mapping">"Film simulation mapping"</h2>
            <p>
                "Label a few files with the menu answer and paste their parsed makernote values.
                The app infers which tag separates the answers."
            </p>
            <label for="mapping-files">"One file per line: answer Tag=value ..."</label>
            <textarea
                id="mapping-files"
                rows="5"
                cols="60"
                prop:value=move || files_text.get()
                on:input=move |event| files_text.set(event_target_value(&event))
            ></textarea>
            {inference}
        </section>
        <section aria-labelledby="fixtures">
            <h2 id="fixtures">"Fixture donation"</h2>
            <p>
                "Each file is a separate opt-in. The consent record holds your handle, the date,
                the CC0-1.0 license and the file's sha256, and exports into fixtures/manifest.toml.
                Mappings without a donated fixture stay provisional."
            </p>
            <label for="fixture-files">"Files you can donate"</label>
            <input type="file" id="fixture-files" multiple on:change=on_fixtures/>
            <div class="field-row">
                <label for="fixture-make">"Make"</label>
                <input
                    type="text"
                    id="fixture-make"
                    prop:value=move || fixture_make.get()
                    on:input=move |event| fixture_make.set(event_target_value(&event))
                />
                <label for="fixture-model">"Model"</label>
                <input
                    type="text"
                    id="fixture-model"
                    prop:value=move || fixture_model.get()
                    on:input=move |event| fixture_model.set(event_target_value(&event))
                />
            </div>
            {move || {
                fixture_files
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(index, file)| {
                        view! {
                            <label class="consent">
                                <input
                                    type="checkbox"
                                    id=format!("donate-{index}")
                                    prop:checked=move || {
                                        consented.get().get(index).copied().unwrap_or(false)
                                    }
                                    on:change=move |event| {
                                        let checked = event_target_checked(&event);
                                        consented
                                            .update(|values| {
                                                if let Some(slot) = values.get_mut(index) {
                                                    *slot = checked;
                                                }
                                            });
                                    }
                                />
                                {format!(
                                    "Donate {} under CC0-1.0 ({} bytes, sha256 {})",
                                    file.name,
                                    file.bytes.len(),
                                    &file.sha256.to_hex()[..16],
                                )}
                            </label>
                        }
                    })
                    .collect_view()
            }}
        </section>
        <section aria-labelledby="submit">
            <h2 id="submit">"Submit"</h2>
            <p>
                "Submission needs your passkey's signature over the payload and a proof-of-work,
                and is rate-limited per credential."
            </p>
            <button type="button" id="submit-proposal" disabled=move || busy.get() on:click=submit>
                {move || if busy.get() { "Signing and working…" } else { "Sign and submit" }}
            </button>
            {move || {
                message
                    .get()
                    .map(|text| view! { <p class="error" role="alert">{text}</p> })
            }}
            {move || {
                submitted
                    .get()
                    .map(|proposal| {
                        let href = format!("/proposals/{}", proposal.id.as_str());
                        view! {
                            <div class="valid">
                                <p>"Submitted. It is now in the moderation queue."</p>
                                <a id="submitted-link" href=href>"Open the proposal"</a>
                            </div>
                        }
                    })
            }}
        </section>
    }
}
