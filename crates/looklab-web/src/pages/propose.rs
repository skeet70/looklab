// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use leptos::prelude::*;
use looklab_schema::{
    LabeledFile, MENU_QUESTION, MakernoteTag, MakernoteValue, ProposalKind, SimKey, infer_mapping,
    validate_proposal,
};

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

#[component]
pub fn ProposePage() -> impl IntoView {
    let kind = RwSignal::new(ProposalKind::Mapping);
    let payload = RwSignal::new(example(ProposalKind::Mapping).to_owned());
    let files = RwSignal::new(EXAMPLE_FILES.to_owned());
    let donate = RwSignal::new(false);

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

    let inference = move || match infer_files(&files.get()) {
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
                prop:value=move || files.get()
                on:input=move |event| files.set(event_target_value(&event))
            ></textarea>
            {inference}
        </section>
        <section aria-labelledby="fixtures">
            <h2 id="fixtures">"Fixture donation"</h2>
            <label>
                <input
                    type="checkbox"
                    prop:checked=move || donate.get()
                    on:change=move |event| donate.set(event_target_checked(&event))
                />
                "Donate these files as test fixtures"
            </label>
            <p class="muted">
                "Each file is a separate opt-in. The consent record holds your handle, the date,
                the CC0-1.0 license and the file's sha256, and exports into fixtures/manifest.toml.
                Mappings without a donated fixture stay provisional."
            </p>
        </section>
        <section aria-labelledby="submit">
            <h2 id="submit">"Submit"</h2>
            <p>
                "Submission needs your passkey's signature over the payload and a proof-of-work,
                and is rate-limited per credential. Until the app is connected, this page only
                validates and summarizes; nothing is uploaded."
            </p>
            <button type="button" disabled=true>"Sign and submit"</button>
        </section>
    }
}

fn infer_files(text: &str) -> Result<looklab_schema::MappingInference, String> {
    let mut files = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (label, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("line {}: expected `answer Tag=value`", index + 1))?;
        let label =
            SimKey::try_from(label).map_err(|error| format!("line {}: {error}", index + 1))?;
        let mut tags = BTreeMap::new();
        for token in rest.split_whitespace() {
            let (tag, value) = token
                .split_once('=')
                .ok_or_else(|| format!("line {}: `{token}` is not `Tag=value`", index + 1))?;
            let tag = MakernoteTag::try_from(tag)
                .map_err(|error| format!("line {}: {error}", index + 1))?;
            let value: u64 = match value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse(),
            }
            .map_err(|_| format!("line {}: `{value}` is not a makernote value", index + 1))?;
            tags.insert(tag, MakernoteValue(value));
        }
        files.push(LabeledFile { label, tags });
    }
    infer_mapping(&files).map_err(|error| error.to_string())
}
