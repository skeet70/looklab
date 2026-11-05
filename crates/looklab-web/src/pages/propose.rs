// SPDX-License-Identifier: AGPL-3.0-or-later
//! The proposal form ([docs/08 §4](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_schema::{MENU_QUESTION, ProposalKind, ProposalView, validate_proposal};

use crate::proposal_form::{self, FixtureFile};
use crate::{data, files};

const EXAMPLE_FILES: &str = "\
classic_chrome FilmMode=0x0700
provia FilmMode=0x0000
acros FilmMode=0x0500 GrainSize=0x0002";

const BODY: &str = include_str!("../../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../../data/bodies/fujifilm-x-t5.yaml");
const INTERFACE: &str = include_str!("../../../../data/interfaces/example-app-lut3d-v1.yaml");

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

#[derive(Clone, Debug, PartialEq)]
struct PendingDonations {
    files: Vec<FixtureFile>,
    make: String,
    model: String,
}

#[derive(Clone, Debug)]
struct MenuFileDraft {
    name: String,
    answer: String,
    parsed_values: String,
}

fn body_identity(kind: ProposalKind, payload: &str) -> Option<(String, String)> {
    match validate_proposal(kind, payload).ok()? {
        looklab_schema::ValidatedPayload::Body(body) => Some((body.make, body.model)),
        _ => None,
    }
}

