// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::Xyz;
use looklab_color::ucs::Ucs;
use looklab_model::domain_penalty::check_published_paths;
use looklab_model::file::{LookDocument, UncertaintyData};
use looklab_model::{Look, LookInput, ProbeGroup, ProbeSet};
use serde::{Deserialize, Serialize};

use crate::emphasis::Emphasis;
use crate::error::{MatchError, Result};
use crate::linear::ProbeSubset;
use crate::metric::{AssessmentStatus, OutputContext, OutputPin, ScoringMetric, TargetAppearance};
use crate::model::MatchModel;

pub trait TargetFunction {
    fn predict(&self, xyz: Xyz) -> Ucs;

    fn look(&self) -> Option<&Look> {
        None
    }
}

impl TargetFunction for Look {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.eval(xyz)
    }

    fn look(&self) -> Option<&Look> {
        Some(self)
    }
}

pub struct TargetSpec<'a> {
    pub probes: &'a ProbeSet,
    pub function: &'a dyn TargetFunction,
    pub uncertainty: Option<&'a UncertaintyData>,
    pub emphasis: Emphasis,
    pub tau: f64,
    pub support_sigma: f64,
    pub density_floor: Option<f64>,
    pub appearance: TargetAppearance,
}

/// The rendered images a published target's estimate rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImageCount(u32);

impl ImageCount {
    pub const fn new(count: u32) -> Self {
        Self(count)
    }

    /// The images the document's provenance records.
    pub fn of_document(document: &LookDocument) -> Self {
        Self(
            document
                .file
                .provenance
                .bodies
                .iter()
                .map(|body| body.images)
                .sum(),
        )
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// How a prepared target chose its supported probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetSupport {
    /// The published support mask, or the sigma and density predicates when the
    /// target stores no mask.
    Published,
    /// The published mask supported nothing, so the sigma and density predicates
    /// chose the probes.
    Experimental(ImageCount),
}

pub struct PreparedTarget<'a> {
    pub probes: &'a ProbeSet,
    pub function: &'a dyn TargetFunction,
    pub emphasis: Emphasis,
    pub tau: f64,
    pub appearance: TargetAppearance,
    /// The requested assessment point count before any qualified probe filtering.
    pub declared_points: usize,
    pub target_uncertainty: AssessmentStatus,
    pub supported: Vec<usize>,
    pub sigma2: Vec<f64>,
    /// `probe.weight × emphasis × c_p`; zero for unsupported probes.
    pub weight: Vec<f64>,
    /// `probe.weight × emphasis`, without `c_p`.
    pub covariance_weight: Vec<f64>,
    pub target: Vec<Ucs>,
    pub density_floor: f64,
    pub support: TargetSupport,
}

struct ProbeSelection {
    supported: Vec<usize>,
    weight: Vec<f64>,
    covariance_weight: Vec<f64>,
}

fn select_probes(
    spec: &TargetSpec<'_>,
    sigma2: &[f64],
    mask: Option<&[f64]>,
    density_floor: f64,
) -> ProbeSelection {
    let mut selection = ProbeSelection {
        supported: Vec::new(),
        weight: vec![0.0_f64; sigma2.len()],
        covariance_weight: vec![0.0_f64; sigma2.len()],
    };
    for (index, probe) in spec.probes.probes.iter().enumerate() {
        let sigma = sigma2[index].max(0.0).sqrt();
        let dense = spec
            .uncertainty
            .and_then(|data| data.density.as_ref())
            .and_then(|density| density.values.get(index).copied())
            .unwrap_or(f64::INFINITY);
        let render_feasible = spec.appearance.metric().is_legacy()
            || spec.function.look().is_none_or(|look| {
                let input = LookInput::new(probe.xyz);
                let values = look.layer_values(&input);
                check_published_paths(&look.chroma_paths(&input, &values)).is_ok()
            });
        let accepted = render_feasible
            && mask.map_or_else(
                || sigma <= spec.support_sigma && dense >= density_floor,
                |values| values[index] > 0.5,
            );
        if accepted {
            selection.supported.push(index);
            let c = 1.0 / (1.0 + sigma2[index] / (spec.tau * spec.tau));
            selection.covariance_weight[index] =
                probe.weight * spec.emphasis.group_multiplier(probe.group);
            selection.weight[index] = selection.covariance_weight[index] * c;
        }
    }
    selection
}

impl<'a> PreparedTarget<'a> {
    /// # Errors
    /// A covariance that does not cover the probe set, or a probe set with no
    /// supported probe.
    pub fn new(spec: TargetSpec<'a>) -> Result<Self> {
        Self::build(spec, None)
    }

