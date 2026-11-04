// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control-response model and realization of a Look on a body (docs/04 §2,
//! docs/03 §1). Deltas are stored as per-layer basis expansions.

use core::f64::consts::PI;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use looklab_schema::{
    Affects, BodyId, Control, ControlDefault, ControlKey, ControlSchema, ControlType, EnumValue,
    SimKey,
};

use crate::basis::{HUE_COEFS, S_COEFS, TINT_COEFS, TONE_COEFS};
use crate::error::{ModelError, Result};
use crate::look::{GRL_COEFS, Look};

pub const CRM_FORMAT: &str = "looklab-crm/1";
const MAX_CRM_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTROLS: usize = 256;
const MAX_VALUES: usize = 1024;
const MAX_INTERACTIONS: usize = 1024;
const MAX_EXPOSURE_ROWS: usize = 256;

/// Unit-norm feature expansions for the chroma, hue and lightness layers.
pub const GRL_FEATURES: u32 = 10;

const HUE_BUMP_CENTERS: [f64; 4] = [45.0, 135.0, 225.0, 315.0];
const HUE_BUMP_SIGMA: f64 = 40.0;

/// Orthonormal hue patterns: constant, cosine, sine and four Gaussian bumps.
static HUE_PATTERNS: std::sync::LazyLock<[[f64; HUE_COEFS]; 7]> =
    std::sync::LazyLock::new(hue_patterns);

#[allow(clippy::needless_range_loop)]
fn hue_patterns() -> [[f64; HUE_COEFS]; 7] {
    let mut raw = [[0.0_f64; HUE_COEFS]; 7];
    for hue in 0..HUE_COEFS {
        let angle = 2.0 * PI * hue as f64 / HUE_COEFS as f64;
        raw[0][hue] = 1.0;
        raw[1][hue] = libm::cos(angle);
        raw[2][hue] = libm::sin(angle);
    }
    for (bump, center) in HUE_BUMP_CENTERS.iter().enumerate() {
        for hue in 0..HUE_COEFS {
            let mut distance = (hue as f64 * 360.0 / HUE_COEFS as f64 - center).abs();
            if distance > 180.0 {
                distance = 360.0 - distance;
            }
            raw[3 + bump][hue] =
                libm::exp(-0.5 * distance * distance / (HUE_BUMP_SIGMA * HUE_BUMP_SIGMA));
        }
    }
    let mut basis = [[0.0_f64; HUE_COEFS]; 7];
    for (index, row) in raw.iter().enumerate() {
        let mut vector = *row;
        for prior in &basis[..index] {
            let projection: f64 = vector.iter().zip(prior.iter()).map(|(a, b)| a * b).sum();
            for (slot, value) in vector.iter_mut().zip(prior.iter()) {
                *slot -= projection * value;
            }
        }
        let norm: f64 = vector
            .iter()
            .map(|value| value * value * S_COEFS as f64 * 3.0)
            .sum::<f64>()
            .sqrt();
        let scale = 1.0 / norm.max(1e-12);
        for (slot, value) in vector.iter_mut().enumerate() {
            basis[index][slot] = *value * scale;
        }
    }
    basis
}

/// Numeric controls store the integer multiple of their schema step, so `1/3`
/// EV is `Step(1)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlValue {
    Step(i64),
    Enum(EnumValue),
    Bool(bool),
}

/// Settings omitted from the map take the schema default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Settings {
    values: BTreeMap<ControlKey, ControlValue>,
}

impl Settings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: ControlKey, value: ControlValue) {
        self.values.insert(key, value);
    }

    pub fn with(mut self, key: ControlKey, value: ControlValue) -> Self {
        self.set(key, value);
        self
    }

    pub fn get(&self, key: &ControlKey) -> Option<&ControlValue> {
        self.values.get(key)
    }

    pub fn remove(&mut self, key: &ControlKey) -> Option<ControlValue> {
        self.values.remove(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ControlKey, &ControlValue)> {
        self.values.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The schema's `affects` vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affect {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    Wb,
    Exposure,
}

impl Affect {
    pub fn layer(self) -> Option<LayerKind> {
        match self {
            Self::Tone => Some(LayerKind::Tone),
            Self::Tint => Some(LayerKind::Tint),
            Self::Chroma => Some(LayerKind::Chroma),
            Self::Hue => Some(LayerKind::Hue),
            Self::Lightness => Some(LayerKind::Lightness),
            Self::Wb | Self::Exposure => None,
        }
    }
}

impl From<Affects> for Affect {
    fn from(value: Affects) -> Self {
        match value {
            Affects::Tone => Self::Tone,
            Affects::Tint => Self::Tint,
            Affects::Chroma => Self::Chroma,
            Affects::Hue => Self::Hue,
            Affects::Lightness => Self::Lightness,
            Affects::Wb => Self::Wb,
            Affects::Exposure => Self::Exposure,
        }
    }
}

/// A Look layer a control may move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKind {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
}

