// SPDX-License-Identifier: AGPL-3.0-or-later
//! Menu-level summaries derived from a validated proposal payload.

use serde::{Deserialize, Serialize};

use crate::body::{BodyKind, MappingStatus};
use crate::control::Control;
use crate::ids::ControlKey;
use crate::interface::{Format, IlluminantHandling, InsertionPoint, InterfaceProfile};
use crate::proposal::{
    DATA_LICENSE, EpochSplitProposal, GateOverrideProposal, LookSplitProposal, LutEmulationDecl,
    LutEncoding, LutLookProposal, MappingProposal, ValidatedPayload, describe_control,
};
use crate::schema::MakernoteValue;

const MAX_DETAILS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalSummary {
    pub headline: String,
    #[serde(default)]
    pub details: Vec<String>,
}

impl ProposalSummary {
    pub fn new(headline: impl Into<String>) -> Self {
        Self {
            headline: headline.into(),
            details: Vec::new(),
        }
    }

    pub fn with_details(headline: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            headline: headline.into(),
            details,
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        core::iter::once(self.headline.as_str()).chain(self.details.iter().map(String::as_str))
    }

    pub fn text(&self) -> String {
        self.lines().collect::<Vec<_>>().join("\n")
    }
}

pub(crate) fn summarize(payload: &ValidatedPayload) -> ProposalSummary {
    match payload {
        ValidatedPayload::Body(body) => ProposalSummary::with_details(
            format!(
                "adds {} {} as a {}",
                body.make,
                body.model,
                match body.kind {
                    BodyKind::Camera => "camera body",
                    BodyKind::Software => "software body",
                }
            ),
            vec![
                format!("body id: {}", body.id),
                format!(
                    "mapping: {}",
                    match body.mapping_status {
                        MappingStatus::Provisional => "provisional",
                        MappingStatus::FixtureBacked => "fixture-backed",
                    }
                ),
            ],
        ),
        ValidatedPayload::SchemaChange(schema) => {
            let model = menu_words(schema.body.model());
            let mut details = Vec::new();
            let mut controls = 0usize;
            for epoch in &schema.epochs {
                for control in epoch
                    .controls
                    .iter()
                    .filter(|control| control.look_affecting)
                {
                    controls += 1;
                    push_detail(&mut details, control_detail(control));
                }
            }
            if controls == 0 {
                details.push("no look-affecting controls".to_owned());
            }
            ProposalSummary::with_details(format!("updates the {model} control schema"), details)
        }
        ValidatedPayload::Mapping(mapping) => mapping_summary(mapping),
        ValidatedPayload::InterfaceProfile(profile) => interface_summary(profile),
        ValidatedPayload::LutLook(look) => lut_summary(look),
        ValidatedPayload::EpochSplit(split) => epoch_summary(split),
        ValidatedPayload::LookSplit(split) => look_split_summary(split),
        ValidatedPayload::GateOverride(override_) => gate_summary(override_),
    }
}

fn push_detail(details: &mut Vec<String>, detail: String) {
    if details.len() < MAX_DETAILS {
        details.push(detail);
    }
}

fn control_detail(control: &Control) -> String {
    describe_control(control)
}

fn mapping_summary(mapping: &MappingProposal) -> ProposalSummary {
    let firmware = match &mapping.firmware_to {
        Some(to) => format!("{} to {}", mapping.firmware_from, to),
        None => format!("{}+", mapping.firmware_from),
    };
    ProposalSummary::with_details(
        format!(
            "adds {} to {} firmware {firmware}",
            menu_words(mapping.sim_key.as_str()),
            menu_words(mapping.body.model())
        ),
        vec![
            format!(
                "makernote: {} = {}",
                mapping.makernote.tag,
                makernote_value(mapping.makernote.value)
            ),
            format!("epoch: {}", mapping.epoch),
            format!("look: {}", mapping.look),
        ],
    )
}