#[component]
pub fn ProposePage() -> impl IntoView {
    let session = crate::session::use_session();
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
    let pending = RwSignal::new(None::<PendingDonations>);
    let available = crate::base::server_available();
    let labeled = RwSignal::new(Vec::<MenuFileDraft>::new());
    let read_mapping = Callback::new(move |selected| {
        spawn_local(async move {
            match files::read_files(selected).await {
                Ok(read) => {
                    let mut parsed = Vec::new();
                    for file in read {
                        match proposal_form::file_menu_value(&file.bytes) {
                            Ok(tags) => parsed.push(MenuFileDraft {
                                name: file.name,
                                answer: String::new(),
                                parsed_values: tags.replace('\n', " "),
                            }),
                            Err(error) => {
                                message.set(Some(error));
                                return;
                            }
                        }
                    }
                    labeled.set(parsed);
                }
                Err(error) => message.set(Some(error)),
            }
        });
    });
    let query = leptos_router::hooks::use_query_map();
    if let Some(value) = query.get_untracked().get("kind")
        && let Ok(initial) = value.parse::<ProposalKind>()
        && matches!(
            initial,
            ProposalKind::Body
                | ProposalKind::Mapping
                | ProposalKind::SchemaChange
                | ProposalKind::LutLook
                | ProposalKind::InterfaceProfile
        )
    {
        kind.set(initial);
        payload.set(example(initial).to_owned());
    }

    let preview = move || match validate_proposal(kind.get(), &payload.get()) {
        Ok(validated) => {
            let summary = proposal_form::menu_summary(kind.get(), &validated.summary());
            let details = summary
                .details
                .iter()
                .map(|detail| view! { <li>{detail.clone()}</li> })
                .collect_view();
            view! {
                <div class="valid">
                    <p class="muted">"Ready for a moderator to review."</p>
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
                    <strong>"Check these details before sending."</strong>
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

    let on_fixtures = Callback::new(move |selected| {
        spawn_local(async move {
            match files::read_files(selected).await {
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
    });

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
            message.set(Some(format!("Check these details before sending: {text}")));
            return;
        }
        let chosen: Vec<FixtureFile> = fixture_files
            .get()
            .into_iter()
            .enumerate()
            .filter(|(index, _)| consented.get().get(*index).copied().unwrap_or(false))
            .map(|(_, file)| file)
            .collect();
        let identity = body_identity(current_kind, &current_payload);
        let fallback_make = fixture_make.get();
        let fallback_model = fixture_model.get();
        message.set(None);
        submitted.set(None);
        if chosen.is_empty() {
            busy.set(true);
            spawn_local(async move {
                if session.require_profile().await.is_none() {
                    busy.set(false);
                    return;
                }
                match data::submit_proposal(session, current_kind, &current_payload, &[]).await {
                    Ok(reply) => submitted.set(Some(reply.view)),
                    Err(error) => message.set(Some(error)),
                }
                busy.set(false);
            });
            return;
        }
        let (make, model) = identity.unwrap_or((fallback_make, fallback_model));
        pending.set(Some(PendingDonations {
            files: chosen,
            make,
            model,
        }));
    };

    let confirm_donations = move |_| {
        let Some(selected) = pending.get() else {
            return;
        };
        let current_kind = kind.get();
        let current_payload = payload.get();
        busy.set(true);
        message.set(None);
        spawn_local(async move {
            if session.require_profile().await.is_none() {
                busy.set(false);
                return;
            }
            let mut donations = Vec::with_capacity(selected.files.len());
            for file in &selected.files {
                let source = match data::upload_fixture(session, file).await {
                    Ok(source) => source,
                    Err(error) => {
                        message.set(Some(error));
                        busy.set(false);
                        return;
                    }
                };
                match proposal_form::donation(file, &selected.make, &selected.model, source) {
                    Ok(donation) => donations.push(donation),
                    Err(error) => {
                        message.set(Some(error));
                        busy.set(false);
                        pending.set(None);
                        return;
                    }
                }
            }
            match data::submit_proposal(session, current_kind, &current_payload, &donations).await {
                Ok(reply) => {
                    submitted.set(Some(reply.view));
                    pending.set(None);
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let cancel_donations = move |_| pending.set(None);

    view! {
        <h1>"Propose a change"</h1>
        <p class="lede">"Tell us what your camera can do. A moderator will check your change and the evidence you share."</p>
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
                {[ProposalKind::Body, ProposalKind::Mapping, ProposalKind::SchemaChange, ProposalKind::LutLook, ProposalKind::InterfaceProfile]
                    .into_iter()
                    .map(|candidate| {
                        view! {
                            <option
                                value=candidate.as_str()
                                prop:selected=move || kind.get() == candidate
                            >
                                {proposal_form::kind_label(candidate)}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
            <MenuFields kind payload/>
            <crate::ui::Disclosure summary="Advanced".to_owned()>
            <label for="payload-yaml">"Payload (YAML, validated locally)"</label>
            <textarea
                id="payload-yaml"
                rows="16"
                cols="80"
                prop:value=move || payload.get()
                on:input=move |event| payload.set(event_target_value(&event))
            ></textarea>
            </crate::ui::Disclosure>
            <h3>"Your proposed change"</h3>
            {preview}
        </section>
        <section aria-labelledby="mapping" hidden=move || kind.get() != ProposalKind::Mapping>
            <h2 id="mapping">"Which film simulation was this shot with?"</h2>
            <p>"Choose several original camera files, then label each with its film simulation. Photos stay on this device."</p>
            <crate::ui::DropZone on_files=read_mapping accept=".jpg,.jpeg,.raf".to_owned() folders=false/>
            {move || labeled.get().into_iter().enumerate().map(|(index, file)| view! {
                <label for=format!("mapping-answer-{index}")>{file.name}</label>
                <input id=format!("mapping-answer-{index}") value=file.answer placeholder="For example: Classic Chrome" on:input=move |event| {
                    labeled.update(|files| files[index].answer = event_target_value(&event));
                    let text = labeled.get().iter().filter(|file| !file.answer.is_empty()).map(|file| format!("{} {}", file.answer.to_lowercase().replace([' ', '-'], "_"), file.parsed_values)).collect::<Vec<_>>().join("\n");
                    if let Ok(inferred) = proposal_form::parse_labeled_files(&text)
                        && let Some(best) = inferred.best()
                        && let Some((_, value)) = best.pairs.iter().find(|(label, _)| payload.get().lines().any(|line| line.strip_prefix("sim_key:").is_some_and(|value| value.trim().trim_matches('"') == label.as_str())))
                    {
                        payload.update(|text| *text = text.lines().map(|line| if line.starts_with("makernote:") { format!("makernote: {{ tag: {}, value: {} }}", best.tag, value.0) } else { line.to_owned() }).collect::<Vec<_>>().join("\n"));
                    }
                    files_text.set(text);
                }/>
            }).collect_view()}
            <crate::ui::Disclosure summary="Advanced: parsed file information".to_owned()>
            <label for="mapping-files">"One file per line: answer Tag=value ..."</label>
            <textarea
                id="mapping-files"
                rows="5"
                cols="60"
                prop:value=move || files_text.get()
                on:input=move |event| files_text.set(event_target_value(&event))
            ></textarea>
            {inference}
            </crate::ui::Disclosure>
        </section>
        <section aria-labelledby="fixtures">
            <h2 id="fixtures">"Donate sample files"</h2>
            <p>"Choose each file you want to share under CC0. Only checked files leave this device, and they stay private until your proposal is approved. Without a sample file, a mapping is marked as not yet confirmed."</p>
            <crate::ui::DropZone on_files=on_fixtures/>
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
            <p>"Sign in to send your proposal. Preparing it keeps spam out. A moderator will review it before camera data changes."</p>
            {(!available).then(crate::ui::preview_notice)}
            <button type="button" id="submit-proposal" disabled=move || busy.get() || !available on:click=submit>
                {move || if busy.get() { "Preparing proposal…" } else { "Submit proposal" }}
            </button>
            {move || {
                pending
                    .get()
                    .map(|selected| {
                        let rows = selected
                            .files
                            .iter()
                            .map(|file| {
                                view! {
                                    <tr>
                                        <th scope="row">{file.name.clone()}</th>
                                        <td>{file.bytes.len().to_string()}</td>
                                        <td>{file.sha256.to_hex()}</td>
                                    </tr>
                                }
                            })
                            .collect_view();
                        let count = selected.files.len();
                        view! {
                            <div class="valid" id="donation-confirm">
                                <h3>"Files that leave this device"</h3>
                                <p>
                                    {format!(
                                        "These {count} file(s) are uploaded now, stored privately, and published as CC0-1.0 fixtures only if a moderator approves this proposal.",
                                    )}
                                </p>
                                <table class="data-table">
                                    <caption>"Donated files"</caption>
                                    <thead>
                                        <tr>
                                            <th scope="col">"File"</th>
                                            <th scope="col">"Bytes"</th>
                                            <th scope="col">"sha256"</th>
                                        </tr>
                                    </thead>
                                    <tbody>{rows}</tbody>
                                </table>
                                <div class="field-row">
                                    <button
                                        type="button"
                                        id="confirm-donation"
                                        disabled=move || busy.get() || !available
                                        on:click=confirm_donations
                                    >
                                        "Upload files and submit"
                                    </button>
                                    <button
                                        type="button"
                                        id="cancel-donation"
                                        disabled=move || busy.get()
                                        on:click=cancel_donations
                                    >
                                        "Cancel"
                                    </button>
                                </div>
                            </div>
                        }
                    })
            }}
            {move || {
                message
                    .get()
                    .map(|text| view! { <p class="error" role="alert">{text}</p> })
            }}
            {move || {
                submitted
                    .get()
                    .map(|proposal| {
                        let href =
                            crate::base::site_url(&format!("/proposals/{}", proposal.id.as_str()));
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

#[derive(Clone, Copy)]
enum MenuField {
    Camera,
    Make,
    Model,
    Name,
    Look,
    Simulation,
    Firmware,
    Attribution,
    Identifier,
}

impl MenuField {
    fn key(self) -> &'static str {
        match self {
            Self::Camera => "body",
            Self::Make => "make",
            Self::Model => "model",
            Self::Name => "name",
            Self::Look => "look",
            Self::Simulation => "sim_key",
            Self::Firmware => "firmware_from",
            Self::Attribution => "attribution",
            Self::Identifier => "id",
        }
    }
    fn display_value(self, value: &str) -> String {
        match self {
            Self::Simulation => value.replace('_', " "),
            _ => value.to_owned(),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Camera => "Camera (make/model)",
            Self::Make => "Camera make",
            Self::Model => "Camera model",
            Self::Name => "Name",
            Self::Look => "Look name in links",
            Self::Simulation => "Film simulation in the camera menu",
            Self::Firmware => "Available from firmware version",
            Self::Attribution => "Credit the author",
            Self::Identifier => "Name in links",
        }
    }
}

#[component]
fn MenuFields(kind: RwSignal<ProposalKind>, payload: RwSignal<String>) -> impl IntoView {
    view! {
        <div class="proposal-fields">
        {move || (kind.get() == ProposalKind::SchemaChange).then(|| view! { <ControlFields payload/> })}
        {move || {
            let fields = match kind.get() {
                ProposalKind::Body => vec![MenuField::Identifier, MenuField::Make, MenuField::Model],
                ProposalKind::Mapping => vec![MenuField::Camera, MenuField::Simulation, MenuField::Look, MenuField::Firmware],
                ProposalKind::SchemaChange => vec![MenuField::Camera],
                ProposalKind::LutLook => vec![MenuField::Look, MenuField::Name, MenuField::Attribution],
                ProposalKind::InterfaceProfile => vec![MenuField::Identifier, MenuField::Name],
                _ => vec![],
            };
            fields.into_iter().map(|field| {
                let key = field.key();
                let id = format!("proposal-{key}");
                view! {
                    <label for=id.clone()>{field.label()}</label>
                    <input id=id prop:value=move || payload.get().lines().find_map(|line| line.strip_prefix(&format!("{key}:")).map(|value| field.display_value(value.trim().trim_matches('"')))).unwrap_or_default()
                        on:input=move |event| {
                            let input = event_target_value(&event);
                            let input = if matches!(field, MenuField::Simulation) { input.to_lowercase().replace([' ', '-'], "_") } else { input };
                            let value = serde_json::to_string(&input).unwrap_or_default();
                            payload.update(|text| {
                                let prefix = format!("{key}:");
                                *text = text.lines().map(|line| if line.starts_with(&prefix) { format!("{key}: {value}") } else { line.to_owned() }).collect::<Vec<_>>().join("\n");
                            });
                        }/>
                    {move || validate_proposal(kind.get(), &payload.get()).err().map(|errors| errors.into_iter().filter(|error| error.path == key || error.path.starts_with(&format!("{key}."))).map(|error| view! { <p class="error" role="alert">{error.kind.to_string()}</p> }).collect_view())}
                }
            }).collect_view()
        }}
        {move || (kind.get() == ProposalKind::LutLook).then(|| view! { <label class="consent"><input type="checkbox" checked on:change=move |event| {
            let own = event_target_checked(&event);
            payload.update(|text| *text = text.lines().map(|line| if line.starts_with("license:") { format!("license: {{ own_work: {own} }}") } else { line.to_owned() }).collect::<Vec<_>>().join("\n"));
        }/>"I made this look and may share it under the data license."</label><label for="look-source-license">"Or name the license that allows redistribution"</label><input id="look-source-license" placeholder="For example: CC0-1.0" on:input=move |event| {
            let license = serde_json::to_string(&event_target_value(&event)).unwrap_or_default();
            payload.update(|text| *text = text.lines().map(|line| if line.starts_with("license:") { format!("license: {{ own_work: false, source_license: {license} }}") } else { line.to_owned() }).collect::<Vec<_>>().join("\n"));
        }/> })}
        </div>
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlField {
    Range,
    Step,
    Default,
    Choices,
}

impl ControlField {
    fn parse(line: &str) -> Option<Self> {
        match line.split_once(':')?.0.trim() {
            "range" => Some(Self::Range),
            "step" => Some(Self::Step),
            "default" => Some(Self::Default),
            "values" => Some(Self::Choices),
            _ => None,
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Range => "range",
            Self::Step => "step",
            Self::Default => "default",
            Self::Choices => "values",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Range => "Minimum and maximum",
            Self::Step => "Step size",
            Self::Default => "Default setting",
            Self::Choices => "Menu choices",
        }
    }
}

#[component]
fn ControlFields(payload: RwSignal<String>) -> impl IntoView {
    let rows = Memo::new(move |_| {
        let text = payload.get();
        let mut in_controls = false;
        let mut control = String::new();
        let mut firmware = String::new();
        let mut rows = Vec::new();
        let mut epoch_index: Option<usize> = None;
        let mut control_index: Option<usize> = None;
        for (index, line) in text.lines().enumerate() {
            if line.starts_with("  - id:") {
                epoch_index = Some(epoch_index.map_or(0, |index| index + 1));
            }
            if let Some(value) = line.trim().strip_prefix("firmware:") {
                firmware = value
                    .replace(['{', '}', '"'], "")
                    .replace("from:", "")
                    .replace(", to:", " to")
                    .trim()
                    .to_owned();
            }
            if line.trim() == "controls:" {
                in_controls = true;
                control_index = None;
                continue;
            }
            if !in_controls {
                continue;
            }
            if line.starts_with("    ") && !line.starts_with("      ") {
                in_controls = false;
                continue;
            }
            if let Some(key) = line.trim().strip_prefix("- key:") {
                control = key.trim().replace('_', " ");
                control_index = Some(control_index.map_or(0, |index| index + 1));
            }
            if let Some(field) = ControlField::parse(line)
                && line.starts_with("        ")
                && !line.starts_with("          ")
            {
                rows.push((
                    index,
                    control.clone(),
                    firmware.clone(),
                    field,
                    line.split_once(':')
                        .map(|(_, value)| value.trim().to_owned())
                        .unwrap_or_default(),
                    format!(
                        "epochs[{}].controls[{}].{}",
                        epoch_index.unwrap_or_default(),
                        control_index.unwrap_or_default(),
                        field.key()
                    ),
                ));
            }
        }
        rows
    });
    view! { <section><h3>"Camera controls"</h3><p>"Edit settings in camera menu order. Enter ranges as [minimum, maximum], and menu choices in brackets."</p>
        {move || rows.get().into_iter().map(|(index, control, firmware, field, value, path)| view! {
            <label for=format!("control-{index}")>{format!("{} · {}", control, field.label())}</label>
            <small class="muted">{format!("Firmware versions {firmware}")}</small>
            <input id=format!("control-{index}") value=value on:change=move |event| {
                let value = event_target_value(&event);
                payload.update(|text| *text = text.lines().enumerate().map(|(line_index, line)| {
                    if line_index == index { format!("{}: {value}", line.split_once(':').map(|(key, _)| key).unwrap_or_default()) } else { line.to_owned() }
                }).collect::<Vec<_>>().join("\n"));
            }/>
            {move || validate_proposal(ProposalKind::SchemaChange, &payload.get())
                .err()
                .map(|errors| errors.into_iter()
                    .filter(|error| error.path == path || error.path.starts_with(&format!("{path}[")))
                    .map(|error| view! {
                        <p class="error" role="alert">{error.kind.to_string()}</p>
                    })
                    .collect_view())}
        }).collect_view()}
    </section> }
}