impl LayerKind {
    /// Length of the layer's coefficient vector in `Look::coef_vector`.
    pub fn coefficient_len(self) -> usize {
        match self {
            Self::Tone => 1 + TONE_COEFS,
            Self::Tint => 2 * TINT_COEFS,
            Self::Chroma | Self::Hue | Self::Lightness => GRL_COEFS,
        }
    }

    /// Number of stored coefficients (identity basis) or feature expansions.
    pub fn feature_count(self) -> u32 {
        match self {
            Self::Tone => (1 + TONE_COEFS) as u32,
            Self::Tint => (2 * TINT_COEFS) as u32,
            Self::Chroma | Self::Hue | Self::Lightness => GRL_FEATURES,
        }
    }
}

/// A fitted degree of freedom of the control-response model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseComponent {
    Tone,
    Tint,
    Chroma,
    Hue,
    Lightness,
    WbR,
    WbB,
}

impl ResponseComponent {
    pub fn layer(self) -> Option<LayerKind> {
        match self {
            Self::Tone => Some(LayerKind::Tone),
            Self::Tint => Some(LayerKind::Tint),
            Self::Chroma => Some(LayerKind::Chroma),
            Self::Hue => Some(LayerKind::Hue),
            Self::Lightness => Some(LayerKind::Lightness),
            Self::WbR | Self::WbB => None,
        }
    }

    pub fn feature_count(self) -> u32 {
        self.layer().map_or(1, LayerKind::feature_count)
    }
}

impl From<LayerKind> for ResponseComponent {
    fn from(value: LayerKind) -> Self {
        match value {
            LayerKind::Tone => Self::Tone,
            LayerKind::Tint => Self::Tint,
            LayerKind::Chroma => Self::Chroma,
            LayerKind::Hue => Self::Hue,
            LayerKind::Lightness => Self::Lightness,
        }
    }
}

/// A response coefficient, in [`ControlResponseModel::parameter_ids`] order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResponseParamId {
    pub control: ControlKey,
    pub value: ControlValue,
    pub component: ResponseComponent,
    pub feature: u32,
}

/// Basis expansion of one layer's coefficient delta.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerDelta {
    pub layer: LayerKind,
    /// `(feature, coefficient)`, sorted by feature.
    pub entries: Vec<(u32, f64)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// At most one entry per layer.
    pub layers: Vec<LayerDelta>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.layers
            .iter()
            .all(|layer| layer.entries.iter().all(|(_, value)| *value == 0.0))
    }

    fn layer_mut(&mut self, layer: LayerKind) -> &mut LayerDelta {
        if let Some(index) = self.layers.iter().position(|entry| entry.layer == layer) {
            return &mut self.layers[index];
        }
        self.layers.push(LayerDelta {
            layer,
            entries: Vec::new(),
        });
        let index = self.layers.len() - 1;
        &mut self.layers[index]
    }

    pub fn add(&mut self, layer: LayerKind, feature: u32, value: f64) {
        if !value.is_finite() {
            return;
        }
        let target = self.layer_mut(layer);
        match target
            .entries
            .binary_search_by_key(&feature, |entry| entry.0)
        {
            Ok(index) => target.entries[index].1 += value,
            Err(index) => target.entries.insert(index, (feature, value)),
        }
    }

    /// Additive delta over `Look::coef_vector`. A tone delta on a colorimetric
    /// tone is dropped here and rejected by [`Delta::apply`].
    pub fn coefficient_entries(&self, look: &Look) -> Vec<(usize, f64)> {
        let layout = look.layout();
        let mut out: Vec<(usize, f64)> = Vec::new();
        for layer in &self.layers {
            let base = match layer.layer {
                LayerKind::Tone => {
                    if look.tone.is_colorimetric() {
                        continue;
                    }
                    layout.tone.start
                }
                LayerKind::Tint => layout.tint_a.start,
                LayerKind::Chroma => layout.g.start,
                LayerKind::Hue => layout.r.start,
                LayerKind::Lightness => layout.lambda.start,
            };
            for (feature, value) in &layer.entries {
                let vector = layer_feature(layer.layer, *feature);
                for (index, coefficient) in vector.iter().enumerate() {
                    let target = match layer.layer {
                        LayerKind::Tint if index >= TINT_COEFS => {
                            layout.tint_b.start + (index - TINT_COEFS)
                        }
                        _ => base + index,
                    };
                    if *coefficient != 0.0 {
                        out.push((target, value * coefficient));
                    }
                }
            }
        }
        out
    }

    /// Adds the delta to a copy of `look`. Beta offsets are clamped at zero so
    /// the tone spline stays monotone.
    pub fn apply(&self, look: &Look) -> core::result::Result<Look, RealizationError> {
        let mut coef = look.coef_vector();
        let layout = look.layout();
        if look.tone.is_colorimetric()
            && self.layers.iter().any(|layer| {
                layer.layer == LayerKind::Tone
                    && layer.entries.iter().any(|(_, value)| *value != 0.0)
            })
        {
            return Err(RealizationError::ColorimetricToneDelta);
        }
        for (index, value) in self.coefficient_entries(look) {
            if let Some(slot) = coef.get_mut(index) {
                *slot += value;
            }
        }
        if !look.tone.is_colorimetric() {
            for value in &mut coef[layout.tone.start + 1..layout.tone.end] {
                *value = value.max(0.0);
            }
        }
        let mut realized = look.clone();
        realized
            .apply_coef(&coef)
            .map_err(RealizationError::Model)?;
        Ok(realized)
    }
}

