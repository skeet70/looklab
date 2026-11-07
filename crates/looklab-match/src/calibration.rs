// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-sim multiplicative calibration carried by a permalink.

use std::collections::BTreeMap;

use looklab_schema::{ControlKey, SimKey};
use serde::{Deserialize, Serialize};

use crate::error::{MatchError, Result};
use crate::model::MatchModel;
use crate::settings::Plan;
use crate::verify::scale_response;

/// Accepted factor range is `MIN_FACTOR..=MAX_FACTOR` per control.
pub const MIN_FACTOR: f64 = 1e-4;
pub const MAX_FACTOR: f64 = 1e4;

pub const MAX_SIMS: usize = 32;
pub const MAX_CONTROLS: usize = 64;
const MAX_FIELD_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    sims: Vec<SimCalibration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimCalibration {
    sim: SimKey,
    controls: Vec<ControlScale>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlScale {
    control: ControlKey,
    factor: f64,
}

impl Calibration {
    pub fn is_empty(&self) -> bool {
        self.sims.is_empty()
    }

    /// # Errors
    /// A factor outside `MIN_FACTOR..=MAX_FACTOR`, a duplicate entry, or exceeded bounds.
    pub fn merged(&self, sim: &SimKey, scales: &[(ControlKey, f64)]) -> Result<Self> {
        let mut next = self.clone();
        if !scales.is_empty() {
            let index = match next.sims.iter().position(|entry| &entry.sim == sim) {
                Some(index) => index,
                None => {
                    next.sims.push(SimCalibration {
                        sim: sim.clone(),
                        controls: Vec::new(),
                    });
                    next.sims.len() - 1
                }
            };
            let entry = &mut next.sims[index];
            for (control, factor) in scales {
                check_factor(*factor)?;
                match entry
                    .controls
                    .iter_mut()
                    .find(|scale| &scale.control == control)
                {
                    Some(scale) => scale.factor = check_factor(scale.factor * factor)?,
                    None => entry.controls.push(ControlScale {
                        control: control.clone(),
                        factor: *factor,
                    }),
                }
            }
            entry.controls.retain(|scale| scale.factor != 1.0);
        }
        next.sims.retain(|entry| !entry.controls.is_empty());
        next.normalize()?;
        Ok(next)
    }

    /// # Errors
    /// Unknown sim or control, out-of-range factor, duplicate entry, or non-canonical order.
    pub fn validate(&self, sims: &[SimKey], plan: &Plan) -> Result<()> {
        for entry in &self.sims {
            if !sims.contains(&entry.sim) {
                return Err(MatchError::Permalink(format!(
                    "calibration names unknown sim {}",
                    entry.sim
                )));
            }
            for scale in &entry.controls {
                if !plan.by_key.contains_key(&scale.control) {
                    return Err(MatchError::Permalink(format!(
                        "calibration names unknown control {} for {}",
                        scale.control, entry.sim
                    )));
                }
            }
        }
        let mut normalized = self.clone();
        normalized.normalize()?;
        if normalized != *self {
            return Err(MatchError::Permalink(
                "calibration entries are not in canonical order".to_string(),
            ));
        }
        Ok(())
    }

    /// # Errors
    /// An out-of-range factor or an invalid control-response model.
    pub fn apply(&self, model: &MatchModel) -> Result<MatchModel> {
        let Some(entry) = self.sims.iter().find(|entry| &entry.sim == model.sim()) else {
            return Ok(model.clone());
        };
        let mut crm = model.crm.clone();
        for scale in &entry.controls {
            check_factor(scale.factor)?;
            scale_response(&mut crm, &scale.control, scale.factor);
        }
        model.with_crm(crm)
    }

    /// # Errors
    /// Malformed entry, invalid identifier or factor, duplicate, or exceeded bounds.
    pub fn parse(field: &str) -> Result<Self> {
        if field.is_empty() {
            return Err(MatchError::Permalink(
                "calibration field is empty".to_string(),
            ));
        }
        if field.len() > MAX_FIELD_BYTES {
            return Err(MatchError::Permalink(format!(
                "calibration field is {} bytes, more than {MAX_FIELD_BYTES}",
                field.len()
            )));
        }
        let mut sims: BTreeMap<SimKey, BTreeMap<ControlKey, f64>> = BTreeMap::new();
        for entry in field.split(';') {
            if entry.is_empty() {
                return Err(MatchError::Permalink(
                    "calibration has an empty entry".to_string(),
                ));
            }
            let (sim, rest) = entry
                .split_once('@')
                .ok_or_else(|| MatchError::Permalink("calibration entry sim".to_string()))?;
            let (control, factor) = rest
                .split_once('=')
                .ok_or_else(|| MatchError::Permalink("calibration entry factor".to_string()))?;
            if control.contains('@') || control.contains('=') || factor.contains('@') {
                return Err(MatchError::Permalink(
                    "calibration entry separator".to_string(),
                ));
            }
            let sim = SimKey::new(&decode_component(sim)?)
                .map_err(|error| MatchError::Permalink(error.to_string()))?;
            let control = ControlKey::new(&decode_component(control)?)
                .map_err(|error| MatchError::Permalink(error.to_string()))?;
            let factor = parse_factor(factor)?;
            if !sims.contains_key(&sim) && sims.len() >= MAX_SIMS {
                return Err(MatchError::Permalink(format!(
                    "calibration has more than {MAX_SIMS} sims"
                )));
            }
            let controls = sims.entry(sim.clone()).or_default();
            if controls.contains_key(&control) {
                return Err(MatchError::Permalink(format!(
                    "calibration repeats control {control} for {sim}"
                )));
            }
            if controls.len() >= MAX_CONTROLS {
                return Err(MatchError::Permalink(format!(
                    "calibration has more than {MAX_CONTROLS} controls for {sim}"
                )));
            }
            controls.insert(control, factor);
        }
        let mut calibration = Self {
            sims: sims
                .into_iter()
                .map(|(sim, controls)| SimCalibration {
                    sim,
                    controls: controls
                        .into_iter()
                        .map(|(control, factor)| ControlScale { control, factor })
                        .collect(),
                })
                .collect(),
        };
        calibration.normalize()?;
        Ok(calibration)
    }

    pub fn to_field(&self) -> String {
        let mut out = String::new();
        for (entry_index, entry) in self.sims.iter().enumerate() {
            for (scale_index, scale) in entry.controls.iter().enumerate() {
                if entry_index > 0 || scale_index > 0 {
                    out.push(';');
                }
                out.push_str(&encode_component(entry.sim.as_str()));
                out.push('@');
                out.push_str(&encode_component(scale.control.as_str()));
                out.push('=');
                out.push_str(&scale.factor.to_string());
            }
        }
        out
    }

    fn normalize(&mut self) -> Result<()> {
        self.sims.sort_by(|left, right| left.sim.cmp(&right.sim));
        if self.sims.len() > MAX_SIMS {
            return Err(MatchError::Permalink(format!(
                "calibration has {} sims, more than {MAX_SIMS}",
                self.sims.len()
            )));
        }
        for pair in self.sims.windows(2) {
            if pair[0].sim == pair[1].sim {
                return Err(MatchError::Permalink(format!(
                    "calibration repeats sim {}",
                    pair[0].sim
                )));
            }
        }
        for entry in &mut self.sims {
            if entry.controls.is_empty() {
                return Err(MatchError::Permalink(format!(
                    "calibration has no controls for {}",
                    entry.sim
                )));
            }
            entry
                .controls
                .sort_by(|left, right| left.control.cmp(&right.control));
            if entry.controls.len() > MAX_CONTROLS {
                return Err(MatchError::Permalink(format!(
                    "calibration has {} controls for {}, more than {MAX_CONTROLS}",
                    entry.controls.len(),
                    entry.sim
                )));
            }
            for pair in entry.controls.windows(2) {
                if pair[0].control == pair[1].control {
                    return Err(MatchError::Permalink(format!(
                        "calibration repeats control {} for {}",
                        pair[0].control, entry.sim
                    )));
                }
            }
            for scale in &entry.controls {
                check_factor(scale.factor)?;
            }
        }
        Ok(())
    }
}

fn check_factor(factor: f64) -> Result<f64> {
    if !factor.is_finite() || !(MIN_FACTOR..=MAX_FACTOR).contains(&factor) {
        return Err(MatchError::Permalink(format!(
            "calibration factor {factor} is outside {MIN_FACTOR}..={MAX_FACTOR}"
        )));
    }
    Ok(factor)
}

fn parse_factor(value: &str) -> Result<f64> {
    let factor: f64 = value
        .parse()
        .map_err(|_| MatchError::Permalink(format!("calibration factor {value:?}")))?;
    check_factor(factor)
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn decode_component(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value
                .get(index + 1..index + 3)
                .ok_or_else(|| MatchError::Permalink("truncated escape".to_string()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| MatchError::Permalink("escape".to_string()))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| MatchError::Permalink("utf-8".to_string()))
}
