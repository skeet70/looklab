// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use looklab_model::realization::{ControlValue, Settings};
use looklab_schema::{
    Control, ControlDefault, ControlKey, ControlSchema, ControlType, Epoch, Rational,
};

use crate::error::{MatchError, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct ControlDomain {
    pub key: ControlKey,
    pub values: Vec<ControlValue>,
    pub default: usize,
}

impl ControlDomain {
    fn build(control: &Control) -> Result<Self> {
        let default = default_value(control)?;
        let values: Vec<ControlValue> = match control.kind {
            ControlType::Int | ControlType::Decimal => {
                control.steps().map(ControlValue::Step).collect::<Vec<_>>()
            }
            ControlType::Enum => control
                .values
                .as_ref()
                .map(|values| values.iter().cloned().map(ControlValue::Enum).collect())
                .ok_or_else(|| MatchError::Schema(format!("{} has no values", control.key)))?,
            ControlType::Bool => vec![ControlValue::Bool(false), ControlValue::Bool(true)],
        };
        if values.is_empty() {
            return Err(MatchError::Schema(format!(
                "{} has an empty domain",
                control.key
            )));
        }
        let default_index = values
            .iter()
            .position(|value| value == &default)
            .ok_or_else(|| {
                MatchError::Schema(format!("{} default is outside its domain", control.key))
            })?;
        Ok(Self {
            key: control.key.clone(),
            values,
            default: default_index,
        })
    }

    pub fn default_value(&self) -> &ControlValue {
        &self.values[self.default]
    }

    pub fn index_of(&self, value: &ControlValue) -> Option<usize> {
        self.values.iter().position(|entry| entry == value)
    }
}

fn default_value(control: &Control) -> Result<ControlValue> {
    match &control.default {
        ControlDefault::Number(number) => {
            let step = control
                .step
                .as_ref()
                .ok_or_else(|| MatchError::Schema(format!("{} has no step", control.key)))?;
            let index = number.div_exact(step).ok_or_else(|| {
                MatchError::Schema(format!("{} default is not on its step grid", control.key))
            })?;
            Ok(ControlValue::Step(index))
        }
        ControlDefault::Enum(value) => Ok(ControlValue::Enum(value.clone())),
        ControlDefault::Bool(value) => Ok(ControlValue::Bool(*value)),
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub epoch: u32,
    pub controls: Vec<Control>,
    pub domains: Vec<ControlDomain>,
    pub by_key: BTreeMap<ControlKey, usize>,
    pub stages: Vec<Vec<usize>>,
    pub plan_controls: Vec<usize>,
    pub default: Settings,
    assessed: Option<crate::domain::AssessedDomain>,
}

impl Plan {
    pub fn new(schema: &ControlSchema, epoch: u32) -> Result<Self> {
        let epoch_schema = schema
            .epochs
            .iter()
            .find(|entry| entry.id == epoch)
            .ok_or(MatchError::UnknownEpoch(epoch))?;
        Self::from_epoch(epoch_schema)
    }

    pub fn from_epoch(epoch_schema: &Epoch) -> Result<Self> {
        let mut domains = Vec::with_capacity(epoch_schema.controls.len());
        let mut by_key = BTreeMap::new();
        for (index, control) in epoch_schema.controls.iter().enumerate() {
            domains.push(ControlDomain::build(control)?);
            by_key.insert(control.key.clone(), index);
        }

        let mut stages = Vec::with_capacity(epoch_schema.solve_plan.len());
        let mut plan_controls = Vec::new();
        for step in &epoch_schema.solve_plan {
            let mut stage = Vec::with_capacity(step.controls.len());
            for key in &step.controls {
                let index = *by_key
                    .get(key)
                    .ok_or_else(|| MatchError::UnknownControl(key.to_string()))?;
                stage.push(index);
                if !plan_controls.contains(&index) {
                    plan_controls.push(index);
                }
            }
            stages.push(stage);
        }

        let mut default = Settings::new();
        for domain in &domains {
            default.set(domain.key.clone(), domain.default_value().clone());
        }

        Ok(Self {
            epoch: epoch_schema.id,
            controls: epoch_schema.controls.clone(),
            domains,
            by_key,
            stages,
            plan_controls,
            default,
            assessed: None,
        })
    }

    pub fn with_assessed(mut self, assessed: crate::domain::AssessedDomain) -> Self {
        self.assessed = Some(assessed);
        self
    }

    pub fn assessed(&self) -> Option<&crate::domain::AssessedDomain> {
        self.assessed.as_ref()
    }

    pub fn key(&self, index: usize) -> &ControlKey {
        &self.domains[index].key
    }

    pub fn value(&self, index: usize, settings: &Settings) -> ControlValue {
        settings
            .get(&self.domains[index].key)
            .cloned()
            .unwrap_or_else(|| self.domains[index].default_value().clone())
    }

    pub fn value_index(&self, index: usize, settings: &Settings) -> usize {
        let value = self.value(index, settings);
        self.domains[index]
            .index_of(&value)
            .unwrap_or(self.domains[index].default)
    }

    pub fn set(&self, index: usize, value: usize, settings: &mut Settings) {
        settings.set(
            self.domains[index].key.clone(),
            self.domains[index].values[value].clone(),
        );
    }

    pub fn candidate(&self, base: &Settings, index: usize, value: usize) -> Settings {
        let mut out = base.clone();
        self.set(index, value, &mut out);
        out
    }

    pub fn default_index(&self, index: usize) -> usize {
        self.domains[index].default
    }

    pub fn label(&self, index: usize, value: usize) -> String {
        format_value(&self.controls[index], &self.domains[index].values[value])
    }

    pub fn current_label(&self, index: usize, settings: &Settings) -> String {
        self.label(index, self.value_index(index, settings))
    }

    pub fn is_default(&self, index: usize, settings: &Settings) -> bool {
        self.value_index(index, settings) == self.domains[index].default
    }

    pub fn stage_controls(&self) -> impl Iterator<Item = usize> + '_ {
        self.plan_controls.iter().copied()
    }
}

fn format_value(control: &Control, value: &ControlValue) -> String {
    match value {
        ControlValue::Step(step) => {
            let number = control
                .value_at(*step)
                .unwrap_or_else(|| Rational::from_integer(*step));
            if control.key.as_str().contains("exposure_compensation") {
                format!("{:+.2} EV", number.to_f64())
            } else {
                signed(number)
            }
        }
        ControlValue::Enum(value) => value.to_string(),
        ControlValue::Bool(value) => {
            if *value {
                "on".to_string()
            } else {
                "off".to_string()
            }
        }
    }
}

fn signed(number: Rational) -> String {
    if number.den == 1 {
        format!("{:+}", number.num)
    } else {
        let mut text = format!("{:+.2}", number.to_f64());
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
        text
    }
}
