// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, RgbSpace, Xyz};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, Delta, LayerDelta, LayerKind, Settings,
};
use looklab_model::solve::reference_model;
use looklab_model::{Look, ProbeSet};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_schema::{ControlSchema, SimKey};
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

use crate::error::{MatchError, Result};
use crate::model::MatchModel;
use crate::settings::Plan;
use crate::target::TargetFunction;

pub const SYNTH_SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
pub const SYNTH_SIM: &str = "classic_chrome";

pub struct SyntheticBody {
    pub model: MatchModel,
    pub settings: Settings,
}

impl SyntheticBody {
    pub fn target(&self) -> RealizationTarget {
        RealizationTarget {
            model: self.model.clone(),
            settings: self.settings.clone(),
        }
    }

    pub fn degraded(&self, factor: f64) -> Self {
        Self {
            model: self.model.scaled(factor),
            settings: self.settings.clone(),
        }
    }
}

pub struct RealizationTarget {
    pub model: MatchModel,
    pub settings: Settings,
}

impl TargetFunction for RealizationTarget {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.model
            .predict(&self.settings, xyz)
            .expect("synthetic control-response model is valid")
    }
}

pub fn schema() -> ControlSchema {
    looklab_schema::validate_control_schema(SYNTH_SCHEMA).expect("example schema is valid")
}

pub fn plan() -> Plan {
    Plan::new(&schema(), 1).expect("example epoch exists")
}

pub fn probes() -> ProbeSet {
    looklab_model::probes::v1()
}

pub fn transform() -> Mat3 {
    RgbSpace::AdobeRgb.to_xyz_matrix()
}

pub fn body(seed: u64) -> Result<SyntheticBody> {
    let schema = schema();
    let plan = plan();
    let mut rng = ChaCha12Rng::seed_from_u64(seed);
    let base_look = looklab_fit::synth::truth_look(seed);
    let sim = SimKey::new(SYNTH_SIM).map_err(|error| MatchError::Schema(error.to_string()))?;
    let mut crm = ControlResponseModel::zeroed(&schema, 1, &sim, 1)
        .map_err(|error| MatchError::Schema(error.to_string()))?;
    let mut settings = plan.default.clone();

    for control in &mut crm.controls {
        if !plan.by_key.contains_key(&control.key) || control.values.is_empty() {
            continue;
        }
        let pick = 1 + (rng.next_u32() as usize % control.values.len());
        let value_index = pick % control.values.len();
        let value = control.values[value_index].value.clone();
        let layers = control.layers();
        let mut delta = Delta::default();
        for layer in layers {
            let features = layer.feature_count();
            let entries = match layer {
                LayerKind::Tone | LayerKind::Tint => {
                    let count = 2 + (rng.next_u32() as usize % 3);
                    let mut chosen = Vec::with_capacity(count);
                    for _ in 0..count {
                        let feature = rng.next_u32() % features;
                        if !chosen.iter().any(|(entry, _)| *entry == feature) {
                            let value = match layer {
                                LayerKind::Tone => 0.35 * gaussian(&mut rng),
                                _ => 0.6 * gaussian(&mut rng),
                            };
                            chosen.push((feature, value));
                        }
                    }
                    chosen
                }
                _ => (0..features)
                    .map(|feature| (feature, 0.25 * gaussian(&mut rng)))
                    .collect(),
            };
            delta.layers.push(LayerDelta { layer, entries });
        }
        control.values[value_index].delta = delta;
        settings.set(control.key.clone(), value);
    }

    for control in &mut crm.wb.controls {
        if !plan.by_key.contains_key(&control.key) {
            continue;
        }
        if control.values.is_empty() {
            continue;
        }
        let value_index = rng.next_u32() as usize % control.values.len();
        let value = control.values[value_index].value.clone();
        let magnitude = 0.006 + 0.006 * rng.next_u32() as f64 / u32::MAX as f64;
        let sign = if rng.next_u32() & 1 == 0 { 1.0 } else { -1.0 };
        control.values[value_index].log_gain = if control.key.as_str().ends_with("_r") {
            [sign * magnitude, 0.0]
        } else {
            [0.0, sign * magnitude]
        };
        settings.set(control.key.clone(), value);
    }

    for mode in &mut crm.exposure.modes {
        if mode.value
            == ControlValue::Enum(
                looklab_schema::EnumValue::new("dr200")
                    .map_err(|error| MatchError::Schema(error.to_string()))?,
            )
        {
            mode.ev = -0.25;
            settings.set(mode.control.clone(), mode.value.clone());
        }
    }
    let ec = plan
        .controls
        .iter()
        .position(|control| control.key.as_str() == "exposure_compensation");
    if let Some(index) = ec {
        let steps = plan.domains[index].values.len();
        let value_index = 1 + (rng.next_u32() as usize % (steps - 1));
        settings.set(
            plan.domains[index].key.clone(),
            plan.domains[index].values[value_index].clone(),
        );
    }

    crm.validate()
        .map_err(|error| MatchError::Schema(error.to_string()))?;
    let model = MatchModel::new(base_look, crm, transform(), None, 1, 1)?;
    Ok(SyntheticBody { model, settings })
}

pub fn degraded(seed: u64, factor: f64) -> Result<SyntheticBody> {
    Ok(body(seed)?.degraded(factor))
}

fn gaussian(rng: &mut ChaCha12Rng) -> f64 {
    let u1 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    let u2 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    (-2.0 * libm::log(u1)).sqrt() * libm::cos(2.0 * core::f64::consts::PI * u2)
}

/// Samples obey the fitting contract: `in_rgb = A_c⁻¹ · scene`, without the
/// realization's WB or exposure effects.
pub fn observed(
    model: &MatchModel,
    settings: &Settings,
    probes: &ProbeSet,
    sample_limit: usize,
) -> Result<SampleSet> {
    let mut in_rgb = Vec::new();
    let mut out_rgb = Vec::new();
    let stride = (probes.probes.len() / sample_limit.max(1)).max(1);
    for probe in probes.probes.iter().step_by(stride) {
        let scene = probe.xyz;
        let camera = model.camera_from_scene(scene);
        if camera
            .iter()
            .any(|value| !value.is_finite() || *value < -0.4 || *value > 32.0)
        {
            continue;
        }
        let prediction = model.predict(settings, scene)?;
        let jmh = ucs_to_jmh(prediction, UcsSpace::Ucs);
        let xyz = reference_model().jmh_to_xyz(jmh);
        let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
        if [linear.r, linear.g, linear.b]
            .iter()
            .any(|value| !(0.02..=0.98).contains(value))
        {
            continue;
        }
        in_rgb.push(camera.map(|value| value as f32));
        out_rgb.push([linear.r, linear.g, linear.b].map(|value| value as f32));
    }
    if in_rgb.is_empty() {
        return Err(MatchError::EmptyObserved);
    }
    let n = in_rgb.len();
    let header = SampleHeader {
        n: n as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 0.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(0),
        prior_transform_version: PriorTransformVersion::new(0),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    };
    SampleSet::new(
        header,
        in_rgb,
        out_rgb,
        vec![1.0; n],
        vec![SampleFlags::NONE; n],
    )
    .map_err(MatchError::Samples)
}

pub fn true_look(seed: u64) -> Look {
    looklab_fit::synth::truth_look(seed)
}
