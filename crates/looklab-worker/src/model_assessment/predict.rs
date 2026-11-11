// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact realized prediction and per-image conditional nuisances.

use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, Realization, Settings, realize,
};
use looklab_model::{Look, LookInput};
use looklab_samples::{OutputSpace, SampleSet};
use looklab_schema::{ExposureModeKey, FirmwareId, MatchParams, MatchRealizationParams, WbModeKey};

use crate::assessment::median;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Nuisance {
    pub exposure: f64,
    pub wb_r: f64,
    pub wb_b: f64,
}

pub(crate) struct ExactModel {
    look: Look,
    crm: Option<ControlResponseModel>,
    params: MatchParams,
    realization_params: MatchRealizationParams,
    clip_k: f64,
    space: RgbSpace,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
}

impl ExactModel {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        look: Look,
        crm: Option<ControlResponseModel>,
        params: MatchParams,
        realization_params: MatchRealizationParams,
        clip_k: f64,
        space: RgbSpace,
        input_transform_version: u32,
        realization_params_version: u32,
    ) -> Result<Self, String> {
        if !(clip_k.is_finite() && clip_k > 0.0) {
            return Err("realization clip is not positive and finite".to_owned());
        }
        let transform = Mat3(params.transform.camera_to_xyz);
        transform
            .inverse()
            .ok_or_else(|| "body transform is singular".to_owned())?;
        Ok(Self {
            look,
            crm,
            params,
            realization_params,
            clip_k,
            space,
            input_transform_version,
            realization_params_version,
        })
    }

    pub(crate) fn space(&self) -> RgbSpace {
        self.space
    }

    pub(crate) fn look(&self) -> &Look {
        &self.look
    }

    pub(crate) fn has_control_response(&self) -> bool {
        self.crm.is_some()
    }

    pub(crate) fn match_model_with_transform(
        &self,
        covariance: Option<looklab_match::ControlCovariance>,
        transform: Mat3,
    ) -> Result<looklab_match::MatchModel, String> {
        let crm = self
            .crm
            .clone()
            .ok_or_else(|| "realization has no fitted control response".to_owned())?;
        let mut params = self.params.clone();
        params.transform.camera_to_xyz = transform.0;
        let model = looklab_match::MatchModel::new(
            self.look.clone(),
            crm,
            transform,
            covariance,
            self.input_transform_version,
            self.realization_params_version,
            crate::model_assessment::artifacts::output_encoding(self.space),
        )
        .map_err(|error| error.to_string())?;
        model
            .with_params(params)
            .map_err(|error| error.to_string())
            .map(|model| model.with_realization(self.realization_params.clone()))
    }

    pub(crate) fn realize(
        &self,
        settings: &Settings,
        firmware: &FirmwareId,
        wb_mode: &WbModeKey,
        exposure_mode: &ExposureModeKey,
    ) -> Result<Realization, String> {
        let mut look = self.look.clone();
        if let Some(offset) = self
            .realization_params
            .firmware_offset(Some(firmware))
            .map_err(str::to_owned)?
        {
            look = looklab_fit::firmware_look(&look, offset);
        }
        look.clip.k = self.clip_k;
        let mut realization = match &self.crm {
            Some(crm) => realize(&look, crm, settings).map_err(|error| error.to_string())?,
            None => Realization {
                look,
                wb_gain: [1.0, 1.0, 1.0],
                exposure_ev: 0.0,
            },
        };
        realization.exposure_ev += self.exposure_offset(settings, exposure_mode);
        let wb = self
            .params
            .wb_offset(Some(wb_mode))
            .map_err(str::to_owned)?;
        realization.wb_gain[0] *= libm::exp(wb[0]);
        realization.wb_gain[2] *= libm::exp(wb[1]);
        Ok(realization)
    }

    fn exposure_offset(&self, settings: &Settings, exposure_mode: &ExposureModeKey) -> f64 {
        let Some(crm) = &self.crm else {
            return self
                .params
                .exposure
                .iter()
                .filter(|entry| entry.mode == *exposure_mode)
                .map(|entry| entry.offset)
                .sum();
        };
        let mut total = 0.0;
        for entry in &self.params.exposure {
            for mode in &crm.exposure.modes {
                if let ControlValue::Enum(value) = &mode.value
                    && value.as_str() == entry.mode.as_str()
                    && settings.get(&mode.control) == Some(&mode.value)
                {
                    total += entry.offset;
                }
            }
        }
        total
    }

    pub(crate) fn predict_camera(
        &self,
        realization: &Realization,
        transform: Mat3,
        rgb: [f64; 3],
        nuisance: Nuisance,
    ) -> (Ucs, LookInput) {
        let exposure = libm::pow(2.0, realization.exposure_ev + nuisance.exposure);
        let gains = [
            realization.wb_gain[0] * libm::exp(nuisance.wb_r) * exposure,
            realization.wb_gain[1] * exposure,
            realization.wb_gain[2] * libm::exp(nuisance.wb_b) * exposure,
        ];
        let scene = Xyz::from_array(transform.mul_vec([
            rgb[0] * gains[0],
            rgb[1] * gains[1],
            rgb[2] * gains[2],
        ]));
        (
            self.predict_scene(realization, scene),
            LookInput::new(scene),
        )
    }

    pub(crate) fn predict_scene(&self, realization: &Realization, scene: Xyz) -> Ucs {
        looklab_fit::post_clip(&realization.look, scene, self.space, self.clip_k)
    }
}

