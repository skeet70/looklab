// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owner confirmation and correction of documentation-derived camera controls.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_publish::{ManualCapabilityPage, ManualPage};
use looklab_schema::{
    Availability, Axis, BodyId, CapabilityDomain, CapabilityKey, ConfirmationBasis,
    ConfirmationScope, ControlObservation, Finding, ManualConfirmationSummary, ManualUrl,
    ModeratorEvidence, ModeratorEvidenceKind, ModeratorFirmwareScope, Rational, ReportedFirmware,
    Sha256Hex,
};
use looklab_upload::AttestationHash;
use looklab_wasm::prep::ModuleFunction;
use looklab_wasm::{
    ManualConfirmationEnvelope, build_manual_confirmation, build_manual_moderation,
    build_send_manifest_request, decode_build_send_manifest_reply, module_fn_id,
};

use super::body::{
    availability_text, category_text, confirmation_text, control_actual_text, documentation_note,
    domain_text, finding_text, firmware_actual_text, firmware_basis_text,
    firmware_definition_digest, firmware_detail, moderator_evidence_text,
};
use crate::session::SessionContext;
use crate::worker::PrepWorker;
use crate::{data, idb, webauthn};

/// The answer an owner gives for one documented control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Matches,
    Corrected,
    Unavailable,
}

impl fmt::Display for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Matches => "matches",
            Self::Corrected => "corrected",
            Self::Unavailable => "unavailable",
        })
    }
}

impl FromStr for Answer {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "matches" => Ok(Self::Matches),
            "corrected" => Ok(Self::Corrected),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DomainKindDraft {
    Unspecified,
    Boolean,
    Enum,
    Integer,
    Rational,
    Axes,
}

impl fmt::Display for DomainKindDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unspecified => "unspecified",
            Self::Boolean => "boolean",
            Self::Enum => "enum",
            Self::Integer => "integer",
            Self::Rational => "rational",
            Self::Axes => "axes",
        })
    }
}

impl FromStr for DomainKindDraft {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unspecified" => Ok(Self::Unspecified),
            "boolean" => Ok(Self::Boolean),
            "enum" => Ok(Self::Enum),
            "integer" => Ok(Self::Integer),
            "rational" => Ok(Self::Rational),
            "axes" => Ok(Self::Axes),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKindDraft {
    Unspecified,
    Exact,
    Range,
}

impl fmt::Display for ScopeKindDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unspecified => "unspecified",
            Self::Exact => "exact",
            Self::Range => "range",
        })
    }
}

impl FromStr for ScopeKindDraft {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unspecified" => Ok(Self::Unspecified),
            "exact" => Ok(Self::Exact),
            "range" => Ok(Self::Range),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AxisDraft {
    label: String,
    min: String,
    max: String,
    step: String,
}

#[derive(Clone, Debug, PartialEq)]
enum DomainDraft {
    Unspecified,
    Boolean,
    Enum {
        values: String,
    },
    Integer {
        min: String,
        max: String,
        steps: String,
    },
    Rational {
        min: String,
        max: String,
        steps: String,
    },
    Axes {
        axes: Vec<AxisDraft>,
    },
}

impl DomainDraft {
    fn from_domain(domain: &CapabilityDomain) -> Self {
        match domain {
            CapabilityDomain::Boolean { .. } => Self::Boolean,
            CapabilityDomain::Enum { values, .. } => Self::Enum {
                values: values.join("\n"),
            },
            CapabilityDomain::Integer {
                min,
                max,
                step,
                steps,
                ..
            } => Self::Integer {
                min: min.to_string(),
                max: max.to_string(),
                steps: step.as_ref().map(ToString::to_string).unwrap_or_else(|| {
                    steps
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                }),
            },
            CapabilityDomain::Rational {
                min,
                max,
                step,
                steps,
                ..
            } => Self::Rational {
                min: min.clone(),
                max: max.clone(),
                steps: step.clone().unwrap_or_else(|| steps.join(", ")),
            },
            CapabilityDomain::Axes { axes } => Self::Axes {
                axes: axes
                    .iter()
                    .map(|axis| AxisDraft {
                        label: axis.label.clone(),
                        min: axis.min.map(|value| value.to_string()).unwrap_or_default(),
                        max: axis.max.map(|value| value.to_string()).unwrap_or_default(),
                        step: axis.step.map(|value| value.to_string()).unwrap_or_default(),
                    })
                    .collect(),
            },
            CapabilityDomain::Unspecified => Self::Unspecified,
        }
    }

    fn kind(&self) -> DomainKindDraft {
        match self {
            Self::Unspecified => DomainKindDraft::Unspecified,
            Self::Boolean => DomainKindDraft::Boolean,
            Self::Enum { .. } => DomainKindDraft::Enum,
            Self::Integer { .. } => DomainKindDraft::Integer,
            Self::Rational { .. } => DomainKindDraft::Rational,
            Self::Axes { .. } => DomainKindDraft::Axes,
        }
    }

    fn with_kind(kind: DomainKindDraft, current: &Self, base: &CapabilityDomain) -> Self {
        if current.kind() == kind {
            return current.clone();
        }
        let base = Self::from_domain(base);
        if base.kind() == kind {
            return base;
        }
        match kind {
            DomainKindDraft::Boolean => Self::Boolean,
            DomainKindDraft::Enum => Self::Enum {
                values: String::new(),
            },
            DomainKindDraft::Integer => Self::Integer {
                min: String::new(),
                max: String::new(),
                steps: String::new(),
            },
            DomainKindDraft::Rational => Self::Rational {
                min: String::new(),
                max: String::new(),
                steps: String::new(),
            },
            DomainKindDraft::Axes => Self::Axes {
                axes: vec![AxisDraft {
                    label: String::new(),
                    min: String::new(),
                    max: String::new(),
                    step: String::new(),
                }],
            },
            DomainKindDraft::Unspecified => Self::Unspecified,
        }
    }

