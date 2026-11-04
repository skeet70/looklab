// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image-clustered sandwich uncertainty and probe support (docs/02 §5).

use looklab_model::basis::RESIDUAL_COEFS;
use looklab_model::{Look, LookInput};

use crate::layout::Params;
use crate::predict::{Jac, evaluate_probe};
use crate::problem::{BlockAcc, Problem, ResidualSample};
use crate::solve::{factor_in_place, solve_in_place};
use crate::types::{Density, FitOptions, ProbeEstimate, ProbeUncertainty, Result};

fn invert(h: &[f64], n: usize) -> Option<Vec<f64>> {
    if n == 0 {
        return Some(Vec::new());
    }
    let mut l = h.to_vec();
    if !factor_in_place(&mut l, n) {
        return None;
    }
    let mut inverse = vec![0.0_f64; n * n];
    let mut column = vec![0.0_f64; n];
    for j in 0..n {
        column.iter_mut().for_each(|value| *value = 0.0);
        column[j] = 1.0;
        solve_in_place(&l, n, &mut column);
        for (i, value) in column.iter().enumerate() {
            inverse[i * n + j] = *value;
        }
    }
    Some(inverse)
}

fn prior_diagonal(options: &FitOptions, problem: &Problem, params: &Params) -> Vec<f64> {
    let layout = &problem.layout;
    let mut diagonal = vec![0.0_f64; layout.total];
    let exposure = 1.0 / (options.exposure_prior_sd * options.exposure_prior_sd);
    let wb = 1.0 / (options.wb_prior_sd * options.wb_prior_sd);
    for index in 0..problem.images.len() {
        let range = layout.image(index);
        diagonal[range.start] = exposure;
        diagonal[range.start + 1] = wb;
        diagonal[range.start + 2] = wb;
    }
    let mode = 1.0 / (options.exposure_mode_prior_sd * options.exposure_mode_prior_sd);
    for index in 0..problem.modes.len() {
        diagonal[layout.mode_of(index)] = mode;
    }
    let slope = params.clip_slope();
    diagonal[layout.clip] = slope * slope / (options.clip_prior.1 * options.clip_prior.1);
    for index in layout.response.clone() {
        diagonal[index] = RESPONSE_PRIOR_PRECISION;
    }
    diagonal
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn probe_uncertainty(
    problem: &Problem,
    params: &Params,
    look: &Look,
    block: &BlockAcc,
    penalty: &[f64],
    hessian_scale: f64,
    lambdas: &crate::types::StageLambdas,
    per_image_gradient: &[Vec<f64>],
    density: &Density,
    residual_samples: &[ResidualSample],
    options: &FitOptions,
) -> Result<ProbeUncertainty> {
    let layout = &problem.layout;
    let priors = prior_diagonal(options, problem, params);
    let n = block.len();
    let mut h = block.hessian.clone();
    for value in &mut h {
        *value *= hessian_scale;
    }
    for entry in 0..n * n {
        h[entry] += penalty[entry];
    }
    let identity = crate::fit::identity_curvature(layout, block);
    for (local, global) in block.params.iter().enumerate() {
        h[local * n + local] += priors[*global] + identity[local];
    }
    let ridge = (0..n)
        .map(|index| h[index * n + index])
        .fold(0.0_f64, f64::max)
        .max(1.0)
        * 1e-6;
    for index in 0..n {
        h[index * n + index] += ridge;
    }
    let mut inverse = None;
    let mut bump = ridge;
    for _ in 0..14 {
        let mut attempt = h.clone();
        for index in 0..n {
            attempt[index * n + index] += bump;
        }
        if let Some(found) = invert(&attempt, n) {
            inverse = Some(found);
            break;
        }
        bump *= 10.0;
    }
    let Some(inverse) = inverse else {
        return Err(crate::types::FitError::SingularRegularization);
    };
    let residual_diagonal = residual_inverse_diagonal(residual_samples, lambdas.residual);
    let residual_start = layout.residual.start;

    let (_, beta_slope) = problem.look_and_slope(params);
    let probes = looklab_model::probes::v1();
    let mut jac = Jac::default();
    let mut estimates = Vec::with_capacity(probes.probes.len());
    let mut supported = 0usize;
    let mut z_dense = vec![0.0_f64; layout.total];
    let mut z_columns: [Vec<f64>; 3] = core::array::from_fn(|_| vec![0.0_f64; layout.total]);
    let mut entries: Vec<(usize, f64)> = Vec::with_capacity(320);

    for probe in &probes.probes {
        let pre = evaluate_probe(look, layout, probe.xyz, &beta_slope, &mut jac);
        for channel in 0..3 {
            z_dense.fill(0.0);
            entries.clear();
            for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
                let local = block.local[*column as usize];
                if local != u32::MAX {
                    entries.push((local as usize, derivative[channel]));
                }
            }
            for i in 0..n {
                let mut sum = 0.0;
                for (local, value) in &entries {
                    sum += inverse[i * n + local] * value;
                }
                z_dense[block.params[i]] = sum;
            }
            for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
                let index = *column as usize;
                if layout.residual.contains(&index) {
                    z_dense[index] =
                        derivative[channel] * residual_diagonal[index - residual_start];
                }
            }
            z_columns[channel].copy_from_slice(&z_dense);
        }
        // `z = H^-1 J_probe` is dense, so the contraction must cover every fitted coordinate.
        let mut support: Vec<usize> = block.params.clone();
        for index in residual_start..residual_start + RESIDUAL_COEFS {
            if z_columns.iter().any(|z| z[index] != 0.0) {
                support.push(index);
            }
        }
        support.sort_unstable();
        support.dedup();

        let mut covariance = [[0.0_f64; 3]; 3];
        for gradient in per_image_gradient {
            let mut scores = [0.0_f64; 3];
            for (channel, z) in z_columns.iter().enumerate() {
                let mut sum = 0.0;
                for index in &support {
                    sum += gradient[*index] * z[*index];
                }
                scores[channel] = sum;
            }
            for a in 0..3 {
                for b in a..3 {
                    covariance[a][b] += scores[a] * scores[b];
                }
            }
        }
        for a in 0..3 {
            for b in 0..a {
                covariance[a][b] = covariance[b][a];
            }
        }
        let sigma_de = (covariance[0][0] + covariance[1][1] + covariance[2][2])
            .max(0.0)
            .sqrt();
        if std::env::var_os("LOOKLAB_SIGMA_TRACE").is_some()
            && matches!(
                probes.probes[estimates.len()].id.as_str(),
                "chart-red-s+1" | "chart-yellow-green-s+2"
            )
        {
            let mut rows = Vec::new();
            for (image, gradient) in per_image_gradient.iter().enumerate() {
                let mut score = [0.0_f64; 3];
                for (channel, z) in z_columns.iter().enumerate() {
                    let mut sum = 0.0;
                    for index in &support {
                        sum += gradient[*index] * z[*index];
                    }
                    score[channel] = sum;
                }
                let norm = (score[0] * score[0] + score[1] * score[1] + score[2] * score[2]).sqrt();
                let gnorm = gradient.iter().map(|v| v * v).sum::<f64>().sqrt();
                rows.push((norm, gnorm, problem.images[image].contributor, image));
            }
            rows.sort_by(|a, b| b.0.total_cmp(&a.0));
            eprintln!(
                "{} base sigma {sigma_de:.3} z {:.4} top scores {:?}",
                probes.probes[estimates.len()].id.as_str(),
                z_columns[0].iter().map(|v| v.abs()).fold(0.0, f64::max),
                rows.iter()
                    .take(6)
                    .map(|(score, g, c, i)| format!("img{i} c{c} |s|{score:.3} |g|{g:.2}"))
                    .collect::<Vec<_>>()
            );
        }
        let input = LookInput::new(probe.xyz);
        let local_density = density.at(input.s, input.h_in, input.c_in);
        let is_supported = sigma_de <= options.support_sigma && local_density >= density.floor;
        let _ = &pre;
        if is_supported {
            supported += 1;
        }
        estimates.push(ProbeEstimate {
            id_hint: estimates.len(),
            ucs: pre,
            covariance,
            sigma_de,
            density: local_density,
            supported: is_supported,
        });
    }
    let ids = probes
        .probes
        .iter()
        .map(|probe| probe.id.clone())
        .collect::<Vec<_>>();
    let fraction = supported as f64 / estimates.len().max(1) as f64;
    Ok(ProbeUncertainty {
        probes: estimates,
        ids,
        supported_fraction: fraction,
    })
}

