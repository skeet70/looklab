// SPDX-License-Identifier: AGPL-3.0-or-later
//! The software model of the declared pipeline, and the validation report that
//! gates an export (docs/04 §7).

use looklab_color::ucs::delta_e_ucs;
use looklab_model::{Look, ProbeSet};

use crate::bake::{BakeIlluminant, BakeOutput, baked_look};
use crate::camera::InputTransform;
use crate::cct::CctK;
use crate::downstream::checked_appearance;
use crate::error::{ExportError, Result};

/// ΔE summary over the supported probes; percentiles are nearest-rank.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeltaStats {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl DeltaStats {
    pub fn from_values(mut values: Vec<f64>) -> Self {
        if values.is_empty() {
            return Self {
                count: 0,
                median: 0.0,
                p95: 0.0,
                max: 0.0,
            };
        }
        values.sort_by(f64::total_cmp);
        let rank = |fraction: f64| {
            let index = ((fraction * values.len() as f64).ceil() as usize)
                .saturating_sub(1)
                .min(values.len() - 1);
            values[index]
        };
        Self {
            count: values.len(),
            median: rank(0.5),
            p95: rank(0.95),
            max: values[values.len() - 1],
        }
    }
}

/// ΔE at one CCT, over the probes with a reproducible target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CctReport {
    pub cct: CctK,
    pub stats: DeltaStats,
    /// Probes the insertion encoding cannot represent at this CCT, or masked out.
    pub unsupported: usize,
    /// Probes whose target needed perceptual clipping.
    pub clipped: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationReport {
    pub bake: BakeIlluminant,
    pub primary: CctReport,
    pub others: Vec<CctReport>,
    pub probes: usize,
    /// Probes whose target appearance needed perceptual clipping.
    pub target_clipped: usize,
    /// Only for `display_encoded_post_tone`: ΔE between the full look and the
    /// colour-only look the bake can realize.
    pub tone_mismatch: Option<DeltaStats>,
}

impl ValidationReport {
    pub fn at(&self, cct: CctK) -> Option<&CctReport> {
        if self.primary.cct == cct {
            return Some(&self.primary);
        }
        self.others.iter().find(|report| report.cct == cct)
    }

    /// p95 above 1.0 at a bake CCT flags the export; a dual bake has two.
    pub fn flagged(&self) -> bool {
        match self.bake {
            BakeIlluminant::Single { cct } => {
                self.at(cct).is_some_and(|report| report.stats.p95 > 1.0)
            }
            BakeIlluminant::DualInterpolated => [CctK::STD_A, CctK::D65]
                .iter()
                .filter_map(|cct| self.at(*cct))
                .any(|report| report.stats.p95 > 1.0),
        }
    }

    pub fn clipped_fraction(&self) -> f64 {
        if self.probes == 0 {
            0.0
        } else {
            self.target_clipped as f64 / self.probes as f64
        }
    }

    pub fn unsupported_fraction(&self) -> f64 {
        if self.probes == 0 {
            0.0
        } else {
            self.primary.unsupported as f64 / self.probes as f64
        }
    }
}

/// Per-probe support mask; `None` marks every representable probe supported.
pub type Support<'a> = Option<&'a [bool]>;