    fn to_domain(&self) -> Result<CapabilityDomain, String> {
        match self {
            Self::Unspecified => Ok(CapabilityDomain::Unspecified),
            Self::Boolean => Ok(CapabilityDomain::Boolean { default: None }),
            Self::Enum { values } => {
                let values = values
                    .lines()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if values.is_empty() {
                    return Err("Enter at least one option.".to_owned());
                }
                let mut seen = BTreeSet::new();
                for value in &values {
                    if !seen.insert(value.clone()) {
                        return Err(format!("`{value}` is listed twice."));
                    }
                }
                Ok(CapabilityDomain::Enum {
                    values,
                    default: None,
                })
            }
            Self::Integer { min, max, steps } => {
                let min = min
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "Enter a whole-number minimum.".to_owned())?;
                let max = max
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "Enter a whole-number maximum.".to_owned())?;
                if min > max {
                    return Err("The minimum is above the maximum.".to_owned());
                }
                let steps = steps
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        let step = value
                            .parse::<i64>()
                            .map_err(|_| "Enter whole-number steps.".to_owned())?;
                        if step <= 0 {
                            return Err("Each step must be positive.".to_owned());
                        }
                        Ok(step)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let (step, steps) = if steps.is_empty() {
                    (None, Vec::new())
                } else if steps.len() == 1 {
                    (Some(steps[0]), Vec::new())
                } else {
                    (None, steps)
                };
                Ok(CapabilityDomain::Integer {
                    min,
                    max,
                    step,
                    steps,
                    default: None,
                })
            }
            Self::Rational { min, max, steps } => {
                let min_value = Rational::parse(min)
                    .map_err(|_| "Enter an exact minimum such as -5 or 1/3.".to_owned())?;
                let max_value = Rational::parse(max)
                    .map_err(|_| "Enter an exact maximum such as 5 or 1/3.".to_owned())?;
                if min_value > max_value {
                    return Err("The minimum is above the maximum.".to_owned());
                }
                let steps = steps
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        Rational::parse_step(value)
                            .map_err(|_| "Enter non-zero exact steps such as 1/3.".to_owned())?;
                        Ok(value.to_owned())
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let (step, steps) = if steps.is_empty() {
                    (None, Vec::new())
                } else if steps.len() == 1 {
                    (Some(steps[0].clone()), Vec::new())
                } else {
                    (None, steps)
                };
                Ok(CapabilityDomain::Rational {
                    min: min.trim().to_owned(),
                    max: max.trim().to_owned(),
                    step,
                    steps,
                    default: None,
                })
            }
            Self::Axes { axes } => {
                if axes.is_empty() {
                    return Err("Add at least one axis.".to_owned());
                }
                let mut labels = BTreeSet::new();
                let mut domains = Vec::with_capacity(axes.len());
                for axis in axes {
                    let label = axis.label.trim();
                    if label.is_empty() {
                        return Err("Enter an axis label.".to_owned());
                    }
                    if !labels.insert(label.to_owned()) {
                        return Err(format!("Axis `{label}` is listed twice."));
                    }
                    let (min, max, step) = if axis.min.trim().is_empty()
                        && axis.max.trim().is_empty()
                        && axis.step.trim().is_empty()
                    {
                        (None, None, None)
                    } else {
                        let min =
                            axis.min.trim().parse::<i64>().map_err(|_| {
                                format!("Axis `{label}` needs a whole-number minimum.")
                            })?;
                        let max =
                            axis.max.trim().parse::<i64>().map_err(|_| {
                                format!("Axis `{label}` needs a whole-number maximum.")
                            })?;
                        let step =
                            axis.step.trim().parse::<i64>().map_err(|_| {
                                format!("Axis `{label}` needs a whole-number step.")
                            })?;
                        if min > max {
                            return Err(format!("Axis `{label}` minimum is above its maximum."));
                        }
                        if step <= 0 {
                            return Err(format!("Axis `{label}` step must be positive."));
                        }
                        (Some(min), Some(max), Some(step))
                    };
                    domains.push(Axis {
                        label: label.to_owned(),
                        min,
                        max,
                        step,
                    });
                }
                Ok(CapabilityDomain::Axes { axes: domains })
            }
        }
    }
}

fn answer_options(availability: Availability) -> Vec<(Option<Answer>, &'static str)> {
    let mut options = vec![(None, "No answer")];
    match availability {
        Availability::Available => {
            options.push((Some(Answer::Matches), "Matches documentation"));
            options.push((
                Some(Answer::Corrected),
                "Available, but the options or range differ",
            ));
            options.push((Some(Answer::Unavailable), "Not available on my camera"));
        }
        Availability::Unavailable => {
            options.push((
                Some(Answer::Matches),
                "Still not available (matches documentation)",
            ));
            options.push((Some(Answer::Corrected), "Available on my camera"));
        }
        Availability::NotDocumented => {
            options.push((Some(Answer::Corrected), "Available on my camera"));
            options.push((Some(Answer::Unavailable), "Not available on my camera"));
        }
    }
    options
}