/// Image-clustered sandwich covariance of the control-response coefficients.
pub(crate) fn response_covariance(
    problem: &Problem,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &crate::types::StageLambdas,
    hessian_scale: f64,
    per_image_gradient: &[Vec<f64>],
) -> (Vec<looklab_model::realization::ResponseParamId>, Vec<f64>) {
    let n = problem.layout.response.len();
    let mut covariance = vec![0.0_f64; n * n];
    for (block, penalty) in blocks.iter().zip(penalties.iter()) {
        let Some(lambda) = response_lambda(lambdas, block.kind) else {
            continue;
        };
        let m = block.len();
        if m == 0 {
            continue;
        }
        let mut h = block.hessian.clone();
        for value in &mut h {
            *value *= hessian_scale;
        }
        for entry in 0..m * m {
            h[entry] += lambda * penalty[entry];
        }
        for index in 0..m {
            h[index * m + index] += 1e-9;
        }
        let Some(inverse) = invert(&h, m) else {
            continue;
        };
        let mut scores = vec![vec![0.0_f64; m]; per_image_gradient.len()];
        for (image, gradient) in per_image_gradient.iter().enumerate() {
            for (local, global) in block.params.iter().enumerate() {
                scores[image][local] = gradient[*global];
            }
        }
        let mut middle = vec![0.0_f64; m * m];
        for score in &scores {
            for a in 0..m {
                for b in 0..m {
                    middle[a * m + b] += score[a] * score[b];
                }
            }
        }
        let mut left = vec![0.0_f64; m * m];
        for a in 0..m {
            for i in 0..m {
                let value = inverse[a * m + i];
                if value == 0.0 {
                    continue;
                }
                for j in 0..m {
                    left[a * m + j] += value * middle[i * m + j];
                }
            }
        }
        for a in 0..m {
            for j in 0..m {
                let global_a = block.params[a] - problem.layout.response.start;
                for b in 0..m {
                    let global_b = block.params[b] - problem.layout.response.start;
                    covariance[global_a * n + global_b] += left[a * m + j] * inverse[j * m + b];
                }
            }
        }
    }
    for index in 0..n {
        covariance[index * n + index] += 1.0 / RESPONSE_PRIOR_PRECISION;
    }
    (problem.parameter_ids.clone(), covariance)
}

