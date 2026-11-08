// SPDX-License-Identifier: AGPL-3.0-or-later
//! Authoritative render classification and family-scoped control applicability.

use serde::{Deserialize, Serialize};

use looklab_meta::{ColorSaturation, FujiTag, MonochromeFilter};

use crate::ids::{ControlKey, LookId, SimKey};
use crate::makernote::{MakernoteValues, matching_sims};
use crate::mapping::MappingError;
use crate::schema::{Epoch, NativeSim};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderClass {
    Colour,
    Monochrome,
}

impl RenderClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Colour => "colour",
            Self::Monochrome => "monochrome",
        }
    }
}

impl core::fmt::Display for RenderClass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The normalized monochrome filter setting; toning is not a filter value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonoFilter {
    None,
    Yellow,
    Red,
    Green,
}

impl From<MonochromeFilter> for MonoFilter {
    fn from(filter: MonochromeFilter) -> Self {
        match filter {
            MonochromeFilter::NoFilter | MonochromeFilter::Sepia => Self::None,
            MonochromeFilter::Yellow => Self::Yellow,
            MonochromeFilter::Red => Self::Red,
            MonochromeFilter::Green => Self::Green,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderClassification {
    pub render_class: RenderClass,
    /// The file's native sim key.
    pub native_sim: SimKey,
    /// The schema's `NativeSim.look`.
    pub family: LookId,
    /// `Some` exactly for a monochrome render.
    pub mono_filter: Option<MonoFilter>,
}

/// # Errors
/// No native sim matches, or several colour sims do.
pub fn classify_render(
    epoch: &Epoch,
    values: &MakernoteValues,
) -> Result<RenderClassification, MappingError> {
    let matches = matching_sims(epoch, values);
    if let Some(sim) = matches.iter().find(|sim| sim.monochrome) {
        return Ok(RenderClassification {
            render_class: RenderClass::Monochrome,
            native_sim: sim.key.clone(),
            family: sim.look.clone(),
            mono_filter: Some(mono_filter(sim)),
        });
    }
    match matches.as_slice() {
        [sim] => Ok(RenderClassification {
            render_class: RenderClass::Colour,
            native_sim: sim.key.clone(),
            family: sim.look.clone(),
            mono_filter: None,
        }),
        [] => Err(MappingError::UnknownSim),
        _ => Err(MappingError::AmbiguousSim),
    }
}

fn mono_filter(sim: &NativeSim) -> MonoFilter {
    if sim.makernote.tag.as_str() != FujiTag::Saturation.exiftool_name() {
        return MonoFilter::None;
    }
    let Ok(raw) = u16::try_from(sim.makernote.value.0) else {
        return MonoFilter::None;
    };
    match ColorSaturation::from_raw(raw) {
        Some(ColorSaturation::Monochrome(filter) | ColorSaturation::Acros(filter)) => filter.into(),
        _ => MonoFilter::None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardRenderClassification {
    pub render_class: RenderClass,
    pub family: LookId,
    pub mono_filter: Option<MonoFilter>,
}

/// # Errors
/// The epoch declares `family` with the opposite class.
pub fn classify_card_render(
    epoch: &Epoch,
    family: &LookId,
    declared: RenderClass,
    values: &MakernoteValues,
) -> Result<CardRenderClassification, MappingError> {
    if let Some(resolved) = epoch.family_render_class(family)
        && resolved != declared
    {
        return Err(MappingError::RenderClassConflict {
            family: family.clone(),
            declared,
            resolved,
        });
    }
    let mono_filter = (declared == RenderClass::Monochrome)
        .then(|| metadata_mono_filter(values).unwrap_or(MonoFilter::None));
    Ok(CardRenderClassification {
        render_class: declared,
        family: family.clone(),
        mono_filter,
    })
}

fn metadata_mono_filter(values: &MakernoteValues) -> Option<MonoFilter> {
    let raw = u16::try_from(values.tag_value(FujiTag::Saturation)?).ok()?;
    match ColorSaturation::from_raw(raw)? {
        ColorSaturation::Monochrome(filter) | ColorSaturation::Acros(filter) => Some(filter.into()),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicableControls {
    /// Look-affecting controls the schema maps from a makernote tag.
    pub mapped: Vec<ControlKey>,
    /// Look-affecting controls the schema maps from no tag.
    pub unmapped: Vec<ControlKey>,
    /// Spatial controls, which deferred monochrome intake still requires a source for.
    pub spatial: Vec<ControlKey>,
}

impl ApplicableControls {
    pub fn all(&self) -> impl Iterator<Item = &ControlKey> {
        self.mapped
            .iter()
            .chain(&self.unmapped)
            .chain(&self.spatial)
    }
}

pub fn applicable_controls(epoch: &Epoch, family: &LookId) -> ApplicableControls {
    let mut controls = ApplicableControls::default();
    for control in &epoch.controls {
        if !control.scope.includes(family) {
            continue;
        }
        if control.spatial {
            controls.spatial.push(control.key.clone());
        } else if control.look_affecting {
            if control.makernote.is_some() {
                controls.mapped.push(control.key.clone());
            } else {
                controls.unmapped.push(control.key.clone());
            }
        }
    }
    controls
}

impl Epoch {
    /// # Errors
    /// No native sim matches, or several colour sims do.
    pub fn classify_render(
        &self,
        values: &MakernoteValues,
    ) -> Result<RenderClassification, MappingError> {
        classify_render(self, values)
    }

    pub fn applicable_controls(&self, family: &LookId) -> ApplicableControls {
        applicable_controls(self, family)
    }

    pub fn family_render_class(&self, family: &LookId) -> Option<RenderClass> {
        self.native_sims
            .iter()
            .find(|sim| &sim.look == family)
            .map(|sim| {
                if sim.monochrome {
                    RenderClass::Monochrome
                } else {
                    RenderClass::Colour
                }
            })
    }
}
