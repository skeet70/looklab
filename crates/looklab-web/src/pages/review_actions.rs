// SPDX-License-Identifier: AGPL-3.0-or-later

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_trust::{EvidenceRef, Grant, Scope, ScopeRule};
use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct RecordedAction {}
use crate::base::api_url;
use crate::request_digest::RequestDomain;
use crate::{data, session::use_session, ui, webauthn};

macro_rules! internal_id {
    ($name:ident) => {
        #[derive(Clone, Serialize)]
        #[serde(transparent)]
        struct $name(String);
        impl $name {
            fn parse(value: &str) -> Result<Self, String> {
                let value = value.trim();
                if value.len() != 36 || !value.bytes().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) { byte == b'-' } else { byte.is_ascii_hexdigit() }
                }) {
                    return Err("Enter the internal ID in UUID form, copied from the review queue or public list.".to_owned());
                }
                Ok(Self(value.to_ascii_lowercase()))
            }
        }
    };
}
internal_id!(SubjectId);
internal_id!(ContributionId);
internal_id!(RootAdmissionId);

#[derive(Clone, Serialize)]
#[serde(transparent)]
struct Reason(String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Endorse,
    RevokeEndorsement,
    AdmitRoot,
    RevokeRoot,
    Grant,
    RevokeGrant,
}

impl ActionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Endorse => "Endorse a contributor",
            Self::RevokeEndorsement => "Revoke an endorsement",
            Self::AdmitRoot => "Admit reference data",
            Self::RevokeRoot => "Revoke reference data",
            Self::Grant => "Grant a role or seeder tier",
            Self::RevokeGrant => "Revoke a role or seeder tier",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Endorse => "endorse",
            Self::RevokeEndorsement => "revoke-endorsement",
            Self::AdmitRoot => "admit-root",
            Self::RevokeRoot => "revoke-root",
            Self::Grant => "grant",
            Self::RevokeGrant => "revoke-grant",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        [
            Self::Endorse,
            Self::RevokeEndorsement,
            Self::AdmitRoot,
            Self::RevokeRoot,
            Self::Grant,
            Self::RevokeGrant,
        ]
        .into_iter()
        .find(|action| action.key() == value)
    }
    fn allowed(self, seeder: bool, admin: bool) -> bool {
        match self {
            Self::Endorse | Self::AdmitRoot => seeder,
            Self::RevokeEndorsement | Self::RevokeRoot => seeder || admin,
            Self::Grant | Self::RevokeGrant => admin,
        }
    }
    fn scoped(self) -> bool {
        matches!(self, Self::Endorse | Self::AdmitRoot | Self::Grant)
    }
    fn domain(self) -> RequestDomain {
        match self {
            Self::Endorse => RequestDomain::Endorse,
            Self::RevokeEndorsement => RequestDomain::RevokeEndorsement,
            Self::AdmitRoot => RequestDomain::AdmitRoot,
            Self::RevokeRoot => RequestDomain::RevokeRoot,
            Self::Grant => RequestDomain::Grant,
            Self::RevokeGrant => RequestDomain::RevokeGrant,
        }
    }
    fn path(self) -> &'static str {
        match self {
            Self::Endorse => "/api/seeder/endorse",
            Self::RevokeEndorsement => "/api/seeder/revoke-endorsement",
            Self::AdmitRoot => "/api/seeder/admit-root",
            Self::RevokeRoot => "/api/seeder/revoke-root",
            Self::Grant => "/api/admin/grant",
            Self::RevokeGrant => "/api/admin/revoke-grant",
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum ActionBody {
    Endorse {
        subject: SubjectId,
        scope: Scope,
        reason: Reason,
        evidence: Vec<EvidenceRef>,
    },
    RevokeEndorsement {
        subject: SubjectId,
        reason: Reason,
    },
    AdmitRoot {
        contribution_id: ContributionId,
        scope: Scope,
        reason: Reason,
    },
    RevokeRoot {
        root_admission_id: RootAdmissionId,
        reason: Reason,
    },
    Grant {
        subject: SubjectId,
        grant: Grant,
        scope: Option<Scope>,
        reason: Reason,
    },
    RevokeGrant {
        subject: SubjectId,
        grant: Grant,
        reason: Reason,
    },
}

fn grant_key(grant: Grant) -> &'static str {
    match grant {
        Grant::Seeder => "seeder",
        Grant::Moderator => "moderator",
        Grant::Admin => "admin",
    }
}