const DOMAIN_KINDS: [(DomainKindDraft, &str); 6] = [
    (DomainKindDraft::Unspecified, "Not specified"),
    (DomainKindDraft::Boolean, "On or off"),
    (DomainKindDraft::Enum, "A list of choices"),
    (DomainKindDraft::Integer, "Whole-number range"),
    (DomainKindDraft::Rational, "Exact range"),
    (DomainKindDraft::Axes, "Two-axis range"),
];

fn editor_domain(
    capability: &looklab_schema::DocumentedCapability,
    confirmation: &looklab_schema::ControlConfirmation,
    firmware: &str,
) -> CapabilityDomain {
    let reported = firmware.trim();
    confirmation
        .firmware
        .iter()
        .find(|result| result.firmware.as_str() == reported)
        .and_then(
            |result| match (result.basis.as_ref(), result.confirmed_actual.as_ref()) {
                (Some(ConfirmationBasis::Corrected), Some(actual)) => Some(actual.domain.clone()),
                _ => None,
            },
        )
        .unwrap_or_else(|| capability.domain.clone())
}

fn control_finding(
    capability: &ManualCapabilityPage,
    answer: Answer,
    drafts: &BTreeMap<CapabilityKey, DomainDraft>,
) -> Result<Finding, String> {
    match answer {
        Answer::Matches => {
            if capability.documented.availability == Availability::Available {
                Ok(Finding::Matches)
            } else {
                Ok(Finding::Corrected {
                    availability: Availability::Unavailable,
                    domain: CapabilityDomain::Unspecified,
                })
            }
        }
        Answer::Unavailable => Ok(Finding::Corrected {
            availability: Availability::Unavailable,
            domain: CapabilityDomain::Unspecified,
        }),
        Answer::Corrected => {
            let draft = drafts
                .get(&capability.documented.key)
                .cloned()
                .unwrap_or_else(|| DomainDraft::from_domain(&capability.documented.domain));
            Ok(Finding::Corrected {
                availability: Availability::Available,
                domain: draft.to_domain()?,
            })
        }
    }
}

fn envelope(
    camera: &BodyId,
    camera_digest: &Sha256Hex,
    capabilities: &[ManualCapabilityPage],
    firmware: &str,
    answers: &BTreeMap<CapabilityKey, Answer>,
    drafts: &BTreeMap<CapabilityKey, DomainDraft>,
) -> Result<Option<ManualConfirmationEnvelope>, String> {
    let Some(firmware) = ReportedFirmware::new(firmware).ok() else {
        return Ok(None);
    };
    let mut observations = Vec::new();
    for capability in capabilities {
        let Some(answer) = answers.get(&capability.documented.key) else {
            continue;
        };
        observations.push(ControlObservation {
            key: capability.documented.key,
            definition_digest: capability.definition_digest,
            finding: control_finding(capability, *answer, drafts)?,
        });
    }
    if observations.is_empty() {
        return Ok(None);
    }
    let keys = observations
        .iter()
        .map(|observation| observation.key)
        .collect::<Vec<CapabilityKey>>();
    build_manual_confirmation(
        camera.clone(),
        *camera_digest,
        firmware,
        ConfirmationScope::Controls { keys },
        observations,
    )
    .map(Some)
    .map_err(|error| error.to_string())
}

fn update_draft(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
    edit: impl FnOnce(&mut DomainDraft),
) {
    drafts.update(|drafts| {
        if let Some(draft) = drafts.get_mut(&key) {
            edit(draft);
        }
    });
}

fn enum_editor(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
) -> impl IntoView {
    let initial = drafts.with(|drafts| match drafts.get(&key) {
        Some(DomainDraft::Enum { values }) => values.clone(),
        _ => String::new(),
    });
    let id = format!("domain-enum-{}", key.as_str());
    view! {
        <label for=id.clone()>"Options, one per line"</label>
        <textarea id=id rows="3" prop:value=initial
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Enum { values } = draft {
                        *values = value;
                    }
                });
            }></textarea>
    }
}

fn integer_editor(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
) -> impl IntoView {
    let (min, max, steps) = drafts.with(|drafts| match drafts.get(&key) {
        Some(DomainDraft::Integer { min, max, steps }) => (min.clone(), max.clone(), steps.clone()),
        _ => (String::new(), String::new(), String::new()),
    });
    let prefix = format!("domain-integer-{}", key.as_str());
    view! {
        <label for=format!("{prefix}-min")>"Minimum"</label>
        <input id=format!("{prefix}-min") type="number" prop:value=min
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Integer { min, .. } = draft {
                        *min = value;
                    }
                });
            } />
        <label for=format!("{prefix}-max")>"Maximum"</label>
        <input id=format!("{prefix}-max") type="number" prop:value=max
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Integer { max, .. } = draft {
                        *max = value;
                    }
                });
            } />
        <label for=format!("{prefix}-steps")>"Step or steps (comma-separated, optional)"</label>
        <input id=format!("{prefix}-steps") type="text" prop:value=steps
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Integer { steps, .. } = draft {
                        *steps = value;
                    }
                });
            } />
    }
}