const RESPONSE_PRIOR_PRECISION: f64 = 10.0;

fn response_lambda(
    lambdas: &crate::types::StageLambdas,
    kind: crate::layout::BlockKind,
) -> Option<f64> {
    use crate::layout::BlockKind;
    match kind {
        BlockKind::ResponseTone => Some(lambdas.response_tone),
        BlockKind::ResponseTint => Some(lambdas.response_tint),
        BlockKind::ResponseChroma => Some(lambdas.response_chroma),
        BlockKind::ResponseHue => Some(lambdas.response_hue),
        BlockKind::ResponseLightness => Some(lambdas.response_lightness),
        BlockKind::ResponseWb => Some(lambdas.response_wb),
        _ => None,
    }
}

fn residual_inverse_diagonal(samples: &[ResidualSample], lambda: f64) -> Vec<f64> {
    let mut diagonal = vec![0.0_f64; RESIDUAL_COEFS];
    for sample in samples {
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                let mut sum = 0.0;
                for row in 0..3 {
                    sum += sample.clip.0[row][channel] * sample.clip.0[row][channel];
                }
                diagonal[*node as usize * 3 + channel] += sample.alpha * weight * weight * sum;
            }
        }
    }
    let identity = 2.0 * crate::fit::identity_weight(crate::layout::BlockKind::Residual);
    for value in &mut diagonal {
        *value += identity;
    }
    let peak = diagonal.iter().copied().fold(0.0_f64, f64::max);
    for value in &mut diagonal {
        // Nodes without data stay pinned at zero, so their posterior is degenerate.
        *value = if *value > 1e-6 * peak && *value + lambda > 0.0 {
            1.0 / (*value + lambda)
        } else {
            0.0
        };
    }
    diagonal
}