/// One coefficient of the unit-norm feature vector.
pub fn feature_coefficient(layer: LayerKind, feature: u32, index: usize) -> f64 {
    if index >= layer.coefficient_len() {
        return 0.0;
    }
    match layer {
        LayerKind::Tone | LayerKind::Tint => {
            if index == feature as usize {
                1.0
            } else {
                0.0
            }
        }
        LayerKind::Chroma | LayerKind::Hue | LayerKind::Lightness => {
            grl_feature_coefficient(feature, index)
        }
    }
}

/// Unit-norm expansion of `feature` over the layer's coefficient vector.
pub fn layer_feature(layer: LayerKind, feature: u32) -> Vec<f64> {
    let mut out = vec![0.0_f64; layer.coefficient_len()];
    match layer {
        LayerKind::Tone | LayerKind::Tint => {
            if let Some(slot) = out.get_mut(feature as usize) {
                *slot = 1.0;
            }
        }
        LayerKind::Chroma | LayerKind::Hue | LayerKind::Lightness => {
            grl_feature_into(&mut out, feature);
        }
    }
    out
}

fn grl_feature_coefficient(feature: u32, index: usize) -> f64 {
    let volume_len = HUE_COEFS * S_COEFS * 3;
    if index < S_COEFS {
        return u32::from(feature == 0) as f64 / (S_COEFS as f64).sqrt();
    }
    let node = index - S_COEFS;
    if node >= volume_len {
        return 0.0;
    }
    let c = node % 3;
    let s = (node / 3) % S_COEFS;
    let hue = node / (3 * S_COEFS);
    match feature {
        1 => HUE_PATTERNS[0][hue],
        2 => HUE_PATTERNS[1][hue],
        3 => HUE_PATTERNS[2][hue],
        4 => {
            let variance = (S_COEFS as f64 * S_COEFS as f64 - 1.0) / 12.0;
            let scale = 1.0 / (variance * volume_len as f64).sqrt();
            (s as f64 - (S_COEFS as f64 - 1.0) / 2.0) * scale
        }
        5 => ((c as f64) - 1.0) / ((2.0 / 3.0) * volume_len as f64).sqrt(),
        6..=9 => HUE_PATTERNS[3 + (feature - 6) as usize][hue],
        _ => 0.0,
    }
}

fn grl_feature_into(out: &mut [f64], feature: u32) {
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = grl_feature_coefficient(feature, index);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValueResponse {
    pub value: ControlValue,
    pub delta: Delta,
}

/// Coefficients a control moves from its default. `values` excludes the default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub key: ControlKey,
    pub default: ControlValue,
    pub affects: Vec<Affect>,
    pub values: Vec<ValueResponse>,
}