const MAX_EVIDENCE_REFS: usize = 20;

fn evidence_refs(text: &str) -> Result<Vec<EvidenceRef>, String> {
    let mut refs = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let reference = normalize_evidence(line)?;
        if seen.insert(reference.clone()) {
            refs.push(EvidenceRef::new(reference));
        }
    }
    if refs.len() > MAX_EVIDENCE_REFS {
        return Err(format!(
            "Use at most {MAX_EVIDENCE_REFS} evidence references."
        ));
    }
    Ok(refs)
}

fn normalize_evidence(line: &str) -> Result<String, String> {
    for prefix in ["contribution:", "image:"] {
        if let Some(id) = line.strip_prefix(prefix) {
            return uuid::Uuid::parse_str(id)
                .map(|id| format!("{prefix}{}", id.hyphenated()))
                .map_err(|_| format!("{line}: expected {prefix}<uuid>"));
        }
    }
    if let Ok(id) = uuid::Uuid::parse_str(line) {
        return Ok(id.hyphenated().to_string());
    }
    Ok(line.to_owned())
}

fn bind(parts: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(parts.iter().map(|part| 8 + part.len()).sum());
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part);
    }
    bytes
}

impl ActionBody {
    fn binding(&self) -> Result<Vec<u8>, String> {
        let json = |value: &Scope| serde_json::to_vec(value).map_err(|error| error.to_string());
        Ok(match self {
            Self::Endorse {
                subject,
                scope,
                reason,
                evidence,
            } => bind(&[
                subject.0.as_bytes(),
                &json(scope)?,
                reason.0.as_bytes(),
                &serde_json::to_vec(evidence).map_err(|error| error.to_string())?,
            ]),
            Self::RevokeEndorsement { subject, reason } => {
                bind(&[subject.0.as_bytes(), reason.0.as_bytes()])
            }
            Self::AdmitRoot {
                contribution_id,
                scope,
                reason,
            } => bind(&[
                contribution_id.0.as_bytes(),
                &json(scope)?,
                reason.0.as_bytes(),
            ]),
            Self::RevokeRoot {
                root_admission_id,
                reason,
            } => bind(&[root_admission_id.0.as_bytes(), reason.0.as_bytes()]),
            Self::Grant {
                subject,
                grant,
                scope,
                reason,
            } => {
                let scope = scope.as_ref().map(json).transpose()?.unwrap_or_default();
                bind(&[
                    subject.0.as_bytes(),
                    grant_key(*grant).as_bytes(),
                    &scope,
                    reason.0.as_bytes(),
                ])
            }
            Self::RevokeGrant {
                subject,
                grant,
                reason,
            } => bind(&[
                subject.0.as_bytes(),
                grant_key(*grant).as_bytes(),
                reason.0.as_bytes(),
            ]),
        })
    }
}

#[derive(Clone)]
struct Preview {
    kind: ActionKind,
    body: ActionBody,
    line: String,
    target: String,
}

#[derive(Serialize)]
struct SignedBody<'a> {
    #[serde(flatten)]
    signed: webauthn::SignedFields,
    #[serde(flatten)]
    body: &'a ActionBody,
}