fn interface_summary(profile: &InterfaceProfile) -> ProposalSummary {
    let inserts = match profile.insertion.point {
        InsertionPoint::CameraRgbWbLinear => "in camera RGB",
        InsertionPoint::SceneLinearRec2020 => "in scene-linear Rec. 2020",
        InsertionPoint::SceneLinearXyzD65 => "in scene-linear XYZ",
        InsertionPoint::DisplayEncodedPostTone => "after the camera's tone curve",
    };
    let illuminants = match profile.insertion.illuminant_handling {
        IlluminantHandling::Single => "one illuminant".to_owned(),
        IlluminantHandling::DualInterpolated => "interpolated between StdA and D65".to_owned(),
    };
    let formats: Vec<&str> = profile.formats.iter().map(format_name).collect();
    let bodies = if profile.bodies.is_empty() {
        "any body".to_owned()
    } else {
        profile
            .bodies
            .iter()
            .map(|body| menu_words(body.model()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    ProposalSummary::with_details(
        format!("adds the {} software interface profile", profile.name),
        vec![
            format!("profile id: {}", profile.id),
            format!("inserts {inserts}, {illuminants}"),
            format!("accepts: {}", formats.join(", ")),
            format!("bodies: {bodies}"),
        ],
    )
}

fn format_name(format: &Format) -> &'static str {
    match format {
        Format::Cube => "cube",
        Format::NativeJson => "native JSON",
        Format::Haldclut => "Hald clut",
    }
}

fn lut_summary(look: &LutLookProposal) -> ProposalSummary {
    let mut details = vec![
        format!("look id: {}", look.look),
        format!("license: {}", look.license.describe()),
        format!("attribution: {}", look.attribution),
        format!("LUT: {} ({} bytes)", look.lut.file, look.lut.size),
        format!("input: {}", lut_encoding(&look.encoding)),
    ];
    if let Some(emulates) = &look.emulates {
        details.push(format!("emulates: {}", lut_emulation(emulates)));
    }
    details.push(format!("published under {DATA_LICENSE}"));
    ProposalSummary::with_details(format!("adds the LUT-defined look {}", look.name), details)
}

fn lut_encoding(encoding: &LutEncoding) -> String {
    match encoding {
        LutEncoding::Flog2Fgamut => "F-Log2 / F-Gamut".to_owned(),
        LutEncoding::Display { base } => {
            format!("display-referred, base {}", base.describe())
        }
    }
}

fn lut_emulation(emulates: &LutEmulationDecl) -> String {
    match emulates {
        LutEmulationDecl::CameraRendering { maker, rendering } => {
            format!("{maker} {rendering}")
        }
        LutEmulationDecl::FilmStock { name } => name.clone(),
    }
}

fn epoch_summary(split: &EpochSplitProposal) -> ProposalSummary {
    ProposalSummary::with_details(
        format!(
            "splits {} into a new epoch at firmware {}",
            menu_words(split.body.model()),
            split.split_at
        ),
        vec![
            format!("epoch {} keeps earlier firmware", split.from_epoch),
            format!(
                "epoch {} takes {} and later",
                split.new_epoch, split.split_at
            ),
        ],
    )
}

fn look_split_summary(split: &LookSplitProposal) -> ProposalSummary {
    ProposalSummary::with_details(
        format!("splits {} into {}", split.look, split.new_look),
        vec![format!("new look name: {}", split.name)],
    )
}

fn gate_summary(override_: &GateOverrideProposal) -> ProposalSummary {
    ProposalSummary::with_details(
        format!(
            "overrides the release gate for {} version {}",
            override_.look, override_.version
        ),
        vec![format!("justification: {}", override_.justification)],
    )
}

/// Keeps hyphenated model names joined, e.g. "X-T5".
pub(crate) fn menu_words(slug: &str) -> String {
    let mut out = String::with_capacity(slug.len());
    let mut capitalize = true;
    for character in slug.chars() {
        match character {
            '_' => {
                out.push(' ');
                capitalize = true;
            }
            '-' => {
                out.push('-');
                capitalize = true;
            }
            character if capitalize => {
                out.extend(character.to_uppercase());
                capitalize = false;
            }
            character => out.push(character),
        }
    }
    out
}

/// Known control keys, in the camera's own menu wording.
pub(crate) fn menu_label(key: &ControlKey) -> String {
    const KNOWN: [(&str, &str); 15] = [
        ("highlight_tone", "Highlight tone"),
        ("shadow_tone", "Shadow tone"),
        ("color", "Color"),
        ("color_chrome_effect", "Color Chrome effect"),
        ("color_chrome_fx_blue", "Color Chrome FX Blue"),
        ("mono_filter", "Monochrome filter"),
        ("bw_adjustment", "BW adjustment"),
        ("bw_magenta_green", "BW magenta green"),
        ("dynamic_range", "Dynamic range"),
        ("wb_shift_r", "White balance shift R"),
        ("wb_shift_b", "White balance shift B"),
        ("exposure_compensation", "Exposure compensation"),
        ("grain", "Grain"),
        ("clarity", "Clarity"),
        ("noise_reduction", "Noise reduction"),
    ];
    KNOWN
        .iter()
        .find(|(known, _)| *known == key.as_str())
        .map_or_else(
            || sentence_words(key.as_str()),
            |(_, label)| (*label).to_owned(),
        )
}

fn sentence_words(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (index, word) in key.split('_').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        if index == 0 {
            out.extend(word.chars().next().into_iter().flat_map(char::to_uppercase));
            out.push_str(&word[1.min(word.len())..]);
        } else {
            out.push_str(word);
        }
    }
    out
}

fn makernote_value(value: MakernoteValue) -> String {
    if value.0 <= 0xffff {
        format!("0x{:04x}", value.0)
    } else {
        format!("0x{:x}", value.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_words_capitalizes_parts_and_keeps_model_hyphens() {
        assert_eq!(menu_words("x-t5"), "X-T5");
        assert_eq!(menu_words("classic_chrome"), "Classic Chrome");
        assert_eq!(menu_words("reala_ace"), "Reala Ace");
    }

    #[test]
    fn unknown_control_keys_become_sentence_words() {
        assert_eq!(
            menu_label(&ControlKey::new("monochrome_tint").expect("key")),
            "Monochrome tint"
        );
        assert_eq!(
            menu_label(&ControlKey::new("highlight_tone").expect("key")),
            "Highlight tone"
        );
    }

    #[test]
    fn summary_text_lists_the_headline_first() {
        let summary =
            ProposalSummary::with_details("adds X-T5", vec!["body id: fujifilm/x-t5".to_owned()]);
        assert_eq!(summary.text(), "adds X-T5\nbody id: fujifilm/x-t5");
    }
}