    /// An experimental selection: when the published support mask supports no
    /// probe, the sigma and density predicates choose the probes instead.
    ///
    /// # Errors
    /// [`Self::new`]'s errors, including a probe set that neither the mask nor
    /// the predicates support.
    pub fn experimental(spec: TargetSpec<'a>, images: ImageCount) -> Result<Self> {
        Self::build(spec, Some(images))
    }

    fn build(spec: TargetSpec<'a>, fallback: Option<ImageCount>) -> Result<Self> {
        let count = spec.probes.probes.len();
        let mut sigma2 = vec![0.0_f64; count];
        let mut target_uncertainty = AssessmentStatus::Unassessed;
        if let Some(uncertainty) = spec.uncertainty {
            let covariance = match spec.appearance.metric() {
                ScoringMetric::RenderedPostClipV1 => uncertainty.probe_cov_post.as_ref(),
                ScoringMetric::LegacyPreClipV1 => uncertainty
                    .probe_cov_post
                    .as_ref()
                    .or(uncertainty.probe_cov.as_ref()),
            };
            if let Some(covariance) = covariance {
                if covariance.values.len() != count.saturating_mul(9) {
                    return Err(MatchError::Dimension(
                        "probe covariance does not cover the probe set".to_string(),
                    ));
                }
                target_uncertainty = AssessmentStatus::Assessed;
                for (index, chunk) in covariance.values.as_chunks::<9>().0.iter().enumerate() {
                    sigma2[index] = chunk[0] + chunk[4] + chunk[8];
                }
            }
        }
        let mask = spec
            .uncertainty
            .and_then(|data| data.support.as_ref())
            .filter(|mask| mask.values.len() == count)
            .map(|mask| mask.values.as_slice());
        let density_floor = spec.density_floor.unwrap_or(0.0);
        let fallback_floor = spec
            .density_floor
            .unwrap_or(looklab_fit::FitOptions::default().density_floor);
        let target: Vec<Ucs> = spec
            .probes
            .probes
            .iter()
            .map(|probe| spec.appearance.render(spec.function.predict(probe.xyz)))
            .collect();

        let mut support = TargetSupport::Published;
        let mut selection = select_probes(&spec, &sigma2, mask, density_floor);
        let mask_supports_nothing =
            mask.is_some_and(|values| values.iter().all(|value| *value <= 0.5));
        if mask_supports_nothing && let Some(images) = fallback {
            selection = select_probes(&spec, &sigma2, None, fallback_floor);
            support = TargetSupport::Experimental(images);
        }
        if selection.supported.is_empty() {
            return Err(MatchError::NoSupportedProbes);
        }

        Ok(Self {
            probes: spec.probes,
            function: spec.function,
            emphasis: spec.emphasis,
            tau: spec.tau,
            appearance: spec.appearance,
            declared_points: spec.probes.probes.len(),
            target_uncertainty,
            supported: selection.supported,
            sigma2,
            weight: selection.weight,
            covariance_weight: selection.covariance_weight,
            target,
            density_floor,
            support,
        })
    }

    pub const fn metric(&self) -> ScoringMetric {
        self.appearance.metric()
    }

    /// # Errors
    /// A count below the filtered probe set, which cannot describe its points.
    pub fn with_declared_points(mut self, count: usize) -> Result<Self> {
        if count < self.probes.probes.len() {
            return Err(MatchError::Dimension(format!(
                "declared {count} points for {} filtered probes",
                self.probes.probes.len()
            )));
        }
        self.declared_points = count;
        Ok(self)
    }

    pub const fn target_output(&self) -> Option<OutputPin> {
        self.appearance.pin()
    }

    pub fn predict(&self, xyz: Xyz) -> Ucs {
        self.appearance.render(self.function.predict(xyz))
    }

    pub fn output_context(
        &self,
        model: &MatchModel,
        settings: &looklab_model::realization::Settings,
    ) -> Result<OutputContext> {
        let target = self
            .target_output()
            .ok_or(MatchError::MissingOutputContext)?;
        let realization = model.realize(settings)?;
        Ok(OutputContext {
            target,
            camera: model.output_pin(&realization),
        })
    }

    pub fn optional_output_context(
        &self,
        model: &MatchModel,
        settings: &looklab_model::realization::Settings,
    ) -> Result<Option<OutputContext>> {
        match self.target_output() {
            Some(_) => self.output_context(model, settings).map(Some),
            None => Ok(None),
        }
    }

    pub fn support_fraction(&self) -> f64 {
        self.supported.len() as f64 / self.probes.probes.len() as f64
    }

    pub fn full_subset(&self) -> ProbeSubset {
        ProbeSubset {
            indices: self.supported.clone(),
            weights: self
                .supported
                .iter()
                .map(|index| self.weight[*index])
                .collect(),
            covariance_weights: self
                .supported
                .iter()
                .map(|index| self.covariance_weight[*index])
                .collect(),
        }
    }