fn rational_editor(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
) -> impl IntoView {
    let (min, max, steps) = drafts.with(|drafts| match drafts.get(&key) {
        Some(DomainDraft::Rational { min, max, steps }) => {
            (min.clone(), max.clone(), steps.clone())
        }
        _ => (String::new(), String::new(), String::new()),
    });
    let prefix = format!("domain-rational-{}", key.as_str());
    view! {
        <label for=format!("{prefix}-min")>"Exact minimum"</label>
        <input id=format!("{prefix}-min") type="text" prop:value=min
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Rational { min, .. } = draft {
                        *min = value;
                    }
                });
            } />
        <label for=format!("{prefix}-max")>"Exact maximum"</label>
        <input id=format!("{prefix}-max") type="text" prop:value=max
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Rational { max, .. } = draft {
                        *max = value;
                    }
                });
            } />
        <label for=format!("{prefix}-steps")>"Exact step or steps (comma-separated, optional)"</label>
        <input id=format!("{prefix}-steps") type="text" prop:value=steps
            on:input=move |event| {
                let value = event_target_value(&event);
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Rational { steps, .. } = draft {
                        *steps = value;
                    }
                });
            } />
    }
}

fn axis_row(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
    index: usize,
    prefix: String,
) -> impl IntoView {
    let (label, min, max, step) = drafts.with(|drafts| match drafts.get(&key) {
        Some(DomainDraft::Axes { axes }) => axes
            .get(index)
            .map(|axis| {
                (
                    axis.label.clone(),
                    axis.min.clone(),
                    axis.max.clone(),
                    axis.step.clone(),
                )
            })
            .unwrap_or_default(),
        _ => Default::default(),
    });
    let id = format!("{prefix}-{index}");
    view! {
        <div class="field-row">
            <label for=format!("{id}-label")>"Axis label"</label>
            <input id=format!("{id}-label") type="text" prop:value=label
                on:input=move |event| {
                    let value = event_target_value(&event);
                    update_draft(drafts, key, |draft| {
                        if let DomainDraft::Axes { axes } = draft
                            && let Some(axis) = axes.get_mut(index)
                        {
                            axis.label = value;
                        }
                    });
                } />
            <label for=format!("{id}-min")>"Minimum"</label>
            <input id=format!("{id}-min") type="number" prop:value=min
                on:input=move |event| {
                    let value = event_target_value(&event);
                    update_draft(drafts, key, |draft| {
                        if let DomainDraft::Axes { axes } = draft
                            && let Some(axis) = axes.get_mut(index)
                        {
                            axis.min = value;
                        }
                    });
                } />
            <label for=format!("{id}-max")>"Maximum"</label>
            <input id=format!("{id}-max") type="number" prop:value=max
                on:input=move |event| {
                    let value = event_target_value(&event);
                    update_draft(drafts, key, |draft| {
                        if let DomainDraft::Axes { axes } = draft
                            && let Some(axis) = axes.get_mut(index)
                        {
                            axis.max = value;
                        }
                    });
                } />
            <label for=format!("{id}-step")>"Step"</label>
            <input id=format!("{id}-step") type="number" prop:value=step
                on:input=move |event| {
                    let value = event_target_value(&event);
                    update_draft(drafts, key, |draft| {
                        if let DomainDraft::Axes { axes } = draft
                            && let Some(axis) = axes.get_mut(index)
                        {
                            axis.step = value;
                        }
                    });
                } />
            <button type="button" on:click=move |_| {
                update_draft(drafts, key, |draft| {
                    if let DomainDraft::Axes { axes } = draft
                        && index < axes.len()
                    {
                        axes.remove(index);
                    }
                });
            }>"Remove axis"</button>
        </div>
    }
}

fn axes_editor(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
) -> impl IntoView {
    let count = Memo::new(move |_| {
        drafts.with(|drafts| match drafts.get(&key) {
            Some(DomainDraft::Axes { axes }) => axes.len(),
            _ => 0,
        })
    });
    let prefix = format!("domain-axes-{}", key.as_str());
    view! {
        <p class="muted">
            "Each axis needs a label and either all of minimum, maximum and step, or none."
        </p>
        {move || {
            (0..count.get())
                .map(|index| axis_row(drafts, key, index, prefix.clone()))
                .collect_view()
        }}
        <button type="button" on:click=move |_| {
            update_draft(drafts, key, |draft| {
                if let DomainDraft::Axes { axes } = draft {
                    axes.push(AxisDraft {
                        label: String::new(),
                        min: String::new(),
                        max: String::new(),
                        step: String::new(),
                    });
                }
            });
        }>"Add axis"</button>
    }
}

fn domain_editor(
    drafts: RwSignal<BTreeMap<CapabilityKey, DomainDraft>>,
    key: CapabilityKey,
    base: CapabilityDomain,
) -> impl IntoView {
    let base_for_kind = base.clone();
    let kind = Memo::new(move |_| {
        drafts.with(|drafts| {
            drafts
                .get(&key)
                .cloned()
                .unwrap_or_else(|| DomainDraft::from_domain(&base_for_kind))
                .kind()
        })
    });
    let base_for_change = base.clone();
    let kind_id = format!("domain-kind-{}", key.as_str());
    let editor = move || match kind.get() {
        DomainKindDraft::Boolean => view! { <p class="muted">"On or off."</p> }.into_any(),
        DomainKindDraft::Enum => enum_editor(drafts, key).into_any(),
        DomainKindDraft::Integer => integer_editor(drafts, key).into_any(),
        DomainKindDraft::Rational => rational_editor(drafts, key).into_any(),
        DomainKindDraft::Axes => axes_editor(drafts, key).into_any(),
        DomainKindDraft::Unspecified => {
            view! { <p class="muted">"No numeric domain recorded."</p> }.into_any()
        }
    };
    view! {
        <div class="manual-domain-editor">
            <label for=kind_id.clone()>"Actual options or range on my camera"</label>
            <select id=kind_id
                on:change=move |event| {
                    let value = event_target_value(&event);
                    let kind = DomainKindDraft::from_str(&value)
                        .unwrap_or(DomainKindDraft::Unspecified);
                    drafts.update(|drafts| {
                        let current = drafts
                            .get(&key)
                            .cloned()
                            .unwrap_or_else(|| DomainDraft::from_domain(&base_for_change));
                        drafts.insert(
                            key,
                            DomainDraft::with_kind(kind, &current, &base_for_change),
                        );
                    });
                }
            >
                {DOMAIN_KINDS.iter().map(|(candidate, label)| {
                    let candidate = *candidate;
                    view! {
                        <option value=candidate.to_string() selected=move || kind.get() == candidate>{*label}</option>
                    }
                }).collect_view()}
            </select>
            {editor}
        </div>
    }
}