#[component]
pub(super) fn TrustActions(seeder: bool, admin: bool) -> impl IntoView {
    let context = use_session();
    let kind = RwSignal::new(if seeder {
        ActionKind::Endorse
    } else {
        ActionKind::Grant
    });
    let target = RwSignal::new(String::new());
    let public_label = RwSignal::new(String::new());
    let reason = RwSignal::new(String::new());
    let evidence = RwSignal::new(String::new());
    let grant = RwSignal::new(Grant::Seeder);
    let body = RwSignal::new(String::new());
    let look = RwSignal::new(String::new());
    let unrestricted = RwSignal::new(false);
    let catalog = RwSignal::new(None::<looklab_publish::SiteIndex>);
    let catalog_error = RwSignal::new(None::<String>);
    let target_error = RwSignal::new(None::<String>);
    let scope_error = RwSignal::new(None::<String>);
    let evidence_error = RwSignal::new(None::<String>);
    let reason_error = RwSignal::new(None::<String>);
    let status = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let dialog = RwSignal::new(false);
    let pending = RwSignal::new(None::<Preview>);
    spawn_local(async move {
        match data::index().await {
            Ok(index) => catalog.set(Some(index)),
            Err(message) => catalog_error.set(Some(message)),
        }
    });
    let preview = move |_| {
        target_error.set(None);
        scope_error.set(None);
        evidence_error.set(None);
        reason_error.set(None);
        error.set(None);
        status.set(None);
        let action = kind.get_untracked();
        if !action.allowed(seeder, admin) {
            error.set(Some("Your profile cannot perform this action.".to_owned()));
            return;
        }
        let reason_text = reason.get_untracked().trim().to_owned();
        if reason_text.is_empty() {
            reason_error.set(Some("Explain the decision before reviewing it.".to_owned()));
            return;
        }
        let parsed_scope =
            if action.scoped() && !(action == ActionKind::Grant && unrestricted.get_untracked()) {
                let parsed = (|| {
                    let camera = body.get_untracked();
                    let look = look.get_untracked();
                    let camera = if camera.is_empty() {
                        None
                    } else {
                        Some(BodyId::try_from(camera).map_err(|_| "Choose a valid camera.")?)
                    };
                    let look = if look.is_empty() {
                        None
                    } else {
                        Some(LookSlug::try_from(look).map_err(|_| "Choose a valid look.")?)
                    };
                    Ok::<_, &str>(Scope::new([ScopeRule::new(camera, look)]))
                })();
                match parsed {
                    Ok(scope) => Some(scope),
                    Err(message) => {
                        scope_error.set(Some(message.to_owned()));
                        return;
                    }
                }
            } else {
                None
            };
        let target_text = target.get_untracked();
        let reason = Reason(reason_text.clone());
        let evidence_list = if action == ActionKind::Endorse {
            match evidence_refs(&evidence.get_untracked()) {
                Ok(refs) => refs,
                Err(message) => {
                    evidence_error.set(Some(message));
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let built = (|| {
            Ok::<_, String>(match action {
                ActionKind::Endorse => ActionBody::Endorse {
                    subject: SubjectId::parse(&target_text)?,
                    scope: parsed_scope.clone().ok_or("Choose a scope.")?,
                    reason,
                    evidence: evidence_list,
                },
                ActionKind::RevokeEndorsement => ActionBody::RevokeEndorsement {
                    subject: SubjectId::parse(&target_text)?,
                    reason,
                },
                ActionKind::AdmitRoot => ActionBody::AdmitRoot {
                    contribution_id: ContributionId::parse(&target_text)?,
                    scope: parsed_scope.clone().ok_or("Choose a scope.")?,
                    reason,
                },
                ActionKind::RevokeRoot => ActionBody::RevokeRoot {
                    root_admission_id: RootAdmissionId::parse(&target_text)?,
                    reason,
                },
                ActionKind::Grant => ActionBody::Grant {
                    subject: SubjectId::parse(&target_text)?,
                    grant: grant.get_untracked(),
                    scope: parsed_scope.clone(),
                    reason,
                },
                ActionKind::RevokeGrant => ActionBody::RevokeGrant {
                    subject: SubjectId::parse(&target_text)?,
                    grant: grant.get_untracked(),
                    reason,
                },
            })
        })();
        let body = match built {
            Ok(body) => body,
            Err(message) => {
                target_error.set(Some(message));
                return;
            }
        };
        let label = public_label.get_untracked();
        let label = if label.trim().is_empty() {
            "the selected record"
        } else {
            label.trim()
        };
        let scope_label = parsed_scope.as_ref().map(|scope| {
            scope
                .rules
                .iter()
                .map(ScopeRule::render)
                .collect::<Vec<_>>()
                .join("; ")
        });
        let verb = match action {
            ActionKind::Endorse => "Endorsed".to_owned(),
            ActionKind::RevokeEndorsement => "Revoked endorsement for".to_owned(),
            ActionKind::AdmitRoot => "Admitted reference data from".to_owned(),
            ActionKind::RevokeRoot => "Revoked reference admission for".to_owned(),
            ActionKind::Grant => format!("Granted {} to", grant_key(grant.get_untracked())),
            ActionKind::RevokeGrant => format!("Revoked {} from", grant_key(grant.get_untracked())),
        };
        let scope_line = scope_label
            .map(|scope| format!(" Scope: {scope}."))
            .unwrap_or_else(|| {
                if action == ActionKind::Grant {
                    " Scope: unrestricted.".to_owned()
                } else {
                    String::new()
                }
            });
        pending.set(Some(Preview {
            kind: action,
            body,
            line: format!("{verb} {label}.{scope_line} Reason: {reason_text}"),
            target: target_text.trim().to_ascii_lowercase(),
        }));
        dialog.set(true);
    };
    let submit = move |_| {
        if busy.get_untracked() {
            return;
        }
        let Some(preview) = pending.get_untracked() else {
            return;
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            dialog.set(false);
            let result = async {
                let credential = webauthn::profile_credential(context).await?;
                let crate::session::SessionState::SignedIn(session) = context.state.get_untracked() else {
                    return Err("Sign in to your profile before confirming this action.".to_owned());
                };
                let is_seeder = session.tier == webauthn::SessionTier::Seeder;
                let is_admin = session.roles.contains(&webauthn::SessionRole::Admin);
                if !preview.kind.allowed(is_seeder && seeder, is_admin && admin) {
                    return Err("Your current profile does not have permission for this action.".to_owned());
                }
                let binding = preview.body.binding()?;
                let signed = webauthn::sign_request(preview.kind.domain(), &binding, Some(&credential)).await?;
                let response = Request::post(&api_url(preview.kind.path()))
                    .json(&SignedBody { signed: signed.fields, body: &preview.body }).map_err(|error| error.to_string())?
                    .send().await.map_err(|error| error.to_string())?;
                if !response.ok() { return Err(format!("The action was not recorded (HTTP {}). Check your role, scope and target, then review and confirm again.", response.status())); }
                response.json::<RecordedAction>().await
                    .map_err(|_| "The server did not return a recorded action. Review the public log before trying again.".to_owned())?;
                Ok(())
            }.await;
            busy.set(false);
            match result {
                Ok(()) => {
                    status.set(Some("Action recorded in the public audit log.".to_owned()));
                    pending.set(None);
                    reason.set(String::new());
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <section class="trust-actions" hidden=!(seeder || admin)>
            <h3>"Trust actions"</h3>
            <p>"Every action is public and requires a reason and a fresh passkey confirmation."</p>
            <form on:submit=move |event| {
                event.prevent_default();
                preview(event);
            }>
                <fieldset disabled=move || busy.get()>
                    <div class="review-form-grid">
                        <label>
                            "Action"
                            <select prop:value=move || kind.get().key() on:change=move |event| {
                                if let Some(action) = ActionKind::parse(&event_target_value(&event))
                                    .filter(|action| action.allowed(seeder, admin))
                                {
                                    kind.set(action);
                                }
                            }>
                                {[ActionKind::Endorse, ActionKind::RevokeEndorsement,
                                    ActionKind::AdmitRoot, ActionKind::RevokeRoot,
                                    ActionKind::Grant, ActionKind::RevokeGrant]
                                    .into_iter()
                                    .filter(|action| action.allowed(seeder, admin))
                                    .map(|action| view! {
                                        <option value=action.key()>{action.label()}</option>
                                    }).collect_view()}
                            </select>
                        </label>
                        <label>
                            "Public display name or record label (optional)"
                            <input
                                prop:value=move || public_label.get()
                                on:input=move |event| public_label.set(event_target_value(&event))
                            />
                        </label>
                    </div>
                    <ui::Disclosure summary="Technical details".to_owned() open=true>
                        <label>
                            {move || match kind.get() {
                                ActionKind::AdmitRoot => "Contribution ID",
                                ActionKind::RevokeRoot => "Reference admission ID",
                                _ => "Contributor internal ID",
                            }}
                            <input
                                required=true
                                aria-invalid=move || target_error.get().is_some().to_string()
                                prop:value=move || target.get()
                                on:input=move |event| target.set(event_target_value(&event))
                            />
                        </label>
                        {move || target_error.get().map(|message| view! {
                            <p class="error" role="alert">{message}</p>
                        })}
                        <p class="muted">
                            "Use the internal ID from the review queue or public list, not a passkey credential or public key."
                        </p>
                    </ui::Disclosure>
                    <div hidden=move || !matches!(kind.get(), ActionKind::Grant | ActionKind::RevokeGrant)>
                        <label>
                            "Role or tier"
                            <select on:change=move |event| {
                                match event_target_value(&event).as_str() {
                                    "seeder" => grant.set(Grant::Seeder),
                                    "moderator" => grant.set(Grant::Moderator),
                                    "admin" => grant.set(Grant::Admin),
                                    _ => {}
                                }
                            }>
                                <option value="seeder">"Seeder"</option>
                                <option value="moderator">"Moderator"</option>
                                <option value="admin">"Administrator"</option>
                            </select>
                        </label>
                    </div>
                    <div hidden=move || !kind.get().scoped()>
                        <h4>"Applies to"</h4>
                        <label hidden=move || kind.get() != ActionKind::Grant>
                            <input
                                type="checkbox"
                                prop:checked=move || unrestricted.get()
                                on:change=move |event| unrestricted.set(event_target_checked(&event))
                            />
                            "Unrestricted grant"
                        </label>
                        <div hidden=move || kind.get() == ActionKind::Grant && unrestricted.get()>
                            <div class="review-form-grid">
                                <label>
                                    "Camera"
                                    <select
                                        prop:value=move || body.get()
                                        on:change=move |event| body.set(event_target_value(&event))
                                    >
                                        <option value="">"Any camera"</option>
                                        {move || catalog.get().map(|index| index.bodies.into_iter()
                                            .map(|camera| view! {
                                                <option value=camera.id.to_string()>
                                                    {format!("{} {}", camera.make, camera.model)}
                                                </option>
                                            }).collect_view())}
                                    </select>
                                </label>
                                <label>
                                    "Look"
                                    <select
                                        prop:value=move || look.get()
                                        on:change=move |event| look.set(event_target_value(&event))
                                    >
                                        <option value="">"Any look"</option>
                                        {move || catalog.get().map(|index| index.looks.into_iter()
                                            .map(|look| view! {
                                                <option value=look.slug.to_string()>{look.name}</option>
                                            }).collect_view())}
                                    </select>
                                </label>
                            </div>
                            <p class="muted">
                                "Both choices must match. Any camera and any look covers all contributions."
                            </p>
                            {move || catalog_error.get().map(|message| view! {
                                <p class="error" role="alert">
                                    {format!("Camera and look choices could not load: {message}")}
                                </p>
                            })}
                        </div>
                        {move || scope_error.get().map(|message| view! {
                            <p class="error" role="alert">{message}</p>
                        })}
                    </div>
                    <div hidden=move || kind.get() != ActionKind::Endorse>
                        <label for="endorse-evidence">
                            "Evidence references (one per line, optional)"
                        </label>
                        <textarea
                            id="endorse-evidence"
                            rows="2"
                            prop:value=move || evidence.get()
                            on:input=move |event| evidence.set(event_target_value(&event))
                        ></textarea>
                        <p class="muted">
                            "Use contribution:<uuid>, image:<uuid>, or a bare contribution or image id. Chart-session labels are also kept as human evidence. Deferred monochrome contributions and their images cannot be endorsed."
                        </p>
                        {move || evidence_error.get().map(|message| view! {
                            <p class="error" role="alert">{message}</p>
                        })}
                    </div>
                    <label>
                        "Public reason (required)"
                        <textarea
                            required=true
                            rows="3"
                            aria-invalid=move || reason_error.get().is_some().to_string()
                            prop:value=move || reason.get()
                            on:input=move |event| reason.set(event_target_value(&event))
                        ></textarea>
                    </label>
                    {move || reason_error.get().map(|message| view! {
                        <p class="error" role="alert">{message}</p>
                    })}
                    <button type="submit">"Review public audit entry"</button>
                </fieldset>
            </form>
            <ui::Dialog title="Confirm trust action".to_owned() open=dialog>
                <h3>"Public audit-log preview"</h3>
                <p>
                    {move || pending.get().map(|preview| preview.line).unwrap_or_default()}
                </p>
                <p>
                    "Your profile is recorded as the actor. Display labels here are for review; the public record identifies the exact target below."
                </p>
                <ui::Disclosure summary="Technical details".to_owned()>
                    <p>
                        {move || pending.get()
                            .map(|preview| format!("Target: {}", preview.target))
                            .unwrap_or_default()}
                    </p>
                    <pre>
                        {move || pending.get()
                            .and_then(|preview| serde_json::to_string_pretty(&preview.body).ok())
                            .unwrap_or_default()}
                    </pre>
                </ui::Disclosure>
                <button type="button" disabled=move || busy.get() on:click=submit>
                    "Confirm with passkey and submit"
                </button>
            </ui::Dialog>
            {move || busy.get().then(|| view! {
                <p role="status">"Confirm the action with your passkey…"</p>
            })}
            {move || error.get().map(|message| view! {
                <p class="error" role="alert">{message}</p>
            })}
            {move || status.get().map(|message| view! {
                <p class="valid" role="status">
                    {message}
                    <a href=crate::base::site_url("/community/log")>" View public audit log"</a>
                </p>
            })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_evidence_is_normalized_and_deduplicated() {
        let refs = evidence_refs(
            "contribution:01234567-89ab-cdef-0123-456789abcdef\n\
             image:0123456789abcdef0123456789abcdef\n\
             0123456789abcdef0123456789abcdef\n\
             contribution:0123456789abcdef0123456789abcdef",
        )
        .expect("valid evidence");

        assert_eq!(
            refs.iter().map(EvidenceRef::as_str).collect::<Vec<_>>(),
            vec![
                "contribution:01234567-89ab-cdef-0123-456789abcdef",
                "image:01234567-89ab-cdef-0123-456789abcdef",
                "01234567-89ab-cdef-0123-456789abcdef",
            ]
        );
    }

    #[test]
    fn human_evidence_labels_pass_through() {
        let refs = evidence_refs("chart-session-1\n\n  field-note: white end clipped  ")
            .expect("human labels are allowed");
        assert_eq!(
            refs.iter().map(EvidenceRef::as_str).collect::<Vec<_>>(),
            vec!["chart-session-1", "field-note: white end clipped"]
        );
    }

    #[test]
    fn malformed_internal_evidence_is_rejected() {
        assert!(evidence_refs("contribution:not-a-uuid").is_err());
        assert!(evidence_refs("image:1234").is_err());
        assert!(evidence_refs("contribution:").is_err());
    }

    #[test]
    fn empty_evidence_sends_nothing() {
        assert!(evidence_refs(" \n\t ").expect("empty evidence").is_empty());
    }

    #[test]
    fn too_many_evidence_references_are_rejected() {
        let text = (0..=MAX_EVIDENCE_REFS)
            .map(|index| format!("chart-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(evidence_refs(&text).is_err());
    }
}