impl ControlResponse {
    pub fn layers(&self) -> Vec<LayerKind> {
        let mut layers: Vec<LayerKind> = Vec::new();
        for affect in &self.affects {
            if let Some(layer) = affect.layer()
                && !layers.contains(&layer)
            {
                layers.push(layer);
            }
        }
        layers
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WbValueResponse {
    pub value: ControlValue,
    /// `[log R vs G, log B vs G]` in camera space.
    pub log_gain: [f64; 2],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WbControlResponse {
    pub key: ControlKey,
    pub default: ControlValue,
    pub values: Vec<WbValueResponse>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WbEffect {
    pub controls: Vec<WbControlResponse>,
    /// Body-level `ω` (R/B vs G), the gauge for the designated body's default class.
    pub omega: [f64; 2],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeOffset {
    pub control: ControlKey,
    pub value: ControlValue,
    pub ev: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureStep {
    pub key: ControlKey,
    pub default: i64,
    /// EV per integer step; `1/3` for the camera's exact-thirds EC control.
    pub ev_per_step: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExposureEffect {
    /// `ξ` per exposure mode, selected by an exposure-affecting enum value.
    pub modes: Vec<ModeOffset>,
    pub steps: Vec<ExposureStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlSupport {
    pub key: ControlKey,
    pub default: ControlValue,
    /// Values with data, including the default.
    pub observed: Vec<ControlValue>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Support {
    pub controls: Vec<ControlSupport>,
}

impl Support {
    pub fn observed(&self, key: &ControlKey) -> Option<&ControlSupport> {
        self.controls.iter().find(|entry| &entry.key == key)
    }

    /// Variance multiplier for a setting: 1 inside the observed range, growing
    /// with the step distance outside it. Unobserved enum values get 2.
    pub fn inflation(&self, settings: &Settings) -> f64 {
        let mut worst = 1.0_f64;
        for control in &self.controls {
            let value = settings.get(&control.key).unwrap_or(&control.default);
            worst = worst.max(self.value_inflation(control, value));
        }
        worst
    }

    pub fn value_inflation(&self, control: &ControlSupport, value: &ControlValue) -> f64 {
        if control.observed.iter().any(|entry| entry == value) {
            return 1.0;
        }
        match value {
            ControlValue::Step(step) => {
                let default = match control.default {
                    ControlValue::Step(default) => default,
                    _ => return 2.0,
                };
                let mut low = default;
                let mut high = default;
                for observed in &control.observed {
                    if let ControlValue::Step(other) = observed {
                        low = low.min(*other);
                        high = high.max(*other);
                    }
                }
                if *step < low {
                    (low - step) as f64 + 1.0
                } else if *step > high {
                    (step - high) as f64 + 1.0
                } else {
                    1.0
                }
            }
            ControlValue::Enum(_) | ControlValue::Bool(_) => 2.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub first: (ControlKey, ControlValue),
    pub second: (ControlKey, ControlValue),
    pub delta: Delta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlResponseModel {
    pub body: BodyId,
    pub epoch: u32,
    pub base_sim: SimKey,
    pub version: u32,
    pub controls: Vec<ControlResponse>,
    #[serde(default)]
    pub interactions: Vec<Interaction>,
    #[serde(default)]
    pub wb: WbEffect,
    #[serde(default)]
    pub exposure: ExposureEffect,
    #[serde(default)]
    pub support: Support,
}

/// A realized base sim at settings `u` (docs/04 §2).
#[derive(Clone, Debug, PartialEq)]
pub struct Realization {
    pub look: Look,
    pub wb_gain: [f64; 3],
    /// Signed EV applied before `A_c`, including `ξ` for the selected exposure mode.
    pub exposure_ev: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum RealizationError {
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("tone delta needs a spline tone, not the colorimetric reference")]
    ColorimetricToneDelta,
    #[error("control response model is invalid: {0}")]
    Invalid(String),
}

impl ControlResponseModel {
    /// Zero-valued model for a schema epoch.
    pub fn zeroed(
        schema: &ControlSchema,
        epoch: u32,
        base_sim: &SimKey,
        version: u32,
    ) -> core::result::Result<Self, RealizationError> {
        let epoch_schema = schema
            .epochs
            .iter()
            .find(|entry| entry.id == epoch)
            .ok_or_else(|| RealizationError::Invalid(format!("epoch {epoch} is not declared")))?;
        let mode_keys: Vec<&str> = epoch_schema
            .exposure_modes
            .iter()
            .map(|mode| mode.key.as_str())
            .collect();
        let mut controls = Vec::new();
        let mut wb_controls = Vec::new();
        let mut modes = Vec::new();
        let mut steps = Vec::new();
        for control in &epoch_schema.controls {
            let default = default_value(control)?;
            let affects: Vec<Affect> = control.affects.iter().copied().map(Affect::from).collect();
            let has_layer = affects.iter().any(|affect| affect.layer().is_some());
            let has_wb = affects.contains(&Affect::Wb);
            let has_exposure = affects.contains(&Affect::Exposure);
            let values = non_default_values(control, &default);
            if has_layer {
                controls.push(ControlResponse {
                    key: control.key.clone(),
                    default: default.clone(),
                    affects: affects.clone(),
                    values: values
                        .iter()
                        .map(|value| ValueResponse {
                            value: value.clone(),
                            delta: Delta::default(),
                        })
                        .collect(),
                });
            }
            if has_wb {
                wb_controls.push(WbControlResponse {
                    key: control.key.clone(),
                    default: default.clone(),
                    values: values
                        .iter()
                        .map(|value| WbValueResponse {
                            value: value.clone(),
                            log_gain: [0.0; 2],
                        })
                        .collect(),
                });
            }
            if has_exposure {
                match (&control.kind, &default) {
                    (ControlType::Enum, ControlValue::Enum(default_name)) => {
                        for value in &values {
                            let ControlValue::Enum(name) = value else {
                                continue;
                            };
                            if mode_keys.contains(&name.as_str())
                                || name == default_name
                                || !mode_keys.is_empty()
                            {
                                modes.push(ModeOffset {
                                    control: control.key.clone(),
                                    value: value.clone(),
                                    ev: 0.0,
                                });
                            }
                        }
                        if mode_keys.contains(&default_name.as_str())
                            && !modes
                                .iter()
                                .any(|mode| mode.control == control.key && mode.value == default)
                        {
                            modes.push(ModeOffset {
                                control: control.key.clone(),
                                value: default.clone(),
                                ev: 0.0,
                            });
                        }
                    }
                    (ControlType::Int | ControlType::Decimal, ControlValue::Step(_)) => {
                        if let Some(step) = &control.step {
                            steps.push(ExposureStep {
                                key: control.key.clone(),
                                default: match default {
                                    ControlValue::Step(value) => value,
                                    _ => 0,
                                },
                                ev_per_step: step.to_f64(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        modes.sort_by_key(|mode| {
            let name = match &mode.value {
                ControlValue::Enum(name) => Some(name.as_str()),
                _ => None,
            };
            name.and_then(|name| mode_keys.iter().position(|key| *key == name))
                .unwrap_or(usize::MAX)
        });
        let model = Self {
            body: schema.body.clone(),
            epoch,
            base_sim: base_sim.clone(),
            version,
            controls,
            interactions: Vec::new(),
            wb: WbEffect {
                controls: wb_controls,
                omega: [0.0; 2],
            },
            exposure: ExposureEffect { modes, steps },
            support: Support::default(),
        };
        model.validate()?;
        Ok(model)
    }

    /// Canonical parameter order for the fit's covariance.
    pub fn parameter_ids(&self) -> Vec<ResponseParamId> {
        let mut out = Vec::new();
        for control in &self.controls {
            let layers = control.layers();
            for layer in layers {
                let features = layer.feature_count();
                for value in &control.values {
                    for feature in 0..features {
                        out.push(ResponseParamId {
                            control: control.key.clone(),
                            value: value.value.clone(),
                            component: layer.into(),
                            feature,
                        });
                    }
                }
            }
        }
        for control in &self.wb.controls {
            for value in &control.values {
                for component in [ResponseComponent::WbR, ResponseComponent::WbB] {
                    out.push(ResponseParamId {
                        control: control.key.clone(),
                        value: value.value.clone(),
                        component,
                        feature: 0,
                    });
                }
            }
        }
        out
    }

    /// Weights of the stored non-default values, with piecewise-linear
    /// interpolation over numeric steps.
    pub fn value_weights(&self, key: &ControlKey, settings: &Settings) -> Vec<(ControlValue, f64)> {
        if let Some(control) = self.controls.iter().find(|entry| &entry.key == key) {
            return weights(&control.default, &control.values, settings.get(key));
        }
        if let Some(control) = self.wb.controls.iter().find(|entry| &entry.key == key) {
            let values: Vec<ControlValue> = control
                .values
                .iter()
                .map(|entry| entry.value.clone())
                .collect();
            return weights(&control.default, &values, settings.get(key));
        }
        Vec::new()
    }

    /// Response parameters active at `settings`, in [`Self::parameter_ids`] order.
    pub fn active_parameters(&self, settings: &Settings) -> Vec<(usize, f64)> {
        let ids = self.parameter_ids();
        let mut weights_by_control: Vec<(ControlKey, Vec<(ControlValue, f64)>)> = Vec::new();
        for control in &self.controls {
            weights_by_control.push((
                control.key.clone(),
                self.value_weights(&control.key, settings),
            ));
        }
        for control in &self.wb.controls {
            weights_by_control.push((
                control.key.clone(),
                self.value_weights(&control.key, settings),
            ));
        }
        let mut out = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            let weight = weights_by_control
                .iter()
                .find(|(key, _)| key == &id.control)
                .and_then(|(_, weights)| {
                    weights
                        .iter()
                        .find(|(value, _)| value == &id.value)
                        .map(|(_, weight)| *weight)
                })
                .unwrap_or(0.0);
            if weight != 0.0 {
                out.push((index, weight));
            }
        }
        out
    }

    /// Rows mapping each active response parameter to `Look::coef_vector`
    /// indices, including the setting's interpolation weight.
    pub fn active_coefficient_rows(
        &self,
        look: &Look,
        settings: &Settings,
    ) -> Vec<(usize, Vec<(usize, f64)>)> {
        let ids = self.parameter_ids();
        let layout = look.layout();
        let mut out = Vec::new();
        for (param, weight) in self.active_parameters(settings) {
            let Some(id) = ids.get(param) else {
                continue;
            };
            let Some(layer) = id.component.layer() else {
                continue;
            };
            if layer == LayerKind::Tone && look.tone.is_colorimetric() {
                continue;
            }
            let base = match layer {
                LayerKind::Tone => layout.tone.start,
                LayerKind::Tint => layout.tint_a.start,
                LayerKind::Chroma => layout.g.start,
                LayerKind::Hue => layout.r.start,
                LayerKind::Lightness => layout.lambda.start,
            };
            let mut row = Vec::new();
            for index in 0..layer.coefficient_len() {
                let value = feature_coefficient(layer, id.feature, index);
                if value == 0.0 {
                    continue;
                }
                let target = if layer == LayerKind::Tint && index >= TINT_COEFS {
                    layout.tint_b.start + (index - TINT_COEFS)
                } else {
                    base + index
                };
                row.push((target, value * weight));
            }
            out.push((param, row));
        }
        out
    }

    /// The additive layer delta and WB control increments (`ω` excluded).
    pub fn delta(&self, settings: &Settings) -> (Delta, [f64; 2]) {
        let ids = self.parameter_ids();
        let mut delta = Delta::default();
        let mut wb = [0.0_f64; 2];
        for (index, weight) in self.active_parameters(settings) {
            let Some(id) = ids.get(index) else {
                continue;
            };
            match id.component {
                ResponseComponent::WbR => wb[0] += self.wb_gain(&id.control, &id.value)[0] * weight,
                ResponseComponent::WbB => wb[1] += self.wb_gain(&id.control, &id.value)[1] * weight,
                _ => {
                    if let Some(layer) = id.component.layer() {
                        let value = self.delta_value(&id.control, &id.value, layer, id.feature);
                        delta.add(layer, id.feature, value * weight);
                    }
                }
            }
        }
        for interaction in &self.interactions {
            let first = effective(settings, &interaction.first.0, &interaction.first.1);
            let second = effective(settings, &interaction.second.0, &interaction.second.1);
            if first && second {
                for layer in &interaction.delta.layers {
                    for (feature, value) in &layer.entries {
                        delta.add(layer.layer, *feature, *value);
                    }
                }
            }
        }
        (delta, wb)
    }

    fn delta_value(
        &self,
        control: &ControlKey,
        value: &ControlValue,
        layer: LayerKind,
        feature: u32,
    ) -> f64 {
        self.controls
            .iter()
            .find(|entry| &entry.key == control)
            .and_then(|entry| entry.values.iter().find(|entry| &entry.value == value))
            .and_then(|entry| {
                entry
                    .delta
                    .layers
                    .iter()
                    .find(|entry| entry.layer == layer)
                    .and_then(|entry| {
                        entry
                            .entries
                            .iter()
                            .find(|(index, _)| *index == feature)
                            .map(|(_, value)| *value)
                    })
            })
            .unwrap_or(0.0)
    }

    fn wb_gain(&self, control: &ControlKey, value: &ControlValue) -> [f64; 2] {
        self.wb
            .controls
            .iter()
            .find(|entry| &entry.key == control)
            .and_then(|entry| entry.values.iter().find(|entry| &entry.value == value))
            .map(|entry| entry.log_gain)
            .unwrap_or([0.0; 2])
    }

    pub fn validate(&self) -> core::result::Result<(), RealizationError> {
        if self.controls.len() > MAX_CONTROLS
            || self.wb.controls.len() > MAX_CONTROLS
            || self.interactions.len() > MAX_INTERACTIONS
            || self.exposure.modes.len() > MAX_EXPOSURE_ROWS
            || self.exposure.steps.len() > MAX_EXPOSURE_ROWS
            || self
                .controls
                .iter()
                .any(|control| control.values.len() > MAX_VALUES)
            || self
                .wb
                .controls
                .iter()
                .any(|control| control.values.len() > MAX_VALUES)
        {
            return Err(RealizationError::Invalid(
                "control response model exceeds its size limits".to_string(),
            ));
        }
        for (index, control) in self.controls.iter().enumerate() {
            if self.controls[..index]
                .iter()
                .any(|other| other.key == control.key)
            {
                return Err(RealizationError::Invalid(format!(
                    "duplicate control {}",
                    control.key
                )));
            }
            for (position, value) in control.values.iter().enumerate() {
                if control.values[..position]
                    .iter()
                    .any(|other| other.value == value.value)
                {
                    return Err(RealizationError::Invalid(format!(
                        "duplicate value on {}",
                        control.key
                    )));
                }
            }
        }
        for (index, control) in self.wb.controls.iter().enumerate() {
            if self.wb.controls[..index]
                .iter()
                .any(|other| other.key == control.key)
            {
                return Err(RealizationError::Invalid(format!(
                    "duplicate WB control {}",
                    control.key
                )));
            }
        }
        for control in &self.controls {
            if control
                .values
                .iter()
                .any(|value| value.value == control.default)
            {
                return Err(RealizationError::Invalid(format!(
                    "{} stores a response at its default",
                    control.key
                )));
            }
            let layers = control.layers();
            for value in &control.values {
                for layer in &value.delta.layers {
                    if !layers.contains(&layer.layer)
                        || layer
                            .entries
                            .iter()
                            .any(|(feature, _)| *feature >= layer.layer.feature_count())
                        || layer.entries.iter().any(|(_, value)| !value.is_finite())
                    {
                        return Err(RealizationError::Invalid(format!(
                            "{} has a delta outside its affects",
                            control.key
                        )));
                    }
                }
            }
        }
        for control in &self.wb.controls {
            if control
                .values
                .iter()
                .any(|value| !value.log_gain.iter().all(|entry| entry.is_finite()))
            {
                return Err(RealizationError::Invalid(format!(
                    "{} has a non-finite WB gain",
                    control.key
                )));
            }
        }
        if !self.exposure.modes.iter().all(|mode| mode.ev.is_finite())
            || !self
                .exposure
                .steps
                .iter()
                .all(|step| step.ev_per_step.is_finite())
        {
            return Err(RealizationError::Invalid(
                "non-finite exposure effect".to_string(),
            ));
        }
        Ok(())
    }
}

/// Realize a Look on a body at settings (docs/04 §2).
pub fn realize(
    look: &Look,
    model: &ControlResponseModel,
    settings: &Settings,
) -> core::result::Result<Realization, RealizationError> {
    model.validate()?;
    let (delta, wb_log) = model.delta(settings);
    let realized = delta.apply(look)?;
    let mut exposure_ev = 0.0;
    for mode in &model.exposure.modes {
        if settings.get(&mode.control) == Some(&mode.value) {
            exposure_ev += mode.ev;
        }
    }
    for step in &model.exposure.steps {
        if let Some(ControlValue::Step(value)) = settings.get(&step.key) {
            exposure_ev += (*value - step.default) as f64 * step.ev_per_step;
        }
    }
    Ok(Realization {
        look: realized,
        wb_gain: [
            libm::exp(model.wb.omega[0] + wb_log[0]),
            1.0,
            libm::exp(model.wb.omega[1] + wb_log[1]),
        ],
        exposure_ev,
    })
}

fn effective(settings: &Settings, key: &ControlKey, value: &ControlValue) -> bool {
    settings.get(key) == Some(value)
}

fn weights(
    default: &ControlValue,
    values: &[impl AsRefValue],
    current: Option<&ControlValue>,
) -> Vec<(ControlValue, f64)> {
    let current = current.cloned().unwrap_or_else(|| default.clone());
    if &current == default {
        return Vec::new();
    }
    match (default, &current) {
        (ControlValue::Step(default), ControlValue::Step(value)) => {
            let target = value - default;
            let offsets: Vec<i64> = values
                .iter()
                .filter_map(|entry| match entry.as_ref() {
                    ControlValue::Step(step) => Some(step - default),
                    _ => None,
                })
                .collect();
            if offsets.contains(&target) {
                return values
                    .iter()
                    .filter_map(|entry| match entry.as_ref() {
                        ControlValue::Step(step) if step - default == target => {
                            Some((entry.as_ref().clone(), 1.0))
                        }
                        _ => None,
                    })
                    .collect();
            }
            let mut points: Vec<i64> = vec![0];
            points.extend(offsets.iter().copied());
            points.sort_unstable();
            points.dedup();
            if target < *points.first().unwrap_or(&0) {
                let high = points[1.min(points.len() - 1)];
                if high == 0 {
                    return Vec::new();
                }
                return values
                    .iter()
                    .filter_map(|entry| match entry.as_ref() {
                        ControlValue::Step(step) if step - default == high => {
                            Some((entry.as_ref().clone(), target as f64 / high as f64))
                        }
                        _ => None,
                    })
                    .collect();
            }
            if target > *points.last().unwrap_or(&0) {
                let low = *points.last().unwrap_or(&0);
                return values
                    .iter()
                    .filter_map(|entry| match entry.as_ref() {
                        ControlValue::Step(step) if step - default == low => {
                            Some((entry.as_ref().clone(), 1.0))
                        }
                        _ => None,
                    })
                    .collect();
            }
            let position = points
                .iter()
                .position(|point| *point > target)
                .unwrap_or(points.len() - 1);
            let high = points[position];
            let low = points[position - 1];
            if high == low {
                return Vec::new();
            }
            let fraction = (target - low) as f64 / (high - low) as f64;
            let mut out = Vec::new();
            if low != 0 {
                for entry in values {
                    if let ControlValue::Step(step) = entry.as_ref()
                        && step - default == low
                    {
                        out.push((entry.as_ref().clone(), 1.0 - fraction));
                    }
                }
            }
            for entry in values {
                if let ControlValue::Step(step) = entry.as_ref()
                    && step - default == high
                {
                    out.push((entry.as_ref().clone(), fraction));
                }
            }
            out
        }
        _ => {
            if values.iter().any(|entry| entry.as_ref() == &current) {
                vec![(current, 1.0)]
            } else {
                Vec::new()
            }
        }
    }
}

trait AsRefValue {
    fn as_ref(&self) -> &ControlValue;
}

impl AsRefValue for ControlValue {
    fn as_ref(&self) -> &ControlValue {
        self
    }
}

impl AsRefValue for WbValueResponse {
    fn as_ref(&self) -> &ControlValue {
        &self.value
    }
}

impl AsRefValue for ValueResponse {
    fn as_ref(&self) -> &ControlValue {
        &self.value
    }
}

fn default_value(control: &Control) -> core::result::Result<ControlValue, RealizationError> {
    match &control.default {
        ControlDefault::Number(number) => {
            let step = control
                .step
                .as_ref()
                .ok_or_else(|| RealizationError::Invalid(format!("{} has no step", control.key)))?;
            let index = number.div_exact(step).ok_or_else(|| {
                RealizationError::Invalid(format!(
                    "{} default is not on its step grid",
                    control.key
                ))
            })?;
            Ok(ControlValue::Step(index))
        }
        ControlDefault::Enum(value) => Ok(ControlValue::Enum(value.clone())),
        ControlDefault::Bool(value) => Ok(ControlValue::Bool(*value)),
    }
}

fn non_default_values(control: &Control, default: &ControlValue) -> Vec<ControlValue> {
    let mut out = Vec::new();
    match control.kind {
        ControlType::Int | ControlType::Decimal => {
            for step in control.steps() {
                let value = ControlValue::Step(step);
                if &value != default {
                    out.push(value);
                }
            }
        }
        ControlType::Enum => {
            if let Some(values) = &control.values {
                for value in values {
                    let value = ControlValue::Enum(value.clone());
                    if &value != default {
                        out.push(value);
                    }
                }
            }
        }
        ControlType::Bool => {
            for value in [false, true] {
                let value = ControlValue::Bool(value);
                if &value != default {
                    out.push(value);
                }
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CrmFile {
    format: String,
    model: ControlResponseModel,
}

/// `crm.json` payload: one control-response model with its format tag.
pub fn encode_crm(model: &ControlResponseModel) -> Result<Vec<u8>> {
    model.validate().map_err(|error| match error {
        RealizationError::Model(error) => error,
        other => ModelError::Invalid(other.to_string()),
    })?;
    let file = CrmFile {
        format: CRM_FORMAT.to_string(),
        model: model.clone(),
    };
    Ok(serde_json::to_vec_pretty(&file)?)
}

pub fn decode_crm(bytes: &[u8]) -> Result<ControlResponseModel> {
    if bytes.len() > MAX_CRM_BYTES {
        return Err(ModelError::Invalid("crm.json is too large".to_string()));
    }
    let file: CrmFile = serde_json::from_slice(bytes)?;
    if file.format != CRM_FORMAT {
        return Err(ModelError::Format(file.format));
    }
    file.model.validate().map_err(|error| match error {
        RealizationError::Model(error) => error,
        other => ModelError::Invalid(other.to_string()),
    })?;
    Ok(file.model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Look;

    fn model() -> ControlResponseModel {
        let key = ControlKey::new("wb_shift_r").expect("key");
        ControlResponseModel {
            body: BodyId::new("test/body").expect("body"),
            epoch: 1,
            base_sim: SimKey::new("sim").expect("sim"),
            version: 1,
            controls: Vec::new(),
            interactions: Vec::new(),
            wb: WbEffect {
                controls: vec![WbControlResponse {
                    key,
                    default: ControlValue::Step(0),
                    values: vec![WbValueResponse {
                        value: ControlValue::Step(1),
                        log_gain: [0.1, -0.1],
                    }],
                }],
                omega: [-0.02, 0.03],
            },
            exposure: ExposureEffect::default(),
            support: Support::default(),
        }
    }

    #[test]
    fn wb_channels_stay_independent_and_omega_is_applied_once() {
        let model = model();
        let look = Look::colorimetric();
        let mut settings = Settings::new();
        settings.set(
            ControlKey::new("wb_shift_r").expect("key"),
            ControlValue::Step(1),
        );
        let realized = realize(&look, &model, &settings).expect("realizes");
        assert!((realized.wb_gain[0] - libm::exp(0.08)).abs() < 1e-12);
        assert!((realized.wb_gain[2] - libm::exp(-0.07)).abs() < 1e-12);
        let neutral = realize(&look, &model, &Settings::new()).expect("realizes");
        assert!((neutral.wb_gain[0] - libm::exp(-0.02)).abs() < 1e-12);
        assert!((neutral.wb_gain[2] - libm::exp(0.03)).abs() < 1e-12);
    }

    #[test]
    fn crm_validation_rejects_duplicate_controls() {
        let mut model = model();
        let duplicate = model.wb.controls[0].clone();
        model.wb.controls.push(duplicate);
        assert!(model.validate().is_err());
    }
}
