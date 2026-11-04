// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, Xyz};
use looklab_model::realization::{
    ControlResponseModel, ControlValue, Realization, Settings, realize,
};
use looklab_model::{Look, LookInput};
use looklab_schema::{ControlKey, SimKey};

use crate::covariance::{ControlCovariance, CovarianceGroup, SparseDelta};
use crate::error::{MatchError, Result};

#[derive(Clone, Debug)]
pub struct MatchModel {
    pub base_look: Look,
    pub crm: ControlResponseModel,
    pub transform: Mat3,
    pub(crate) inverse: Mat3,
    pub covariance: Option<ControlCovariance>,
    pub groups: Vec<CovarianceGroup>,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
}

impl MatchModel {
    pub fn new(
        base_look: Look,
        crm: ControlResponseModel,
        transform: Mat3,
        covariance: Option<ControlCovariance>,
        input_transform_version: u32,
        realization_params_version: u32,
    ) -> Result<Self> {
        crm.validate()
            .map_err(|error| MatchError::Schema(error.to_string()))?;
        let inverse = transform.inverse().ok_or(MatchError::SingularTransform)?;
        let groups = match &covariance {
            Some(covariance) => covariance.groups(&base_look, &crm)?,
            None => Vec::new(),
        };
        Ok(Self {
            base_look,
            crm,
            transform,
            inverse,
            covariance,
            groups,
            input_transform_version,
            realization_params_version,
        })
    }

    pub fn sim(&self) -> &SimKey {
        &self.crm.base_sim
    }

    pub fn version(&self) -> u32 {
        self.crm.version
    }

    pub fn realize(&self, settings: &Settings) -> Result<Realization> {
        realize(&self.base_look, &self.crm, settings)
            .map_err(|error| MatchError::Schema(error.to_string()))
    }

    pub fn input_xyz(&self, realization: &Realization, xyz: Xyz) -> Xyz {
        let camera = self.inverse.mul_vec(xyz.to_array());
        let exposure = libm::pow(2.0, realization.exposure_ev);
        let gains = [
            realization.wb_gain[0] * exposure,
            realization.wb_gain[1] * exposure,
            realization.wb_gain[2] * exposure,
        ];
        Xyz::from_array(self.transform.mul_vec([
            camera[0] * gains[0],
            camera[1] * gains[1],
            camera[2] * gains[2],
        ]))
    }

    pub fn input_ucs(
        &self,
        realization: &Realization,
        xyz: Xyz,
        extra_wb: [f64; 2],
        extra_exposure: f64,
    ) -> Ucs {
        let camera = self.inverse.mul_vec(xyz.to_array());
        let exposure = libm::pow(2.0, realization.exposure_ev + extra_exposure);
        let gains = [
            realization.wb_gain[0] * libm::exp(extra_wb[0]) * exposure,
            realization.wb_gain[1] * exposure,
            realization.wb_gain[2] * libm::exp(extra_wb[1]) * exposure,
        ];
        let input = Xyz::from_array(self.transform.mul_vec([
            camera[0] * gains[0],
            camera[1] * gains[1],
            camera[2] * gains[2],
        ]));
        realization.look.eval(input)
    }

    pub fn predict(&self, settings: &Settings, xyz: Xyz) -> Result<Ucs> {
        let realization = self.realize(settings)?;
        Ok(self.input_ucs(&realization, xyz, [0.0, 0.0], 0.0))
    }

    pub fn scene_from_camera(&self, rgb: [f64; 3]) -> Xyz {
        Xyz::from_array(self.transform.mul_vec(rgb))
    }

    pub fn camera_from_scene(&self, xyz: Xyz) -> [f64; 3] {
        self.inverse.mul_vec(xyz.to_array())
    }

    pub fn camera_from_xyz(&self, realization: &Realization, xyz: Xyz) -> [f64; 3] {
        let camera = self.inverse.mul_vec(xyz.to_array());
        let exposure = libm::pow(2.0, realization.exposure_ev);
        [
            camera[0] / (realization.wb_gain[0] * exposure),
            camera[1] / (realization.wb_gain[1] * exposure),
            camera[2] / (realization.wb_gain[2] * exposure),
        ]
    }

    pub fn xyz_from_camera(&self, realization: &Realization, rgb: [f64; 3]) -> Xyz {
        let exposure = libm::pow(2.0, realization.exposure_ev);
        Xyz::from_array(self.transform.mul_vec([
            rgb[0] * realization.wb_gain[0] * exposure,
            rgb[1] * realization.wb_gain[1] * exposure,
            rgb[2] * realization.wb_gain[2] * exposure,
        ]))
    }

    pub fn value_row(
        &self,
        key: &ControlKey,
        value: &ControlValue,
    ) -> Result<(SparseDelta, [f64; 2], f64)> {
        let mut single = Settings::new();
        single.set(key.clone(), value.clone());
        let (delta, wb) = self.crm.delta(&single);
        let entries = delta.coefficient_entries(&self.base_look);
        let mut exposure = 0.0_f64;
        for mode in &self.crm.exposure.modes {
            if single.get(&mode.control) == Some(&mode.value) {
                exposure += mode.ev;
            }
        }
        for step in &self.crm.exposure.steps {
            if let Some(ControlValue::Step(value)) = single.get(&step.key) {
                exposure += (*value - step.default) as f64 * step.ev_per_step;
            }
        }
        Ok((SparseDelta::from_entries(entries), wb, exposure))
    }

    pub fn coefficient_jacobian(
        &self,
        realization: &Realization,
        xyz: Xyz,
    ) -> Vec<looklab_model::CoefTerm> {
        let input = LookInput::new(self.input_xyz(realization, xyz));
        realization.look.coef_jacobian(&input)
    }

    pub fn value_weight(&self, key: &ControlKey, value: &ControlValue, settings: &Settings) -> f64 {
        self.crm
            .value_weights(key, settings)
            .into_iter()
            .find(|(entry, _)| entry == value)
            .map(|(_, weight)| weight)
            .unwrap_or(0.0)
    }

    pub fn scaled(&self, factor: f64) -> Self {
        let mut crm = self.crm.clone();
        for control in &mut crm.controls {
            for value in &mut control.values {
                for layer in &mut value.delta.layers {
                    for (_, coefficient) in &mut layer.entries {
                        *coefficient *= factor;
                    }
                }
            }
        }
        for control in &mut crm.wb.controls {
            for value in &mut control.values {
                value.log_gain[0] *= factor;
                value.log_gain[1] *= factor;
            }
        }
        Self {
            base_look: self.base_look.clone(),
            crm,
            transform: self.transform,
            inverse: self.inverse,
            covariance: self.covariance.clone(),
            groups: self.groups.clone(),
            input_transform_version: self.input_transform_version,
            realization_params_version: self.realization_params_version,
        }
    }

    pub fn with_crm(&self, crm: ControlResponseModel) -> Result<Self> {
        Self::new(
            self.base_look.clone(),
            crm,
            self.transform,
            self.covariance.clone(),
            self.input_transform_version,
            self.realization_params_version,
        )
    }

    pub fn realization_module_version() -> &'static str {
        "looklab-model/realization/1"
    }
}