/// # Errors
/// [`ExportError::ProbeMask`] when the mask length differs from the probe count,
/// [`ExportError::TransformUndefined`] when the body transform is undefined at a
/// reported CCT.
pub fn validate(
    output: &BakeOutput,
    look: &Look,
    transform: &dyn InputTransform,
    probes: &ProbeSet,
    support: Support<'_>,
) -> Result<ValidationReport> {
    if let Some(mask) = support
        && mask.len() != probes.probes.len()
    {
        return Err(ExportError::ProbeMask {
            found: mask.len(),
            expected: probes.probes.len(),
        });
    }
    let look_used = baked_look(look, output.insertion.point);
    let mut feasible = Vec::with_capacity(probes.probes.len());
    let targets: Vec<_> = probes
        .probes
        .iter()
        .map(|probe| {
            feasible.push(checked_appearance(&look_used, probe.xyz).is_ok());
            let target = output.downstream.target(&look_used, probe.xyz);
            (target, output.downstream.appearance(target.codes))
        })
        .collect();
    let target_clipped = targets
        .iter()
        .zip(&feasible)
        .filter(|((target, _), feasible)| **feasible && target.clipped)
        .count();

    let tone_mismatch = (output.insertion.point
        == looklab_schema::InsertionPoint::DisplayEncodedPostTone
        && !look.tone.is_colorimetric())
    .then(|| {
        let mut values = Vec::with_capacity(probes.probes.len());
        for (index, (probe, (_, partial))) in probes.probes.iter().zip(targets.iter()).enumerate() {
            if !feasible[index] || checked_appearance(look, probe.xyz).is_err() {
                continue;
            }
            let full = output.downstream.target(look, probe.xyz);
            let full = output.downstream.appearance(full.codes);
            values.push(delta_e_ucs(full, *partial));
        }
        DeltaStats::from_values(values)
    });

    let primary_cct = output.bake.primary();
    let mut primary = None;
    let mut others = Vec::new();
    for cct in output.bake.report_ccts() {
        let report = evaluate_cct(output, probes, support, &feasible, &targets, transform, cct)?;
        if cct == primary_cct && primary.is_none() {
            primary = Some(report);
        } else {
            others.push(report);
        }
    }
    let primary = primary.ok_or(ExportError::Profile(
        "the bake CCT is missing from the report set",
    ))?;
    Ok(ValidationReport {
        bake: output.bake,
        primary,
        others,
        probes: probes.probes.len(),
        target_clipped,
        tone_mismatch,
    })
}

fn evaluate_cct(
    output: &BakeOutput,
    probes: &ProbeSet,
    support: Support<'_>,
    feasible: &[bool],
    targets: &[(crate::downstream::TargetOutput, looklab_color::ucs::Ucs)],
    transform: &dyn InputTransform,
    cct: CctK,
) -> Result<CctReport> {
    let interpolation = output
        .interpolation
        .unwrap_or(looklab_schema::Interpolation::Trilinear);
    let camera = if output.insertion.needs_camera() {
        Some(
            transform
                .camera_to_xyz(cct)
                .ok_or(ExportError::TransformUndefined { cct: cct.kelvin() })?,
        )
    } else {
        None
    };
    let primary = output
        .artifact(cct)
        .or_else(|| output.primary_artifact())
        .ok_or(ExportError::NoAcceptedArtifact)?;
    let dual = match output.bake {
        BakeIlluminant::DualInterpolated => Some((
            output
                .artifact(CctK::STD_A)
                .ok_or(ExportError::NoAcceptedArtifact)?,
            output
                .artifact(CctK::D65)
                .ok_or(ExportError::NoAcceptedArtifact)?,
        )),
        BakeIlluminant::Single { .. } => None,
    };

    let mut values = Vec::with_capacity(probes.probes.len());
    let mut unsupported = 0;
    let mut clipped = 0;
    for (index, probe) in probes.probes.iter().enumerate() {
        let linear = output.insertion.from_xyz(probe.xyz, camera)?;
        let codes = output.insertion.encode(linear);
        let representable = codes
            .iter()
            .all(|value| value.is_finite() && (-1.0e-9..=1.0 + 1.0e-9).contains(value));
        let allowed = support.is_none_or(|mask| mask[index]);
        if !representable || !allowed || !feasible[index] {
            unsupported += 1;
            continue;
        }
        if targets[index].0.clipped {
            clipped += 1;
        }
        let code = codes.map(|value| value.clamp(0.0, 1.0));
        let linear = output.insertion.decode(code);
        let value = match dual {
            None => primary.eval(code, linear, interpolation),
            Some((std_a, d65)) => {
                let weight = cct.dual_weight();
                let cold = std_a.eval(code, linear, interpolation);
                let warm = d65.eval(code, linear, interpolation);
                [0, 1, 2].map(|channel| cold[channel] * (1.0 - weight) + warm[channel] * weight)
            }
        };
        let encoded = output.downstream.encode(value);
        let encoded = output.downstream.clip(encoded);
        let appearance = output.downstream.appearance(encoded);
        values.push(delta_e_ucs(targets[index].1, appearance));
    }
    Ok(CctReport {
        cct,
        stats: DeltaStats::from_values(values),
        unsupported,
        clipped,
    })
}