/// Used by the upload flow and the documentation-only camera page.
#[component]
pub fn ManualConfirmationPanel(
    manual: ManualPage,
    initial_firmware: Signal<String>,
    on_change: Callback<Option<ManualConfirmationEnvelope>>,
    #[prop(default = true)] show_context: bool,
    #[prop(optional)] summary: Option<RwSignal<ManualConfirmationSummary>>,
) -> impl IntoView {
    let camera = manual.id.clone();
    let camera_digest = manual.camera_digest;
    let id_slug = manual.id.as_str().replace('/', "-");
    let sources = manual.sources.clone();
    let capabilities = manual.capabilities.clone();
    let has_controls = !capabilities.is_empty();
    let summary = summary.unwrap_or_else(|| RwSignal::new(manual.confirmation.clone()));

    let firmware = RwSignal::new(initial_firmware.get_untracked());
    let last_initial = StoredValue::new(initial_firmware.get_untracked());
    Effect::new(move |_| {
        let next = initial_firmware.get();
        let current = firmware.get_untracked();
        if current.is_empty() || current == last_initial.get_value() {
            firmware.set(next.clone());
        }
        last_initial.set_value(next);
    });

    let answers = RwSignal::new(BTreeMap::<CapabilityKey, Answer>::new());
    let drafts = RwSignal::new(BTreeMap::<CapabilityKey, DomainDraft>::new());
    let has_envelope = RwSignal::new(false);
    let status = RwSignal::new(None::<String>);
    {
        let camera = camera.clone();
        let capabilities = capabilities.clone();
        Effect::new(move |_| {
            let built = envelope(
                &camera,
                &camera_digest,
                &capabilities,
                &firmware.get(),
                &answers.get(),
                &drafts.get(),
            );
            match built {
                Ok(envelope) => {
                    status.set(None);
                    has_envelope.set(envelope.is_some());
                    on_change.run(envelope);
                }
                Err(error) => {
                    status.set(Some(error));
                    has_envelope.set(false);
                    on_change.run(None);
                }
            }
        });
    }

    let firmware_id = format!("firmware-{id_slug}");
    let heading = format!("{} {}", manual.make, manual.model);
    let rows = move || {
        let current = summary.get();
        capabilities
            .iter()
            .map(|capability| {
                let confirmation = current
                    .control(capability.documented.key)
                    .cloned()
                    .unwrap_or_else(|| capability.confirmation.clone());
                let key = capability.documented.key;
                let label = capability.documented.label.clone();
                let category = category_text(capability.documented.category);
                let manual_value = format!(
                    "{} · {}",
                    availability_text(capability.documented.availability),
                    domain_text(&capability.documented.domain)
                );
                let firmware_actuals = confirmation.firmware.iter().map(|result| {
                    let firmware = result.firmware.as_str().to_owned();
                    let actual = firmware_actual_text(result);
                    let detail = firmware_detail(result);
                    let digest = firmware_definition_digest(result);
                    view! {
                        <div>
                            <strong>{format!("Firmware {firmware}")}</strong>{format!(": {actual}")}
                            {detail.map(|detail| view! { <div class="muted">{detail}</div> })}
                            {digest.map(|digest| view! {
                                <crate::ui::Disclosure summary="Attestation details".to_owned()>
                                    <p class="muted">"Control definition digest at confirmation"</p>
                                    <code>{digest}</code>
                                </crate::ui::Disclosure>
                            })}
                        </div>
                    }
                }).collect_view();
                let firmware_bases = confirmation.firmware.iter().map(|result| {
                    let firmware = result.firmware.as_str().to_owned();
                    let basis = firmware_basis_text(result);
                    let contradictions = result.contradictions.iter().map(|contradiction| {
                        view! {
                            <li>{format!("Conflicting report: {}", finding_text(&contradiction.finding))}</li>
                        }
                    }).collect_view();
                    view! {
                        <li>
                            <strong>{format!("Firmware {firmware}")}</strong>{format!(": {basis}")}
                            {(!result.contradictions.is_empty()).then(|| view! {
                                <ul>{contradictions}</ul>
                            })}
                        </li>
                    }
                }).collect_view();
                let control_actual = control_actual_text(&confirmation);
                let control_basis = confirmation_text(&confirmation);
                let moderator = moderator_evidence_text(&confirmation);
                let manual_note =
                    documentation_note(capability.documented.availability, &confirmation);
                let documented_for_change = capability.documented.clone();
                let confirmation_for_change = confirmation.clone();
                let documented_for_editor = capability.documented.clone();
                let confirmation_for_editor = confirmation.clone();
                let select_id = format!("confirm-{id_slug}-{}", key.as_str());
                let options = answer_options(capability.documented.availability);
            view! {
                <tr>
                    <th scope="row">
                        {label}
                        <div class="muted"><code>{key.as_str()}</code></div>
                    </th>
                    <td>{category}</td>
                    <td>
                        {(!confirmation.firmware.is_empty()).then(|| view! {
                            <div>{firmware_actuals}</div>
                        })}
                        {confirmation.firmware.is_empty().then(|| view! {
                            <div>{control_actual}</div>
                        })}
                    </td>
                    <td>
                        {manual_value}
                        {manual_note.map(|text| view! { <div class="muted">{text}</div> })}
                    </td>
                    <td>
                        <div class="muted">{control_basis}</div>
                        {(!confirmation.firmware.is_empty()).then(|| view! {
                            <ul>{firmware_bases}</ul>
                        })}
                        {moderator.map(|text| view! {
                            <div class="muted">{text}</div>
                        })}
                    </td>
                    <td>
                        <label for=select_id.clone()>"Your answer"</label>
                        <select
                            id=select_id
                            on:change=move |event| {
                                let value = event_target_value(&event);
                                let answer = Answer::from_str(&value).ok();
                                answers.update(|answers| match answer {
                                    Some(answer) => {
                                        answers.insert(key, answer);
                                    }
                                    None => {
                                        answers.remove(&key);
                                    }
                                });
                                if answer == Some(Answer::Corrected) {
                                    let base = editor_domain(
                                        &documented_for_change,
                                        &confirmation_for_change,
                                        &firmware.get_untracked(),
                                    );
                                    drafts.update(|drafts| {
                                        drafts.entry(key).or_insert_with(|| {
                                            DomainDraft::from_domain(&base)
                                        });
                                    });
                                }
                            }
                        >
                            {options.into_iter().map(|(value, label)| {
                                let value_text = value.map(|answer| answer.to_string()).unwrap_or_default();
                                view! {
                                    <option value=value_text selected=move || {
                                        answers.with(|answers| answers.get(&key).copied() == value)
                                    }>{label}</option>
                                }
                            }).collect_view()}
                        </select>
                    </td>
                </tr>
                {move || {
                    answers
                        .with(|answers| matches!(answers.get(&key), Some(Answer::Corrected)))
                        .then(|| {
                            let base = editor_domain(
                                &documented_for_editor,
                                &confirmation_for_editor,
                                &firmware.get_untracked(),
                            );
                            view! {
                                <tr>
                                    <td colspan="6">
                                        {domain_editor(drafts, key, base)}
                                    </td>
                                </tr>
                            }
                        })
                }}
            }
            })
            .collect_view()
    };

    view! {
        <div class="manual-confirmation-panel">
            {show_context.then(|| view! {
                <p><span class="badge">"From official documentation · owner checks per control"</span></p>
                <h4>{heading}</h4>
                <ul class="manual-sources">
                    {sources.iter().map(|source| {
                        let url = source.url.as_str().to_owned();
                        let title = source.title.clone();
                        let edition = source.edition.clone().unwrap_or_default();
                        let scope = source.firmware_scope.as_str().to_owned();
                        view! {
                            <li>
                                <a href=url target="_blank" rel="noreferrer">{title}</a>
                                {if edition.is_empty() { String::new() } else { format!(" · {edition}") }}
                                <span class="muted">{format!(" · firmware {scope}")}</span>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            })}
            <label for=firmware_id.clone()>"Firmware on your camera"</label>
            <input
                id=firmware_id
                type="text"
                prop:value=move || firmware.get()
                on:input=move |event| firmware.set(event_target_value(&event))
                placeholder="e.g. 1.00"
            />
            <p class="muted">
                "Reported by you. Only the options you answer below are recorded; unchecked options are not counted."
            </p>
            {has_controls.then(|| view! {
                <table class="data-table">
                    <caption>"Check or correct the documented options on your camera"</caption>
                    <thead>
                        <tr>
                            <th scope="col">"Control"</th>
                            <th scope="col">"Category"</th>
                            <th scope="col">"Confirmed actual"</th>
                            <th scope="col">"Manual says (documentation-derived)"</th>
                            <th scope="col">"Confirmation basis"</th>
                            <th scope="col">"Your answer"</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
            })}
            {(!has_controls).then(|| view! {
                <p class="muted">"This record lists no documented control to confirm."</p>
            })}
            {move || {
                if !has_controls {
                    ().into_any()
                } else if ReportedFirmware::new(&firmware.get()).is_err() {
                    view! { <p class="muted">"Enter the firmware you checked to include a confirmation."</p> }.into_any()
                } else if let Some(error) = status.get() {
                    view! { <p class="notice" role="alert">{error}</p> }.into_any()
                } else if !has_envelope.get() {
                    view! { <p class="muted">"Answer at least one option to include a confirmation."</p> }.into_any()
                } else {
                    view! { <p class="muted">"This confirmation will be submitted when you confirm."</p> }.into_any()
                }
            }}
            <ManualWithdrawControl camera=camera.clone() summary=summary/>
        </div>
    }
}

/// Removes the signed-in owner's live checks for one camera.
#[component]
pub fn ManualWithdrawControl(
    camera: BodyId,
    summary: RwSignal<ManualConfirmationSummary>,
) -> impl IntoView {
    let session = crate::session::use_session();
    let available = crate::base::server_available();
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let withdraw_camera = camera.clone();
    let withdraw = move |_| {
        if busy.get_untracked() {
            return;
        }
        let camera = withdraw_camera.clone();
        let make = camera.make().to_owned();
        let model = camera.model().to_owned();
        busy.set(true);
        message.set(Some("Confirm once on your device.".to_owned()));
        spawn_local(async move {
            let result = async move {
                let credential = webauthn::profile_credential(session).await?;
                let signed = webauthn::sign_request(
                    crate::request_digest::RequestDomain::WithdrawManualConfirmation,
                    camera.as_str().as_bytes(),
                    Some(&credential),
                )
                .await?;
                data::withdraw_manual_confirmation(&camera, &signed.fields).await
            }
            .await;
            match result {
                Ok(()) => match data::manual_summary(&make, &model).await {
                    Ok(refreshed) => {
                        summary.set(refreshed);
                        message.set(Some(
                            "Your owner checks for this camera were withdrawn.".to_owned(),
                        ));
                    }
                    Err(error) => message.set(Some(format!(
                        "Your owner checks were withdrawn, but the summary could not be refreshed: {error}"
                    ))),
                },
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="manual-withdraw">
            <button type="button" on:click=withdraw disabled=move || busy.get() || !available>
                "Withdraw my owner checks for this camera"
            </button>
            <p class="muted">
                "This removes your live owner checks from agreement counting. Your photos and published looks are unaffected."
            </p>
            {(!available).then(crate::ui::preview_notice)}
            {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        </div>
    }
}

/// Records moderator decisions with cited evidence for documented controls.
#[component]
pub fn ManualModerationPanel(
    manual: ManualPage,
    summary: RwSignal<ManualConfirmationSummary>,
) -> impl IntoView {
    let camera = manual.id.clone();
    let capabilities = manual.capabilities.clone();
    view! {
        <section class="manual-moderation" aria-labelledby="manual-moderation">
            <h3 id="manual-moderation">"Moderate documented controls"</h3>
            <p class="muted">
                "Evidence is recorded against the firmware scope you enter. Leave the scope as unspecified when the evidence does not establish one; it is then shown as human-reviewed without claiming a firmware range."
            </p>
            {capabilities.into_iter().map(|capability| view! {
                <ModerationRow camera=camera.clone() capability summary/>
            }).collect_view()}
        </section>
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeratorDecision {
    Confirm,
    Reject,
}

#[component]
fn ModerationRow(
    camera: BodyId,
    capability: ManualCapabilityPage,
    summary: RwSignal<ManualConfirmationSummary>,
) -> impl IntoView {
    let session = crate::session::use_session();
    let key = capability.documented.key;
    let definition_digest = capability.definition_digest;
    let label = capability.documented.label.clone();
    let decision = RwSignal::new(ModeratorDecision::Confirm);
    let scope_kind = RwSignal::new(ScopeKindDraft::Unspecified);
    let scope_from = RwSignal::new(String::new());
    let scope_to = RwSignal::new(String::new());
    let kind = RwSignal::new(ModeratorEvidenceKind::Physical);
    let url = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let submit = move |_| {
        if busy.get_untracked() {
            return;
        }
        let confirmed = decision.get_untracked() == ModeratorDecision::Confirm;
        let scope_kind = scope_kind.get_untracked();
        let scope_from = scope_from.get_untracked();
        let scope_to = scope_to.get_untracked();
        let kind = kind.get_untracked();
        let url = url.get_untracked();
        let url = (!url.trim().is_empty()).then(|| url.trim().to_owned());
        let description = description.get_untracked().trim().to_owned();
        let camera = camera.clone();
        busy.set(true);
        message.set(Some("Confirm once on your device.".to_owned()));
        spawn_local(async move {
            let result = async {
                let scope = match scope_kind {
                    ScopeKindDraft::Exact => ModeratorFirmwareScope::exact(scope_from.trim()),
                    ScopeKindDraft::Range => {
                        ModeratorFirmwareScope::range(scope_from.trim(), scope_to.trim())
                    }
                    ScopeKindDraft::Unspecified => Ok(ModeratorFirmwareScope::unspecified()),
                }
                .map_err(|error| error.to_string())?;
                let url = url
                    .as_deref()
                    .map(ManualUrl::new)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let envelope = build_manual_moderation(
                    camera.clone(),
                    key,
                    definition_digest,
                    confirmed,
                    ModeratorEvidence {
                        kind,
                        url,
                        description,
                        firmware_scope: scope,
                    },
                );
                let credential = webauthn::profile_credential(session).await?;
                let signed = webauthn::sign_request(
                    crate::request_digest::RequestDomain::ModerateManualConfirmation,
                    &envelope.canonical_bytes(),
                    Some(&credential),
                )
                .await?;
                let body = envelope
                    .request_body(
                        &signed.fields.nonce,
                        &signed.fields.credential_id,
                        &signed.fields.authenticator_data,
                        &signed.fields.client_data_json,
                        &signed.fields.signature,
                    )
                    .map_err(|error| error.to_string())?;
                data::moderate_manual_confirmation(&camera, key, &body).await
            }
            .await;
            match result {
                Ok(refreshed) => {
                    summary.set(refreshed);
                    message.set(Some("Moderator decision recorded.".to_owned()));
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="manual-moderation-row">
            <strong>{label}</strong>
            <div class="field-row">
                <label>"Decision"
                <select on:change=move |event| {
                    let choice = match event_target_value(&event).as_str() {
                        "confirm" => ModeratorDecision::Confirm,
                        "reject" => ModeratorDecision::Reject,
                        _ => return,
                    };
                    decision.set(choice);
                }>
                    <option value="confirm" selected=true>"Confirm"</option>
                    <option value="reject">"Reject"</option>
                </select>
                </label>
                <label>"Firmware scope"
                <select on:change=move |event| {
                    scope_kind.set(
                        ScopeKindDraft::from_str(&event_target_value(&event))
                            .unwrap_or(ScopeKindDraft::Unspecified),
                    );
                }>
                    {[
                        (ScopeKindDraft::Unspecified, "Not recorded"),
                        (ScopeKindDraft::Exact, "One firmware"),
                        (ScopeKindDraft::Range, "Firmware range"),
                    ].into_iter().map(|(candidate, label)| view! {
                        <option value=candidate.to_string()
                            selected=move || scope_kind.get() == candidate>{label}</option>
                    }).collect_view()}
                </select>
                </label>
                <label>"From"
                <input type="text" prop:value=move || scope_from.get()
                    on:input=move |event| scope_from.set(event_target_value(&event)) />
                </label>
                <label>"To"
                <input type="text" prop:value=move || scope_to.get()
                    on:input=move |event| scope_to.set(event_target_value(&event)) />
                </label>
                <label>"Evidence"
                <select on:change=move |event| {
                    if let Ok(value) = ModeratorEvidenceKind::from_str(&event_target_value(&event)) {
                        kind.set(value);
                    }
                }>
                    <option value="physical" selected=true>"Physical inspection"</option>
                    <option value="video">"Video"</option>
                </select>
                </label>
            </div>
            <label>"Evidence URL"
            <input type="url" prop:value=move || url.get()
                on:input=move |event| url.set(event_target_value(&event)) />
            </label>
            <label>"Evidence description"
            <textarea rows="2" prop:value=move || description.get()
                on:input=move |event| description.set(event_target_value(&event))></textarea>
            </label>
            <button type="button" on:click=submit disabled=move || busy.get()>
                "Record moderator decision"
            </button>
            {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        </div>
    }
}

#[component]
pub fn StandaloneConfirmation(
    manual: ManualPage,
    summary: RwSignal<ManualConfirmationSummary>,
    #[prop(default = false)] measured_contributions: bool,
) -> impl IntoView {
    let session = crate::session::use_session();
    let available = crate::base::server_available();
    let form_manual = manual.clone();
    let envelope = RwSignal::new(None::<ManualConfirmationEnvelope>);
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let on_change =
        Callback::new(move |value: Option<ManualConfirmationEnvelope>| envelope.set(value));
    let submit = move |_| {
        if busy.get_untracked() {
            return;
        }
        let Some(envelope) = envelope.get_untracked() else {
            return;
        };
        busy.set(true);
        message.set(Some("Confirm once on your device.".to_owned()));
        spawn_local(async move {
            match submit_standalone(session, envelope).await {
                Ok(result) => {
                    summary.set(result);
                    message.set(Some(
                        "Confirmation recorded. It remains unconfirmed until independent owners agree or a moderator reviews the evidence."
                            .to_owned(),
                    ));
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let lede = if measured_contributions {
        "You can confirm which documented options you see on this camera without sending photos. If a confirmation sent with your photos did not record, try it again here."
    } else {
        "This camera has documented guidance but no contribution support yet. You can still confirm which documented options you see and correct anything the documentation gets wrong. No photos or RAW files are needed."
    };
    view! {
        <section class="manual-confirmation" aria-labelledby="owner-confirmation">
            <h2 id="owner-confirmation">"Confirm these settings on your camera"</h2>
            <p>{lede}</p>
            <ManualConfirmationPanel
                manual=form_manual
                initial_firmware=Signal::derive(String::new)
                on_change=on_change
                show_context=false
                summary=summary
            />
            {(!available).then(crate::ui::preview_notice)}
            <button
                class="button-primary"
                on:click=submit
                disabled=move || busy.get() || envelope.get().is_none() || !available
            >
                "Confirm checked settings"
            </button>
            {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        </section>
    }
}

async fn submit_standalone(
    session: SessionContext,
    envelope: ManualConfirmationEnvelope,
) -> Result<ManualConfirmationSummary, String> {
    if session.require_profile().await.is_none() {
        return Err("Sign in was cancelled. Nothing was sent.".to_owned());
    }
    let credential_id = match idb::get(idb::StorageKey::CREDENTIAL).await {
        Ok(id) => id.and_then(|text| webauthn::decode_base64url(&text)),
        Err(message) => return Err(message),
    };
    let nonce = webauthn::fetch_nonce().await?;
    let mut worker = PrepWorker::spawn()?;
    let result = async {
        let request = build_send_manifest_request(
            nonce.nonce,
            Vec::new(),
            vec![AttestationHash::from_bytes(envelope.digest())],
        )
        .map_err(|error| error.to_string())?;
        let reply = worker
            .call(module_fn_id(ModuleFunction::BuildSendManifest), request)
            .await
            .map_err(|error| error.message())?;
        let send =
            decode_build_send_manifest_reply(&reply.bytes).map_err(|error| error.to_string())?;
        let assertion = webauthn::sign_batch(
            send.challenge.as_bytes(),
            credential_id.as_deref(),
            &nonce.rp_id,
        )
        .await?;
        let body = envelope
            .request_body(&nonce.nonce, &assertion, &send.send_manifest_bytes)
            .map_err(|error| error.to_string())?;
        data::submit_manual_confirmation(&body).await
    }
    .await;
    worker.terminate();
    result
}