    /// At most `count` supported probes, stratified by group and hue, with
    /// weights rescaled so each group keeps its total weight.
    pub fn subset(&self, count: usize) -> ProbeSubset {
        let count = count.max(1).min(self.supported.len());
        let groups = [
            ProbeGroup::GrayRamp,
            ProbeGroup::ChartPatches,
            ProbeGroup::HueGrid,
            ProbeGroup::Skin,
            ProbeGroup::MemoryColors,
        ];
        let mut members: Vec<Vec<usize>> = Vec::with_capacity(groups.len());
        for group in groups {
            let mut indices: Vec<usize> = self
                .supported
                .iter()
                .copied()
                .filter(|index| self.probes.probes[*index].group == group)
                .collect();
            indices.sort_by(|left, right| {
                let a = &self.probes.probes[*left];
                let b = &self.probes.probes[*right];
                let ia = LookInput::new(a.xyz);
                let ib = LookInput::new(b.xyz);
                hue_bin(ia.h_in)
                    .cmp(&hue_bin(ib.h_in))
                    .then_with(|| ia.s.total_cmp(&ib.s))
                    .then_with(|| ia.c_in.total_cmp(&ib.c_in))
                    .then_with(|| a.id.cmp(&b.id))
            });
            members.push(indices);
        }

        let total_weight: Vec<f64> = members
            .iter()
            .map(|indices| indices.iter().map(|index| self.weight[*index]).sum::<f64>())
            .collect();
        let total_covariance: Vec<f64> = members
            .iter()
            .map(|indices| {
                indices
                    .iter()
                    .map(|index| self.covariance_weight[*index])
                    .sum::<f64>()
            })
            .collect();
        let total: f64 = total_weight.iter().sum();
        let quota: Vec<f64> = if total > 0.0 {
            total_weight
                .iter()
                .map(|weight| count as f64 * weight / total)
                .collect()
        } else {
            members
                .iter()
                .map(|indices| count as f64 * indices.len() as f64 / self.supported.len() as f64)
                .collect()
        };
        let mut selected_counts: Vec<usize> = quota
            .iter()
            .zip(members.iter())
            .map(|(value, indices)| {
                if indices.is_empty() {
                    0
                } else {
                    (*value as usize).max(1).min(indices.len())
                }
            })
            .collect();
        let mut assigned: usize = selected_counts.iter().sum();
        while assigned < count {
            let mut best = 0;
            let mut best_remainder = f64::NEG_INFINITY;
            for (index, value) in quota.iter().enumerate() {
                if selected_counts[index] >= members[index].len() {
                    continue;
                }
                let remainder = *value - selected_counts[index] as f64;
                if remainder > best_remainder {
                    best_remainder = remainder;
                    best = index;
                }
            }
            selected_counts[best] += 1;
            assigned += 1;
        }
        while assigned > count {
            let mut best = usize::MAX;
            let mut best_excess = f64::NEG_INFINITY;
            for (index, value) in quota.iter().enumerate() {
                if selected_counts[index] <= 1 {
                    continue;
                }
                let excess = selected_counts[index] as f64 - *value;
                if excess > best_excess {
                    best_excess = excess;
                    best = index;
                }
            }
            if best == usize::MAX {
                break;
            }
            selected_counts[best] -= 1;
            assigned -= 1;
        }

        let mut indices = Vec::with_capacity(count);
        let mut weights = Vec::with_capacity(count);
        let mut covariance_weights = Vec::with_capacity(count);
        for (group, &take) in selected_counts.iter().enumerate() {
            let pool = &members[group];
            if take == 0 {
                continue;
            }
            let stride = pool.len() as f64 / take as f64;
            let mut chosen = Vec::with_capacity(take);
            for slot in 0..take {
                let position = ((slot as f64 + 0.5) * stride) as usize;
                chosen.push(pool[position.min(pool.len() - 1)]);
            }
            chosen.sort_unstable();
            chosen.dedup();
            let selected_weight: f64 = chosen.iter().map(|index| self.weight[*index]).sum();
            let selected_covariance: f64 = chosen
                .iter()
                .map(|index| self.covariance_weight[*index])
                .sum();
            let scale = if selected_weight > 0.0 {
                total_weight[group] / selected_weight
            } else {
                1.0
            };
            let covariance_scale = if selected_covariance > 0.0 {
                total_covariance[group] / selected_covariance
            } else {
                1.0
            };
            for index in chosen {
                indices.push(index);
                weights.push(self.weight[index] * scale);
                covariance_weights.push(self.covariance_weight[index] * covariance_scale);
            }
        }
        ProbeSubset {
            indices,
            weights,
            covariance_weights,
        }
    }
}

fn hue_bin(hue: f64) -> usize {
    use looklab_report::hue_bin_index;
    hue_bin_index(hue)
}
