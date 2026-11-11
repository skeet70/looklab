// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image-clustered sandwich uncertainty and probe support (docs/02 §5).

use looklab_model::basis::RESIDUAL_COEFS;
use looklab_model::{Look, LookInput};

use crate::layout::Params;
use crate::predict::{Jac, evaluate_probe};
use crate::problem::{BlockAcc, Problem, ResidualSample};
use crate::solve::{factor_in_place, solve_four_in_place, solve_in_place};
use crate::types::{
    Density, FitError, FitOptions, PreviousSupport, ProbeEstimate, ProbeUncertainty, Result,
};

/// Variance stored for a probe whose covariance is a finite fallback; support stays off.
const UNKNOWN_VARIANCE: f64 = 1.0e30;

/// Keep post-clip variance at least `(0.25)²` times the pre-clip trace, the clip
/// derivative below which the pre-clip value is not identifiable.
pub(crate) fn floor_post_covariance(pre: &[[f64; 3]; 3], post: &mut [[f64; 3]; 3]) {
    let pre_trace = pre[0][0] + pre[1][1] + pre[2][2];
    let post_trace = post[0][0] + post[1][1] + post[2][2];
    let floor = 0.0625 * pre_trace;
    if post_trace < floor {
        let extra = (floor - post_trace) / 3.0;
        for (channel, row) in post.iter_mut().enumerate() {
            row[channel] += extra;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SupportFlags {
    pub sigma: bool,
    pub density: bool,
    pub clusters: bool,
    pub bias: bool,
}

impl SupportFlags {
    pub fn all(self) -> bool {
        self.sigma && self.density && self.clusters && self.bias
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn mark_support(
    sigma: f64,
    bias: f64,
    density: f64,
    clusters: usize,
    floor: f64,
    previous_sigma: bool,
    previous_bias: bool,
    options: &FitOptions,
) -> SupportFlags {
    let sigma_limit = if previous_sigma {
        options.support_sigma_exit
    } else {
        options.support_sigma
    };
    let bias_enter = options.bias_fraction * options.support_sigma;
    let bias_limit = if previous_bias {
        1.1 * bias_enter
    } else {
        bias_enter
    };
    SupportFlags {
        sigma: sigma <= sigma_limit,
        density: density >= floor,
        clusters: clusters >= options.min_probe_clusters,
        bias: bias <= bias_limit,
    }
}

const CLUSTER_S: f64 = 0.75;
const CLUSTER_HUE_DEG: f64 = 20.0;
const CLUSTER_C: f64 = 12.0;

#[derive(Clone, Copy)]
struct ClusterSample {
    image: u32,
    s: f64,
    h: f64,
    c: f64,
    weight: f64,
}

fn cluster_samples(problem: &Problem, params: &Params) -> Vec<ClusterSample> {
    problem
        .samples
        .iter()
        .map(|sample| {
            let image = sample.image as usize;
            let context = problem.image_ctx(params, image);
            let exposure = context.exposure_scale;
            let xyz = looklab_color::Xyz::from_array(context.transform.mul_vec([
                sample.rgb[0] * context.wb_r * exposure,
                sample.rgb[1] * exposure,
                sample.rgb[2] * context.wb_b * exposure,
            ]));
            let input = LookInput::new(xyz);
            ClusterSample {
                image: sample.image,
                s: input.s,
                h: input.h_in,
                c: input.c_in,
                weight: sample.weight,
            }
        })
        .collect()
}

fn probe_cluster_count(
    samples: &[ClusterSample],
    xyz: looklab_color::Xyz,
    images: usize,
    token: usize,
) -> usize {
    let input = LookInput::new(xyz);
    let mut last = vec![usize::MAX; images];
    let mut count = 0usize;
    for sample in samples {
        if sample.weight <= 0.0 || (sample.s - input.s).abs() > CLUSTER_S {
            continue;
        }
        let mut delta = (sample.h - input.h_in).abs() % 360.0;
        if delta > 180.0 {
            delta = 360.0 - delta;
        }
        if delta > CLUSTER_HUE_DEG || (sample.c - input.c_in).abs() > CLUSTER_C {
            continue;
        }
        let image = sample.image as usize;
        if last[image] != token {
            last[image] = token;
            count += 1;
        }
    }
    count
}

/// Small-cluster scaling of the sandwich covariance (`G/(G−1)` times the
/// `t_{0.95,G−1}/z` band factor squared; 1 when `G < 2`).
pub fn small_cluster_factor(clusters: usize) -> f64 {
    if clusters < 2 {
        return 1.0;
    }
    let g = clusters as f64;
    let t = student_t_95(g - 1.0);
    g / (g - 1.0) * t * t / (1.644_853_626_951_472_2 * 1.644_853_626_951_472_2)
}

/// Exact for 1-5 dof; Cornish-Fisher above (Abramowitz & Stegun 26.7.5).
pub(crate) fn student_t_95(dof: f64) -> f64 {
    let degrees = dof.round() as u32;
    if (1..=5).contains(&degrees) {
        return [
            0.0,
            12.706_204_6,
            4.302_652_7,
            3.182_446_0,
            2.776_445_1,
            2.570_581_9,
        ][degrees as usize];
    }
    let z = 1.644_853_626_951_472_2;
    let v = dof.max(1.0);
    let z3 = z * z * z;
    let z5 = z3 * z * z;
    let z7 = z5 * z * z;
    z + (z3 + z) / (4.0 * v)
        + (5.0 * z5 + 16.0 * z3 + 3.0 * z) / (96.0 * v * v)
        + (3.0 * z7 + 19.0 * z5 + 17.0 * z3 - 15.0 * z) / (384.0 * v * v * v)
}

fn invert(h: &[f64], n: usize) -> Option<Vec<f64>> {
    if n == 0 {
        return Some(Vec::new());
    }
    let mut l = h.to_vec();
    if !factor_in_place(&mut l, n) {
        return None;
    }
    let mut inverse = vec![0.0_f64; n * n];
    let full = n - n % 4;
    let mut columns = vec![0.0_f64; if full == 0 { n } else { 4 * n }];
    for j in (0..full).step_by(4) {
        columns.fill(0.0);
        for offset in 0..4 {
            columns[offset * n + j + offset] = 1.0;
        }
        let (a, rest) = columns.split_at_mut(n);
        let (b, rest) = rest.split_at_mut(n);
        let (c, d) = rest.split_at_mut(n);
        solve_four_in_place(&l, [a, b, c, d]);
        for i in 0..n {
            for offset in 0..4 {
                inverse[i * n + j + offset] = columns[offset * n + i];
            }
        }
    }
    let column = &mut columns[..n];
    for j in full..n {
        column.fill(0.0);
        column[j] = 1.0;
        solve_in_place(&l, n, column);
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

/// Spans can contain zero-valued interior columns.
struct PenaltySpans {
    /// `(first, end)` per row; `first == end` for an all-zero row.
    rows: Vec<(u32, u32)>,
}

impl PenaltySpans {
    fn new(matrix: &[f64], n: usize) -> Self {
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            let row = &matrix[i * n..(i + 1) * n];
            let first = row.iter().position(|value| *value != 0.0);
            let last = row.iter().rposition(|value| *value != 0.0);
            rows.push(match (first, last) {
                (Some(first), Some(last)) => (first as u32, last as u32 + 1),
                _ => (0, 0),
            });
        }
        Self { rows }
    }
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
    previous: Option<&PreviousSupport>,
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
    let identity = crate::fit::identity_matrix(layout, block);
    for entry in 0..n * n {
        h[entry] += identity[entry];
    }
    for (local, global) in block.params.iter().enumerate() {
        h[local * n + local] += priors[*global];
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
    let inverse = inverse.unwrap_or_else(|| {
        // A zero-curvature direction has no data or prior information; its diagonal
        // inverse keeps the probe covariance finite.
        let mut fallback = vec![0.0_f64; n * n];
        for index in 0..n {
            fallback[index * n + index] = 1.0 / h[index * n + index].max(ridge);
        }
        fallback
    });
    let residual_diagonal =
        residual_inverse_diagonal(residual_samples, lambdas.residual, hessian_scale);
    let residual_start = layout.residual.start;
    let residual_prior: Vec<f64> = crate::fit::residual_penalty_diagonal()
        .into_iter()
        .map(|degree| {
            crate::fit::identity_weight(crate::layout::BlockKind::Residual)
                + 2.0 * lambdas.residual * degree
        })
        .collect();
    let clip_local = block
        .params
        .iter()
        .position(|global| *global == layout.clip);
    let z_clip = clip_local.map(|local| {
        let mut column = vec![0.0_f64; layout.total];
        for i in 0..n {
            column[block.params[i]] = inverse[i * n + local];
        }
        column
    });
    let add_quadratic_priors = |out: &mut [f64], z: &[f64]| {
        for i in 0..n {
            let global = block.params[i];
            out[i] += priors[global] * z[global];
            for j in 0..n {
                out[i] += penalty[i * n + j] * z[block.params[j]];
            }
        }
    };
    let apply_prior = |z: &[f64]| -> Vec<f64> {
        let mut out = crate::fit::identity_apply(layout, block, z);
        add_quadratic_priors(&mut out, z);
        out
    };
    let apply_prior_gradient = |z: &[f64]| -> Vec<f64> {
        let mut out = crate::fit::identity_gradient(layout, block, z);
        add_quadratic_priors(&mut out, z);
        out
    };
    let penalty_spans = options.prior_floor.then(|| PenaltySpans::new(penalty, n));
    let apply_prior_many = |spans: &PenaltySpans, z: [&[f64]; 3]| -> [Vec<f64>; 3] {
        let mut out: [Vec<f64>; 3] =
            core::array::from_fn(|channel| crate::fit::identity_apply(layout, block, z[channel]));
        for i in 0..n {
            let global = block.params[i];
            for channel in 0..3 {
                out[channel][i] += priors[global] * z[channel][global];
            }
            let (first, end) = spans.rows[i];
            for j in first as usize..end as usize {
                let weight = penalty[i * n + j];
                if weight != 0.0 {
                    let source = block.params[j];
                    for channel in 0..3 {
                        out[channel][i] += weight * z[channel][source];
                    }
                }
            }
        }
        out
    };
    let applied_theta = apply_prior_gradient(&params.theta);
    let applied_k = z_clip.as_ref().map(|column| apply_prior(column));

    let (_, beta_slope) = problem.look_and_slope(params);
    let space = problem
        .images
        .first()
        .map_or(looklab_color::RgbSpace::Srgb, |image| image.space);
    let probes = looklab_model::probes::v1();
    let cluster_samples = cluster_samples(problem, params);
    let mut jac = Jac::default();
    let mut estimates = Vec::with_capacity(probes.probes.len());
    let mut supported = 0usize;
    let mut z_columns: [Vec<f64>; 3] = core::array::from_fn(|_| vec![0.0_f64; layout.total]);
    let mut entries: Vec<(usize, usize)> = Vec::with_capacity(320);
    let mut residual_written: Vec<usize> = Vec::new();
    let mut support: Vec<usize> = Vec::with_capacity(n);

    for probe in &probes.probes {
        let pre = evaluate_probe(look, layout, probe.xyz, &beta_slope, &mut jac);
        let mut masked = [false; 3];
        let degraded = if jac
            .derivs
            .iter()
            .all(|deriv| deriv.iter().all(|value| value.is_finite()))
        {
            false
        } else {
            for deriv in &mut jac.derivs {
                for (channel, value) in deriv.iter_mut().enumerate() {
                    if !value.is_finite() {
                        *value = 0.0;
                        masked[channel] = true;
                    }
                }
            }
            true
        };
        for index in residual_written.drain(..) {
            for z in z_columns.iter_mut() {
                z[index] = 0.0;
            }
        }
        entries.clear();
        for (deriv, column) in jac.cols.iter().enumerate() {
            let local = block.local[*column as usize];
            if local != u32::MAX {
                entries.push((local as usize, deriv));
            }
        }
        for i in 0..n {
            let row = &inverse[i * n..i * n + n];
            let mut sums = [0.0_f64; 3];
            for &(local, deriv) in &entries {
                let inverse_value = row[local];
                let derivative = jac.derivs[deriv];
                for channel in 0..3 {
                    sums[channel] += inverse_value * derivative[channel];
                }
            }
            let global = block.params[i];
            for channel in 0..3 {
                z_columns[channel][global] = sums[channel];
            }
        }
        for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
            let index = *column as usize;
            if layout.residual.contains(&index) {
                let scale = residual_diagonal[index - residual_start];
                for channel in 0..3 {
                    z_columns[channel][index] = derivative[channel] * scale;
                }
                residual_written.push(index);
            }
        }
        // `z = H^-1 J_probe` is dense, so the contraction must cover every fitted coordinate.
        support.clear();
        support.extend_from_slice(&block.params);
        for &index in &residual_written {
            if z_columns.iter().any(|z| z[index] != 0.0) {
                support.push(index);
            }
        }
        support.sort_unstable();
        support.dedup();

        let mut meat = [[0.0_f64; 3]; 3];
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
                for b in 0..3 {
                    meat[a][b] += scores[a] * scores[b];
                }
            }
        }
        let factor = if options.sandwich_correction {
            small_cluster_factor(problem.active_images)
        } else {
            1.0
        };

        // `H^-1 (factor * M + L_prior) H^-1`: the meat uses z, the prior term is z' L z.
        let mut prior_cov = [[0.0_f64; 3]; 3];
        if let Some(spans) = &penalty_spans {
            let applied = apply_prior_many(spans, [&z_columns[0], &z_columns[1], &z_columns[2]]);
            for c in 0..3 {
                for d in 0..3 {
                    for (local, global) in block.params.iter().enumerate() {
                        prior_cov[c][d] += z_columns[c][*global] * applied[d][local];
                    }
                    for (offset, weight) in residual_prior.iter().enumerate() {
                        let global = residual_start + offset;
                        prior_cov[c][d] += weight * z_columns[c][global] * z_columns[d][global];
                    }
                }
            }
        }
        let mut covariance = [[0.0_f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                covariance[a][b] = factor * meat[a][b] + prior_cov[a][b];
            }
        }
        let mut sigma_de = (covariance[0][0] + covariance[1][1] + covariance[2][2])
            .max(0.0)
            .sqrt();
        let mut bias_pre = [0.0_f64; 3];
        for (local, global) in block.params.iter().enumerate() {
            let value = applied_theta[local];
            if value != 0.0 {
                for channel in 0..3 {
                    bias_pre[channel] += z_columns[channel][*global] * value;
                }
            }
        }
        for (offset, weight) in residual_prior.iter().enumerate() {
            let global = residual_start + offset;
            let value = weight * params.theta[global];
            for channel in 0..3 {
                bias_pre[channel] += z_columns[channel][global] * value;
            }
        }
        let mut kk = 0.0_f64;
        let mut cross = [0.0_f64; 3];
        let mut k_bias = 0.0_f64;
        if let Some(column) = &z_clip {
            let mut ks = vec![0.0_f64; per_image_gradient.len()];
            for (image, gradient) in per_image_gradient.iter().enumerate() {
                ks[image] = support
                    .iter()
                    .map(|index| gradient[*index] * column[*index])
                    .sum();
            }
            kk += factor * ks.iter().map(|value| value * value).sum::<f64>();
            for (image, gradient) in per_image_gradient.iter().enumerate() {
                for (channel, z) in z_columns.iter().enumerate() {
                    let score: f64 = support
                        .iter()
                        .map(|index| gradient[*index] * z[*index])
                        .sum();
                    cross[channel] += factor * score * ks[image];
                }
            }
            for (local, global) in block.params.iter().enumerate() {
                k_bias += column[*global] * applied_theta[local];
            }
            for (offset, weight) in residual_prior.iter().enumerate() {
                let global = residual_start + offset;
                k_bias += column[global] * weight * params.theta[global];
            }
            if options.prior_floor {
                let applied_k = applied_k.as_ref().expect("clip column present");
                for (local, global) in block.params.iter().enumerate() {
                    kk += column[*global] * applied_k[local];
                    for channel in 0..3 {
                        cross[channel] += z_columns[channel][*global] * applied_k[local];
                    }
                }
                for (offset, weight) in residual_prior.iter().enumerate() {
                    let global = residual_start + offset;
                    kk += weight * column[global] * column[global];
                    for channel in 0..3 {
                        cross[channel] += weight * z_columns[channel][global] * column[global];
                    }
                }
            }
        }
        let clip_slope = params.clip_slope();
        let chain = crate::predict::clip_jacobian(pre, space, params.clip_k());
        let dk = crate::predict::clip_k_gradient(pre, space, params.clip_k());
        let mut post_covariance = [[0.0_f64; 3]; 3];
        let mut bias_post = [0.0_f64; 3];
        for c in 0..3 {
            for d in 0..3 {
                let mut sum = 0.0;
                for a in 0..3 {
                    for b in 0..3 {
                        sum += chain.0[c][a] * covariance[a][b] * chain.0[d][b];
                    }
                }
                let left: f64 = (0..3).map(|a| chain.0[c][a] * cross[a]).sum();
                let right: f64 = (0..3).map(|b| cross[b] * chain.0[d][b]).sum();
                sum += clip_slope * (left * dk[d] + dk[c] * right)
                    + clip_slope * clip_slope * dk[c] * dk[d] * kk;
                post_covariance[c][d] = sum;
            }
            let mut sum = 0.0;
            for a in 0..3 {
                sum += chain.0[c][a] * bias_pre[a];
            }
            bias_post[c] = sum + clip_slope * dk[c] * k_bias;
        }
        floor_post_covariance(&covariance, &mut post_covariance);
        if degraded {
            // A masked channel has no variance estimate; the unknown-variance convention
            // (docs/02 §5) keeps mask-ignoring consumers from reading zero.
            for channel in 0..3 {
                if masked[channel] {
                    covariance[channel][channel] = UNKNOWN_VARIANCE;
                    post_covariance[channel][channel] = UNKNOWN_VARIANCE;
                }
            }
            sigma_de = (covariance[0][0] + covariance[1][1] + covariance[2][2])
                .max(0.0)
                .sqrt();
        }
        let mut post_sigma_de =
            (post_covariance[0][0] + post_covariance[1][1] + post_covariance[2][2])
                .max(0.0)
                .sqrt();
        let mut bias_de = (bias_post[0] * bias_post[0]
            + bias_post[1] * bias_post[1]
            + bias_post[2] * bias_post[2])
            .max(0.0)
            .sqrt();
        let clip_reliable =
            crate::predict::clip_derivative_min(pre, space, params.clip_k()) >= 0.25;
        let mut clip_fallback = false;
        if !post_covariance
            .iter()
            .flatten()
            .all(|value| value.is_finite())
            || !post_sigma_de.is_finite()
            || !bias_post.iter().all(|value| value.is_finite())
            || !bias_de.is_finite()
        {
            post_covariance = covariance;
            post_sigma_de = sigma_de;
            bias_de =
                (bias_pre[0] * bias_pre[0] + bias_pre[1] * bias_pre[1] + bias_pre[2] * bias_pre[2])
                    .max(0.0)
                    .sqrt();
            clip_fallback = true;
        }
        let post_ucs = crate::predict::post_clip(look, probe.xyz, space, params.clip_k());
        let post_ucs = if post_ucs.to_array().iter().all(|value| value.is_finite()) {
            post_ucs
        } else {
            looklab_model::output_encode_clip(pre, space, params.clip_k())
        };
        let clusters = probe_cluster_count(
            &cluster_samples,
            probe.xyz,
            problem.images.len(),
            estimates.len(),
        );
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
        let previous_sigma = previous
            .and_then(|state| state.sigma.get(estimates.len()).copied())
            .unwrap_or(false);
        let previous_bias = previous
            .and_then(|state| state.bias.get(estimates.len()).copied())
            .unwrap_or(false);
        let mut flags = mark_support(
            post_sigma_de,
            bias_de,
            local_density,
            clusters,
            density.floor,
            previous_sigma,
            previous_bias,
            options,
        );
        let withheld = degraded || clip_fallback;
        if withheld {
            flags.sigma = false;
        }
        if !pre.to_array().iter().all(|value| value.is_finite())
            || !covariance.iter().flatten().all(|value| value.is_finite())
            || !post_covariance
                .iter()
                .flatten()
                .all(|value| value.is_finite())
            || !sigma_de.is_finite()
            || !post_sigma_de.is_finite()
            || !bias_de.is_finite()
        {
            return Err(FitError::NonFiniteProbeEstimate {
                probe: probes.probes[estimates.len()].id.clone(),
            });
        }
        if flags.all() {
            supported += 1;
        }
        estimates.push(ProbeEstimate {
            id_hint: estimates.len(),
            ucs: pre,
            covariance,
            sigma_de,
            post_ucs,
            post_covariance,
            post_sigma_de,
            bias_de,
            pre_clip_reliable: !withheld && clip_reliable,
            gamut_edge: probe.id.as_str().split('-').nth(2) == Some("80"),
            density: local_density,
            supported_sigma: flags.sigma,
            supported_density: flags.density,
            supported_clusters: flags.clusters,
            supported_bias: flags.bias,
            supported_evidence: false,
            supported: flags.all(),
            support_withheld: withheld,
            clusters,
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

/// Clears support on probes whose covariance is a finite fallback.
pub(crate) fn unsupport_unreliable(probes: &mut ProbeUncertainty) {
    let mut supported = 0usize;
    for probe in &mut probes.probes {
        if probe.support_withheld {
            probe.supported_sigma = false;
            probe.supported = false;
            probe.pre_clip_reliable = false;
        }
        if probe.supported {
            supported += 1;
        }
    }
    probes.supported_fraction = supported as f64 / probes.probes.len().max(1) as f64;
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

fn residual_inverse_diagonal(
    samples: &[ResidualSample],
    lambda: f64,
    hessian_scale: f64,
) -> Vec<f64> {
    let mut diagonal = vec![0.0_f64; RESIDUAL_COEFS];
    for sample in samples {
        for (m, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[m];
            for channel in 0..3 {
                let mut sum = 0.0;
                for row in 0..3 {
                    sum += sample.clip.0[row][channel] * sample.clip.0[row][channel];
                }
                diagonal[*node as usize * 3 + channel] +=
                    sample.alpha * weight * weight * sum * hessian_scale;
            }
        }
    }
    let identity = crate::fit::identity_weight(crate::layout::BlockKind::Residual);
    let penalty = crate::fit::residual_penalty_diagonal();
    for (index, value) in diagonal.iter_mut().enumerate() {
        *value += identity + 2.0 * lambda * penalty[index];
    }
    let peak = diagonal.iter().copied().fold(0.0_f64, f64::max);
    for value in &mut diagonal {
        // Nodes without data stay pinned at zero, so their posterior is degenerate.
        *value = if *value > 1e-6 * peak {
            1.0 / *value
        } else {
            0.0
        };
    }
    diagonal
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn inverse_recovers_identity_across_batch_boundaries() {
        assert_eq!(invert(&[], 0), Some(Vec::new()));
        assert_eq!(invert(&[0.0], 1), None);
        for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 32, 33] {
            let mut matrix = vec![0.0; n * n];
            for i in 0..n {
                matrix[i * n + i] = 4.0;
                if i + 1 < n {
                    matrix[i * n + i + 1] = 0.5;
                    matrix[(i + 1) * n + i] = 0.5;
                }
            }
            let inverse = invert(&matrix, n).expect("positive-definite matrix");
            for i in 0..n {
                for j in 0..n {
                    let product: f64 = (0..n).map(|k| matrix[i * n + k] * inverse[k * n + j]).sum();
                    let expected = if i == j { 1.0 } else { 0.0 };
                    assert!((product - expected).abs() < 1e-12, "n={n}, ({i}, {j})");
                }
            }
        }
    }

    #[test]
    fn clusters_count_images_not_samples() {
        let probe = looklab_model::probes::v1().probes[0].xyz;
        let input = LookInput::new(probe);
        let samples: Vec<ClusterSample> = (0..3)
            .map(|_| ClusterSample {
                image: 0,
                s: input.s,
                h: input.h_in,
                c: input.c_in,
                weight: 1.0,
            })
            .collect();
        assert_eq!(probe_cluster_count(&samples, probe, 1, 0), 1);
        let mut two = samples.clone();
        two.push(ClusterSample {
            image: 1,
            ..samples[0]
        });
        assert_eq!(probe_cluster_count(&samples, probe, 2, 0), 1);
        assert_eq!(probe_cluster_count(&two, probe, 2, 0), 2);
    }

    #[test]
    fn post_clip_covariance_keeps_a_pre_clip_floor() {
        let pre = [[4.0, 0.0, 0.0], [0.0, 4.0, 0.0], [0.0, 0.0, 4.0]];
        let mut post = [[0.0; 3]; 3];
        floor_post_covariance(&pre, &mut post);
        let trace = post[0][0] + post[1][1] + post[2][2];
        assert!((trace - 0.0625 * 12.0).abs() < 1e-12);
        assert!((post[0][0] - trace / 3.0).abs() < 1e-12);
    }

    #[test]
    fn small_cluster_factor_is_largest_at_two() {
        assert!(small_cluster_factor(2) > small_cluster_factor(3));
        assert!(small_cluster_factor(3) > small_cluster_factor(40));
        assert_eq!(small_cluster_factor(1), 1.0);
        let ratio = 12.706_204_6 / 1.644_853_626_951_472_2;
        let expected = 2.0 * ratio * ratio;
        assert!((small_cluster_factor(2) - expected).abs() < 1e-9);
    }

    #[cfg(feature = "synth")]
    #[test]
    fn singular_chroma_probes_are_finite_and_unsupported() {
        let data = crate::synth::dataset(&crate::synth::SynthConfig {
            images: 2,
            samples_per_image: 64,
            ..crate::synth::SynthConfig::default()
        });
        let options = crate::types::FitOptions::default();
        let problem =
            crate::problem::Problem::new_with(&data.images, None, &options).expect("problem");
        let look = looklab_model::Look::colorimetric_spline().expect("look");
        let params = crate::fit::params_from_look(&problem, &look);
        let (probes, density, _) = crate::fit::look_only_probes(
            &problem,
            &params,
            &crate::types::StageLambdas::default(),
            None,
            &options,
        )
        .expect("probe uncertainty");
        let set = looklab_model::probes::v1();
        let mut neutral = 0usize;
        let mut chart = 0usize;
        for (index, probe) in probes.probes.iter().enumerate() {
            assert!(
                probe
                    .covariance
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite())
            );
            assert!(
                probe
                    .post_covariance
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite())
            );
            assert!(probe.sigma_de.is_finite());
            assert!(probe.post_sigma_de.is_finite());
            assert!(probe.bias_de.is_finite());
            if probe.support_withheld {
                neutral += 1;
                assert!(!probe.supported);
                assert!(!probe.supported_sigma);
                assert!(!probe.pre_clip_reliable);
                assert!(probe.covariance[0][0] > 0.0);
                assert_eq!(probe.covariance[1][1], UNKNOWN_VARIANCE);
                assert_eq!(probe.covariance[2][2], UNKNOWN_VARIANCE);
                assert_eq!(probe.post_covariance[1][1], UNKNOWN_VARIANCE);
                assert_eq!(probe.post_covariance[2][2], UNKNOWN_VARIANCE);
                assert!(probe.post_covariance[0][0] < UNKNOWN_VARIANCE);
            } else {
                chart += 1;
                let expected = mark_support(
                    probe.post_sigma_de,
                    probe.bias_de,
                    probe.density,
                    probe.clusters,
                    density.floor,
                    false,
                    false,
                    &options,
                );
                assert_eq!(probe.supported_sigma, expected.sigma);
                assert_eq!(probe.supported, expected.all());
            }
            let input = LookInput::new(set.probes[index].xyz);
            let values = look.layer_values(&input);
            let paths = look.chroma_paths(&input, &values);
            if paths.singular() && paths.blend < 1.0 {
                assert!(probe.support_withheld);
            }
        }
        assert!(neutral > 0, "colorimetric look has no singular gray probe");
        assert!(chart > 0, "colorimetric look has no non-singular probe");
    }
}