pub(crate) fn observed_ucs(samples: &SampleSet, index: usize, space: RgbSpace) -> Ucs {
    let rgb = Rgb::from_array(samples.out_rgb()[index].map(f64::from));
    looklab_model::solve::ucs_of_xyz(space.rgb_linear_to_xyz(rgb))
}

pub(crate) fn output_space(space: OutputSpace) -> RgbSpace {
    match space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    }
}

pub(crate) fn condition_nuisances(
    model: &ExactModel,
    realization: &Realization,
    transform: Mat3,
    samples: &SampleSet,
    admitted: &[bool],
    space: RgbSpace,
) -> Nuisance {
    let indices: Vec<usize> = admitted
        .iter()
        .enumerate()
        .filter_map(|(index, admitted)| (*admitted).then_some(index))
        .collect();
    if indices.len() < 2 {
        return Nuisance::default();
    }
    let stride = indices.len().div_ceil(32);
    let probe: Vec<usize> = indices.iter().copied().step_by(stride).collect();
    let observed: Vec<Ucs> = probe
        .iter()
        .map(|index| observed_ucs(samples, *index, space))
        .collect();
    let cost = |nuisance: Nuisance| -> f64 {
        let errors: Vec<f64> = probe
            .iter()
            .zip(&observed)
            .map(|(index, target)| {
                let (predicted, _) = model.predict_camera(
                    realization,
                    transform,
                    samples.in_rgb()[*index].map(f64::from),
                    nuisance,
                );
                delta_e_ucs(predicted, *target)
            })
            .collect();
        median(&errors).unwrap_or(f64::INFINITY)
    };
    let mut nuisance = Nuisance::default();
    let mut best = cost(nuisance);
    let mut steps = [0.25_f64, 0.02, 0.02];
    for _ in 0..7 {
        for axis in 0..3 {
            for sign in [-1.0_f64, 1.0] {
                let mut trial = nuisance;
                match axis {
                    0 => trial.exposure += sign * steps[0],
                    1 => trial.wb_r += sign * steps[1],
                    _ => trial.wb_b += sign * steps[2],
                }
                trial.exposure = trial.exposure.clamp(-2.0, 2.0);
                trial.wb_r = trial.wb_r.clamp(-0.2, 0.2);
                trial.wb_b = trial.wb_b.clamp(-0.2, 0.2);
                let value = cost(trial);
                if value < best {
                    best = value;
                    nuisance = trial;
                }
            }
        }
        for step in &mut steps {
            *step *= 0.5;
        }
    }
    nuisance
}
