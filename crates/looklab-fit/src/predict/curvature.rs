// SPDX-License-Identifier: AGPL-3.0-or-later
//! Complete analytic second derivative of one sample's blended residual and margin.
//!
//! `f(x, L(x, c), k)` is differentiated over `(x, l, k)` (3 + 9 + 1 local
//! coordinates) and then chained to the layout.

use core::f64::consts::{LN_2, PI};

use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_model::basis::{
    CORE_TERMS, NEUTRAL_JOIN_C, S_COEFS, TINT_COEFS, TONE_COEFS, VOLUME_COEFS, tint_weights_second,
};
use looklab_model::clip;
use looklab_model::composite::{self, CompositeChart, CompositeDerivative};
use looklab_model::domain_penalty::{self, DOMAIN_AXES, PenaltyDerivatives};
use looklab_model::look;
use looklab_model::realization::{LayerKind, ResponseComponent, feature_coefficient};
use looklab_model::solve;
use looklab_model::{LayerValues, Look, LookInput};

use crate::basis;
use crate::layout::Layout;
use crate::predict::{ClipGeometry, ColumnMap, ImageCtx, ResponseTerm, Target, ToneClamp};

struct Active {
    base_theta: u32,
    beta_second: f64,
    layer: u8,
    local: u32,
    weight: f64,
    slope: f64,
    dx: [[f64; 3]; 9],
    params: Vec<(u32, f64)>,
    /// Derivative of the tone/tint layer values `(t, n_a, n_b)` with respect to
    /// this term's coefficient; zero for a term outside that triple.
    spread: [f64; 3],
}

impl Active {
    /// The layer values this term moves, each with the derivative of that layer
    /// value with respect to the term's coefficient.
    fn moves(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.spread
            .iter()
            .copied()
            .enumerate()
            .chain((self.layer >= 3).then_some((self.layer as usize, 1.0)))
    }
}

struct XMap {
    exposure: Vec<(u32, f64)>,
    wb_r: Vec<(u32, f64)>,
    wb_b: Vec<(u32, f64)>,
    delta: [Option<u32>; 6],
    x_full: [f64; 3],
    x_r: [f64; 3],
    x_b: [f64; 3],
    x_delta: [[f64; 3]; 6],
    x_r_delta: [[f64; 3]; 6],
    x_b_delta: [[f64; 3]; 6],
}

struct SampleData {
    l_x: [[f64; 3]; 9],
    l_xx: [[[f64; 3]; 3]; 9],
    active: Vec<Active>,
    tint_a_curv: f64,
    tint_b_curv: f64,
    x_map: XMap,
    clip: Vec<(u32, f64, f64)>,
    total: usize,
    error: Option<CurvatureError>,
}

pub(crate) struct PredictionCurvature {
    data: SampleData,
    f_x: [[f64; 3]; 4],
    f_l: [[f64; 9]; 4],
    f_k: [f64; 4],
    f_xx: [[[f64; 3]; 3]; 4],
    f_xl: [[[f64; 9]; 3]; 4],
    f_xk: [[f64; 3]; 4],
    f_ll: [[[f64; 9]; 9]; 4],
    f_lk: [[f64; 9]; 4],
    f_kk: [f64; 4],
}

pub(crate) struct SampleCurvature {
    prediction: Box<PredictionCurvature>,
    f_x: [f64; 3],
    f_l: [f64; 9],
    f_k: f64,
    f_xx: [[f64; 3]; 3],
    f_xl: [[f64; 9]; 3],
    f_xk: [f64; 3],
    f_ll: [[f64; 9]; 9],
    f_lk: [f64; 9],
    f_kk: f64,
}

#[derive(Default)]
pub(crate) struct CurvatureScratch {
    c_dir: Vec<f64>,
    out_c: Vec<f64>,
}

impl CurvatureScratch {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn prepare(&mut self, active: usize) {
        if self.c_dir.len() < active {
            self.c_dir.resize(active, 0.0);
            self.out_c.resize(active, 0.0);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CurvatureError {
    NonFinite,
    Singular,
    Domain,
    ScratchTooSmall,
    Length,
}

impl PredictionCurvature {
    pub(crate) fn contract(
        self: Box<Self>,
        residual_adjoint: [f64; 3],
        penalty_adjoint: f64,
    ) -> Result<SampleCurvature, CurvatureError> {
        if let Some(error) = self.data.error {
            return Err(error);
        }
        let adjoint = [
            residual_adjoint[0],
            residual_adjoint[1],
            residual_adjoint[2],
            penalty_adjoint,
        ];
        let mut f_x = [0.0_f64; 3];
        let mut f_l = [0.0_f64; 9];
        let mut f_k = 0.0;
        let mut f_xx = [[0.0_f64; 3]; 3];
        let mut f_xl = [[0.0_f64; 9]; 3];
        let mut f_xk = [0.0_f64; 3];
        let mut f_ll = [[0.0_f64; 9]; 9];
        let mut f_lk = [0.0_f64; 9];
        let mut f_kk = 0.0;
        for (channel, weight) in adjoint.iter().enumerate() {
            if *weight == 0.0 {
                continue;
            }
            for i in 0..3 {
                f_x[i] += weight * self.f_x[channel][i];
                for j in 0..3 {
                    f_xx[i][j] += weight * self.f_xx[channel][i][j];
                }
                for j in 0..9 {
                    f_xl[i][j] += weight * self.f_xl[channel][i][j];
                }
                f_xk[i] += weight * self.f_xk[channel][i];
            }
            for j in 0..9 {
                f_l[j] += weight * self.f_l[channel][j];
                f_lk[j] += weight * self.f_lk[channel][j];
                for m in 0..9 {
                    f_ll[j][m] += weight * self.f_ll[channel][j][m];
                }
            }
            f_k += weight * self.f_k[channel];
            f_kk += weight * self.f_kk[channel];
        }
        let finite = f_x
            .iter()
            .chain(f_l.iter())
            .chain(f_xk.iter())
            .chain(f_lk.iter())
            .chain(f_xx.iter().flatten())
            .chain(f_xl.iter().flatten())
            .chain(f_ll.iter().flatten())
            .chain([&f_k, &f_kk])
            .all(|value| value.is_finite());
        if !finite {
            return Err(CurvatureError::NonFinite);
        }
        Ok(SampleCurvature {
            prediction: self,
            f_x,
            f_l,
            f_k,
            f_xx,
            f_xl,
            f_xk,
            f_ll,
            f_lk,
            f_kk,
        })
    }
}

impl SampleCurvature {
    pub(crate) fn prepare_scratch(
        &self,
        scratch: &mut CurvatureScratch,
    ) -> Result<(), CurvatureError> {
        if let Some(error) = self.prediction.data.error {
            return Err(error);
        }
        scratch.prepare(self.prediction.data.active.len());
        Ok(())
    }

    pub(crate) fn add_hvp(
        &self,
        direction: &[f64],
        out: &mut [f64],
        scratch: &mut CurvatureScratch,
    ) -> Result<(), CurvatureError> {
        let data = &self.prediction.data;
        if direction.len() < data.total || out.len() < data.total {
            return Err(CurvatureError::Length);
        }
        let active_len = data.active.len();
        if scratch.c_dir.len() < active_len || scratch.out_c.len() < active_len {
            return Err(CurvatureError::ScratchTooSmall);
        }

        let mut x_dir = [0.0_f64; 3];
        let mut v_e = 0.0;
        for (theta, factor) in &data.x_map.exposure {
            let value = direction[*theta as usize];
            v_e += factor * value;
            for axis in 0..3 {
                x_dir[axis] += factor * data.x_map.x_full[axis] * value;
            }
        }
        let mut v_r = 0.0;
        for (theta, factor) in &data.x_map.wb_r {
            let value = direction[*theta as usize];
            v_r += factor * value;
            for axis in 0..3 {
                x_dir[axis] += factor * data.x_map.x_r[axis] * value;
            }
        }
        let mut v_b = 0.0;
        for (theta, factor) in &data.x_map.wb_b {
            let value = direction[*theta as usize];
            v_b += factor * value;
            for axis in 0..3 {
                x_dir[axis] += factor * data.x_map.x_b[axis] * value;
            }
        }
        let mut delta_v = [0.0_f64; 6];
        for (slot, theta) in data.x_map.delta.iter().enumerate() {
            if let Some(theta) = theta {
                let value = direction[*theta as usize];
                delta_v[slot] = value;
                for axis in 0..3 {
                    x_dir[axis] += data.x_map.x_delta[slot][axis] * value;
                }
            }
        }
        let mut v_k = 0.0;
        let mut k_dir = 0.0;
        for (theta, slope, _) in &data.clip {
            let value = direction[*theta as usize];
            v_k += value;
            k_dir += slope * value;
        }

        for (slot, active) in data.active.iter().enumerate() {
            let mut value = 0.0;
            for (theta, derivative) in &active.params {
                value += derivative * direction[*theta as usize];
            }
            scratch.c_dir[slot] = value;
        }

        let mut l_dir = [0.0_f64; 9];
        for layer in 0..9 {
            for axis in 0..3 {
                l_dir[layer] += data.l_x[layer][axis] * x_dir[axis];
            }
        }
        for (slot, active) in data.active.iter().enumerate() {
            for (layer, factor) in active.moves() {
                l_dir[layer] += active.weight * factor * scratch.c_dir[slot];
            }
        }

        let mut p_x = [0.0_f64; 3];
        let mut p_l = [0.0_f64; 9];
        for i in 0..3 {
            for j in 0..3 {
                p_x[i] += self.f_xx[i][j] * x_dir[j];
            }
            for l in 0..9 {
                p_x[i] += self.f_xl[i][l] * l_dir[l];
            }
            p_x[i] += self.f_xk[i] * k_dir;
        }
        for l in 0..9 {
            for i in 0..3 {
                p_l[l] += self.f_xl[i][l] * x_dir[i];
            }
            for m in 0..9 {
                p_l[l] += self.f_ll[l][m] * l_dir[m];
            }
            p_l[l] += self.f_lk[l] * k_dir;
        }
        let mut p_k = self.f_kk * k_dir;
        for i in 0..3 {
            p_k += self.f_xk[i] * x_dir[i];
        }
        for l in 0..9 {
            p_k += self.f_lk[l] * l_dir[l];
        }

        let mut out_x = p_x;
        for layer in 0..9 {
            for axis in 0..3 {
                out_x[axis] += data.l_x[layer][axis] * p_l[layer];
            }
        }
        for layer in 0..9 {
            let mut hessian_x = [0.0_f64; 3];
            for axis in 0..3 {
                for b in 0..3 {
                    hessian_x[axis] += data.l_xx[layer][axis][b] * x_dir[b];
                }
            }
            for axis in 0..3 {
                out_x[axis] += self.f_l[layer] * hessian_x[axis];
            }
        }
        let mut l_xc = [[0.0_f64; 3]; 9];
        for (slot, active) in data.active.iter().enumerate() {
            let value = scratch.c_dir[slot];
            if value == 0.0 {
                continue;
            }
            for layer in 0..9 {
                for axis in 0..3 {
                    l_xc[layer][axis] += value * active.dx[layer][axis];
                }
            }
        }
        for layer in 0..9 {
            for axis in 0..3 {
                out_x[axis] += self.f_l[layer] * l_xc[layer][axis];
            }
        }

        let mut t_dir = 0.0;
        let mut tint_a_dir = 0.0;
        let mut tint_b_dir = 0.0;
        for (slot, active) in data.active.iter().enumerate() {
            match active.layer {
                0 => t_dir += active.weight * scratch.c_dir[slot],
                1 => tint_a_dir += active.slope * scratch.c_dir[slot],
                2 => tint_b_dir += active.slope * scratch.c_dir[slot],
                _ => {}
            }
        }
        let a_curv = data.tint_a_curv * t_dir + tint_a_dir;
        let b_curv = data.tint_b_curv * t_dir + tint_b_dir;
        for (slot, active) in data.active.iter().enumerate() {
            let mut value = 0.0;
            for (layer, factor) in active.moves() {
                value += factor * p_l[layer];
            }
            value *= active.weight;
            let mut dx_dot = 0.0;
            for l in 0..9 {
                for axis in 0..3 {
                    dx_dot += self.f_l[l] * active.dx[l][axis] * x_dir[axis];
                }
            }
            value += dx_dot;
            match active.layer {
                0 => value += (self.f_l[1] * a_curv + self.f_l[2] * b_curv) * active.weight,
                1 => value += self.f_l[1] * active.slope * t_dir,
                2 => value += self.f_l[2] * active.slope * t_dir,
                _ => {}
            }
            scratch.out_c[slot] = value;
        }

        for (theta, factor) in &data.x_map.exposure {
            out[*theta as usize] += factor * dot3(out_x, data.x_map.x_full);
        }
        for (theta, factor) in &data.x_map.wb_r {
            out[*theta as usize] += factor * dot3(out_x, data.x_map.x_r);
        }
        for (theta, factor) in &data.x_map.wb_b {
            out[*theta as usize] += factor * dot3(out_x, data.x_map.x_b);
        }
        for (slot, theta) in data.x_map.delta.iter().enumerate() {
            if let Some(theta) = theta {
                out[*theta as usize] += dot3(out_x, data.x_map.x_delta[slot]);
            }
        }
        for (slot, active) in data.active.iter().enumerate() {
            let value = scratch.out_c[slot];
            for (theta, derivative) in &active.params {
                out[*theta as usize] += derivative * value;
            }
        }
        for (theta, slope, _) in &data.clip {
            out[*theta as usize] += slope * p_k;
        }

        let mut g_x = self.f_x;
        for layer in 0..9 {
            for axis in 0..3 {
                g_x[axis] += data.l_x[layer][axis] * self.f_l[layer];
            }
        }
        let mut g_delta = [0.0_f64; 6];
        let mut g_r_delta = [0.0_f64; 6];
        let mut g_b_delta = [0.0_f64; 6];
        for slot in 0..6 {
            g_delta[slot] = dot3(g_x, data.x_map.x_delta[slot]);
            g_r_delta[slot] = dot3(g_x, data.x_map.x_r_delta[slot]);
            g_b_delta[slot] = dot3(g_x, data.x_map.x_b_delta[slot]);
        }
        // `v_e` is the exposure direction, so the `LN_2` of `2^e` is already in it.
        let base_e = dot3(g_x, data.x_map.x_full) * v_e
            + dot3(g_x, data.x_map.x_r) * v_r
            + dot3(g_x, data.x_map.x_b) * v_b
            + (0..6)
                .map(|slot| g_delta[slot] * delta_v[slot])
                .sum::<f64>();
        let base_r = dot3(g_x, data.x_map.x_r) * (v_e + v_r)
            + (0..6)
                .map(|slot| g_r_delta[slot] * delta_v[slot])
                .sum::<f64>();
        let base_b = dot3(g_x, data.x_map.x_b) * (v_e + v_b)
            + (0..6)
                .map(|slot| g_b_delta[slot] * delta_v[slot])
                .sum::<f64>();
        for (theta, factor) in &data.x_map.exposure {
            out[*theta as usize] += factor * base_e;
        }
        for (theta, factor) in &data.x_map.wb_r {
            out[*theta as usize] += factor * base_r;
        }
        for (theta, factor) in &data.x_map.wb_b {
            out[*theta as usize] += factor * base_b;
        }
        for slot in 0..6 {
            if let Some(theta) = data.x_map.delta[slot] {
                out[theta as usize] +=
                    g_delta[slot] * v_e + g_r_delta[slot] * v_r + g_b_delta[slot] * v_b;
            }
        }

        for active in &data.active {
            if active.beta_second == 0.0 {
                continue;
            }
            let mut g_c = 0.0;
            for (layer, factor) in active.moves() {
                g_c += factor * self.f_l[layer];
            }
            let theta = active.base_theta as usize;
            out[theta] += g_c * active.weight * active.beta_second * direction[theta];
        }
        for (theta, _, second) in &data.clip {
            out[*theta as usize] += self.f_k * second * v_k;
        }

        if out.iter().any(|value| !value.is_finite()) {
            return Err(CurvatureError::NonFinite);
        }
        Ok(())
    }
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn unpack_second(packed: &[[f64; 3]; 6]) -> [[[f64; 3]; 3]; 3] {
    let pairs = [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
    let mut out = [[[0.0_f64; 3]; 3]; 3];
    for (slot, (a, b)) in pairs.into_iter().enumerate() {
        out[a][b] = packed[slot];
        out[b][a] = packed[slot];
    }
    out
}

fn unpack_neutral(packed: &[[f64; 2]; 6]) -> [[[f64; 2]; 3]; 3] {
    let pairs = [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
    let mut out = [[[0.0_f64; 2]; 3]; 3];
    for (slot, (a, b)) in pairs.into_iter().enumerate() {
        out[a][b] = packed[slot];
        out[b][a] = packed[slot];
    }
    out
}

struct PostMap {
    value: [f64; 3],
    p: [[f64; 3]; 3],
    pp: [[[f64; 3]; 3]; 3],
    k: [f64; 3],
    pk: [[f64; 3]; 3],
    kk: [f64; 3],
}

#[allow(clippy::type_complexity)]
fn post_chain(
    xyz_pre: Xyz,
    jpre: &[[f64; 3]; 3],
    jpre2: &[[[f64; 3]; 3]; 3],
    space: RgbSpace,
    k: f64,
    output: impl Fn(Xyz) -> ([f64; 3], [[f64; 3]; 3], [[[f64; 3]; 3]; 3]),
) -> PostMap {
    let from_xyz = space.from_xyz_matrix().0;
    let to_xyz = space.to_xyz_matrix().0;
    let mut channels = [0.0_f64; 3];
    for channel in 0..3 {
        channels[channel] = from_xyz[channel][0] * xyz_pre.x
            + from_xyz[channel][1] * xyz_pre.y
            + from_xyz[channel][2] * xyz_pre.z;
    }
    let mut d = [0.0_f64; 3];
    let mut d2 = [0.0_f64; 3];
    let mut dk = [0.0_f64; 3];
    let mut dkk = [0.0_f64; 3];
    let mut dxk = [0.0_f64; 3];
    let mut clipped = [0.0_f64; 3];
    for channel in 0..3 {
        let derivatives = clip::soft_clip_derivatives(channels[channel], k);
        clipped[channel] = derivatives.value;
        d[channel] = derivatives.first;
        d2[channel] = derivatives.second;
        dk[channel] = derivatives.first_k;
        dkk[channel] = derivatives.second_k;
        dxk[channel] = derivatives.mixed;
    }
    let mut xyz_post = [0.0_f64; 3];
    for row in 0..3 {
        xyz_post[row] =
            to_xyz[row][0] * clipped[0] + to_xyz[row][1] * clipped[1] + to_xyz[row][2] * clipped[2];
    }
    let mut rgb_pre = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        for channel in 0..3 {
            rgb_pre[axis][channel] = from_xyz[channel][0] * jpre[0][axis]
                + from_xyz[channel][1] * jpre[1][axis]
                + from_xyz[channel][2] * jpre[2][axis];
        }
    }
    let mut xyz_post_p = [[0.0_f64; 3]; 3];
    let mut xyz_post_pp = [[[0.0_f64; 3]; 3]; 3];
    for axis in 0..3 {
        for row in 0..3 {
            xyz_post_p[axis][row] = to_xyz[row][0] * d[0] * rgb_pre[axis][0]
                + to_xyz[row][1] * d[1] * rgb_pre[axis][1]
                + to_xyz[row][2] * d[2] * rgb_pre[axis][2];
        }
        for other in 0..3 {
            for row in 0..3 {
                let mut value = 0.0;
                for channel in 0..3 {
                    let pre = from_xyz[channel][0] * jpre2[axis][other][0]
                        + from_xyz[channel][1] * jpre2[axis][other][1]
                        + from_xyz[channel][2] * jpre2[axis][other][2];
                    value += to_xyz[row][channel]
                        * (d[channel] * pre
                            + d2[channel] * rgb_pre[axis][channel] * rgb_pre[other][channel]);
                }
                xyz_post_pp[axis][other][row] = value;
            }
        }
    }
    let mut xyz_post_k = [0.0_f64; 3];
    let mut xyz_post_kk = [0.0_f64; 3];
    for row in 0..3 {
        xyz_post_k[row] = to_xyz[row][0] * dk[0] + to_xyz[row][1] * dk[1] + to_xyz[row][2] * dk[2];
        xyz_post_kk[row] =
            to_xyz[row][0] * dkk[0] + to_xyz[row][1] * dkk[1] + to_xyz[row][2] * dkk[2];
    }
    let mut xyz_post_pk = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        for row in 0..3 {
            xyz_post_pk[axis][row] = to_xyz[row][0] * dxk[0] * rgb_pre[axis][0]
                + to_xyz[row][1] * dxk[1] * rgb_pre[axis][1]
                + to_xyz[row][2] * dxk[2] * rgb_pre[axis][2];
        }
    }
    let (value, jacobian, second) = output(Xyz::new(xyz_post[0], xyz_post[1], xyz_post[2]));
    let mut p = [[0.0_f64; 3]; 3];
    let mut pp = [[[0.0_f64; 3]; 3]; 3];
    let mut k_value = [0.0_f64; 3];
    for (channel, entry) in k_value.iter_mut().enumerate() {
        *entry = dot3(jacobian[channel], xyz_post_k);
    }
    let mut pk = [[0.0_f64; 3]; 3];
    let mut kk = [0.0_f64; 3];
    for axis in 0..3 {
        for channel in 0..3 {
            p[axis][channel] = dot3(jacobian[channel], xyz_post_p[axis]);
        }
    }
    for channel in 0..3 {
        for a in 0..3 {
            for b in 0..3 {
                pp[a][b][channel] = dot3(jacobian[channel], xyz_post_pp[a][b]);
                for r in 0..3 {
                    for s in 0..3 {
                        pp[a][b][channel] +=
                            second[r][s][channel] * xyz_post_p[a][r] * xyz_post_p[b][s];
                    }
                }
            }
            pk[a][channel] = dot3(jacobian[channel], xyz_post_pk[a]);
            for r in 0..3 {
                for s in 0..3 {
                    pk[a][channel] += second[r][s][channel] * xyz_post_k[r] * xyz_post_p[a][s];
                }
            }
        }
        kk[channel] = dot3(jacobian[channel], xyz_post_kk);
        for r in 0..3 {
            for s in 0..3 {
                kk[channel] += second[r][s][channel] * xyz_post_k[r] * xyz_post_k[s];
            }
        }
    }
    PostMap {
        value,
        p,
        pp,
        k: k_value,
        pk,
        kk,
    }
}

struct PathDerivs {
    p: [[f64; 9]; 5],
    pp: [[f64; 81]; 5],
    x: [[f64; 3]; 5],
    xx: [[[f64; 3]; 3]; 5],
    xl: [[[f64; 9]; 3]; 5],
}

type UnitJacobian = [[f64; 3]; 3];
type UnitHessian = [[[f64; 3]; 3]; 3];

fn unit_derivatives(xyz: Xyz) -> Result<(UnitJacobian, UnitHessian), CurvatureError> {
    let y = xyz.y;
    if y.is_nan() || y <= 0.0 {
        return Err(CurvatureError::Singular);
    }
    let inv_y = 1.0 / y;
    let du = [
        [inv_y, -xyz.x / (y * y), 0.0],
        [0.0, 0.0, 0.0],
        [0.0, -xyz.z / (y * y), inv_y],
    ];
    let mut d2u = [[[0.0_f64; 3]; 3]; 3];
    d2u[0][1][1] = 2.0 * xyz.x / (y * y * y);
    d2u[0][0][1] = -1.0 / (y * y);
    d2u[0][1][0] = -1.0 / (y * y);
    d2u[2][1][1] = 2.0 * xyz.z / (y * y * y);
    d2u[2][2][1] = -1.0 / (y * y);
    d2u[2][1][2] = -1.0 / (y * y);
    Ok((du, d2u))
}

/// At an exact neutral input the reference chroma is zero and its dual hue chain
/// is undefined, so rows no weight reaches stay zero.
fn path_derivatives(
    look: &Look,
    input: &LookInput,
    values: &LayerValues,
    outer_chroma: bool,
    inner: bool,
) -> Result<PathDerivs, CurvatureError> {
    let (du, d2u) = unit_derivatives(input.xyz)?;
    let y = input.y;
    let u = input.unit_xyz;
    let t = values.t;
    let y_ref = solve::solve_y_ref(t, y);
    let dy = look::y_ref_slope(t);
    let xyz_ref = u.scale(y_ref);
    let (_, jac_ref) = look::ucs_xyz_jacobian(xyz_ref);
    let ref2 = unpack_second(&look::ucs_xyz_second(xyz_ref));
    let (_, jac_neutral) = look::neutral_chroma_jacobian(xyz_ref);
    let neutral2 = unpack_neutral(&look::neutral_chroma_second(xyz_ref));
    let u_array = u.to_array();

    let mut ref_x = [[0.0_f64; 3]; 3];
    let mut ref_xx = [[[0.0_f64; 3]; 3]; 3];
    let mut ref_xl = [[[0.0_f64; 9]; 3]; 3];
    let mut n_x = [[0.0_f64; 3]; 2];
    let mut n_xx = [[[0.0_f64; 3]; 3]; 2];
    let mut n_xl = [[[0.0_f64; 9]; 3]; 2];
    for channel in 0..3 {
        if channel > 0 && !outer_chroma {
            continue;
        }
        for axis in 0..3 {
            for a in 0..3 {
                ref_x[channel][axis] += y_ref * jac_ref[channel][a] * du[a][axis];
            }
            for axis2 in 0..3 {
                for a in 0..3 {
                    ref_xx[channel][axis][axis2] +=
                        y_ref * jac_ref[channel][a] * d2u[a][axis][axis2];
                }
                for a in 0..3 {
                    for b in 0..3 {
                        ref_xx[channel][axis][axis2] +=
                            y_ref * y_ref * ref2[a][b][channel] * du[a][axis] * du[b][axis2];
                    }
                }
            }
        }
        for axis in 0..3 {
            let mut extra = 0.0;
            for a in 0..3 {
                for b in 0..3 {
                    extra += ref2[a][b][channel] * u_array[b] * du[a][axis];
                }
            }
            ref_xl[channel][axis][0] =
                dy * ref_x[channel][axis] / y_ref.max(1e-300) + y_ref * dy * extra;
        }
    }
    if inner {
        for channel in 0..2 {
            for axis in 0..3 {
                for a in 0..3 {
                    n_x[channel][axis] += y_ref * jac_neutral[channel][a] * du[a][axis];
                }
                for axis2 in 0..3 {
                    for a in 0..3 {
                        n_xx[channel][axis][axis2] +=
                            y_ref * jac_neutral[channel][a] * d2u[a][axis][axis2];
                    }
                    for a in 0..3 {
                        for b in 0..3 {
                            n_xx[channel][axis][axis2] += y_ref
                                * y_ref
                                * neutral2[a][b][channel]
                                * du[a][axis]
                                * du[b][axis2];
                        }
                    }
                }
            }
            for axis in 0..3 {
                let mut extra = 0.0;
                for a in 0..3 {
                    for b in 0..3 {
                        extra += neutral2[a][b][channel] * u_array[b] * du[a][axis];
                    }
                }
                n_xl[channel][axis][0] =
                    dy * n_x[channel][axis] / y_ref.max(1e-300) + y_ref * dy * extra;
            }
        }
    }

    let gain = libm::exp(values.log_g);
    let radians = values.r * (PI / 180.0);
    let (sin_r, cos_r) = (libm::sin(radians), libm::cos(radians));
    let degrees = PI / 180.0;
    let rotate = |a: f64, b: f64| (cos_r * a - sin_r * b, sin_r * a + cos_r * b);
    let rotate_deriv = |a: f64, b: f64| {
        (
            (-sin_r * a - cos_r * b) * degrees,
            (cos_r * a - sin_r * b) * degrees,
        )
    };

    let mut out = PathDerivs {
        p: [[0.0_f64; 9]; 5],
        pp: [[0.0_f64; 81]; 5],
        x: [[0.0_f64; 3]; 5],
        xx: [[[0.0_f64; 3]; 3]; 5],
        xl: [[[0.0_f64; 9]; 3]; 5],
    };
    let outer_first = look.d_ucs_d_layer_values(input, values);
    let inner_first = look.d_internal_d_layer_values(input, values);
    for layer in 0..9 {
        out.p[0][layer] = outer_first[0][layer];
        if outer_chroma {
            out.p[1][layer] = outer_first[1][layer];
            out.p[2][layer] = outer_first[2][layer];
        }
        if inner {
            out.p[3][layer] = inner_first[layer][0];
            out.p[4][layer] = inner_first[layer][1];
        }
    }
    let (outer_second, inner_second) = look.path_second(input, values);
    for index in 0..81 {
        out.pp[0][index] = outer_second[index][0];
        if outer_chroma {
            out.pp[1][index] = outer_second[index][1];
            out.pp[2][index] = outer_second[index][2];
        }
        if inner {
            out.pp[3][index] = inner_second[index][0];
            out.pp[4][index] = inner_second[index][1];
        }
    }
    for axis in 0..3 {
        out.x[0][axis] = ref_x[0][axis];
        let (a, b) = rotate(ref_x[1][axis], ref_x[2][axis]);
        out.x[1][axis] = gain * a;
        out.x[2][axis] = gain * b;
        let (a, b) = rotate(n_x[0][axis], n_x[1][axis]);
        out.x[3][axis] = gain * a;
        out.x[4][axis] = gain * b;
        for axis2 in 0..3 {
            out.xx[0][axis][axis2] = ref_xx[0][axis][axis2];
            let (a, b) = rotate(ref_xx[1][axis][axis2], ref_xx[2][axis][axis2]);
            out.xx[1][axis][axis2] = gain * a;
            out.xx[2][axis][axis2] = gain * b;
            let (a, b) = rotate(n_xx[0][axis][axis2], n_xx[1][axis][axis2]);
            out.xx[3][axis][axis2] = gain * a;
            out.xx[4][axis][axis2] = gain * b;
        }
        out.xl[0][axis][0] = ref_xl[0][axis][0];
        let (a, b) = rotate(ref_xl[1][axis][0], ref_xl[2][axis][0]);
        out.xl[1][axis][0] = gain * a;
        out.xl[2][axis][0] = gain * b;
        let (a, b) = rotate(n_xl[0][axis][0], n_xl[1][axis][0]);
        out.xl[3][axis][0] = gain * a;
        out.xl[4][axis][0] = gain * b;
        out.xl[1][axis][3] = out.x[1][axis];
        out.xl[2][axis][3] = out.x[2][axis];
        out.xl[3][axis][3] = out.x[3][axis];
        out.xl[4][axis][3] = out.x[4][axis];
        let (a, b) = rotate_deriv(ref_x[1][axis], ref_x[2][axis]);
        out.xl[1][axis][4] = gain * a;
        out.xl[2][axis][4] = gain * b;
        let (a, b) = rotate_deriv(n_x[0][axis], n_x[1][axis]);
        out.xl[3][axis][4] = gain * a;
        out.xl[4][axis][4] = gain * b;
    }
    let finite = out
        .p
        .iter()
        .flatten()
        .chain(out.pp.iter().flatten())
        .chain(out.x.iter().flatten())
        .chain(out.xx.iter().flatten().flatten())
        .chain(out.xl.iter().flatten().flatten())
        .all(|value| value.is_finite());
    if !finite {
        return Err(CurvatureError::NonFinite);
    }
    Ok(out)
}

struct XDerivs {
    s_lookup_x: [f64; 3],
    s_lookup_xx: [[f64; 3]; 3],
    h_x: [f64; 3],
    h_xx: [[f64; 3]; 3],
    c_x: [f64; 3],
    c_xx: [[f64; 3]; 3],
    neutral_x: [[f64; 3]; 2],
    neutral_xx: [[[f64; 3]; 3]; 2],
    residual_a_x: [f64; 3],
    residual_a_xx: [[f64; 3]; 3],
    residual_b_x: [f64; 3],
    residual_b_xx: [[f64; 3]; 3],
    t_x: [f64; 3],
    t_xx: [[f64; 3]; 3],
}

struct XBundle {
    derivs: XDerivs,
    w_x: [f64; 3],
    w_xx: [[f64; 3]; 3],
}

fn blend_derivatives(
    input: &LookInput,
    du: &[[f64; 3]; 3],
    d2u: &[[[f64; 3]; 3]; 3],
) -> ([f64; 3], [[f64; 3]; 3]) {
    let xyz = input.unit_xyz.scale(0.18);
    let (_, jacobian) = look::neutral_chroma_jacobian(xyz);
    let second = unpack_neutral(&look::neutral_chroma_second(xyz));
    let mut n_x = [[0.0_f64; 3]; 2];
    let mut n_xx = [[[0.0_f64; 3]; 3]; 2];
    for channel in 0..2 {
        for axis in 0..3 {
            for a in 0..3 {
                n_x[channel][axis] += 0.18 * jacobian[channel][a] * du[a][axis];
            }
            for axis2 in 0..3 {
                for a in 0..3 {
                    n_xx[channel][axis][axis2] += 0.18 * jacobian[channel][a] * d2u[a][axis][axis2];
                }
                for a in 0..3 {
                    for b in 0..3 {
                        n_xx[channel][axis][axis2] +=
                            0.18 * 0.18 * second[a][b][channel] * du[a][axis] * du[b][axis2];
                    }
                }
            }
        }
    }
    let (a, b) = (input.neutral.a, input.neutral.b);
    let q = a * a + b * b;
    let mut q_x = [0.0_f64; 3];
    for axis in 0..3 {
        q_x[axis] = 2.0 * (a * n_x[0][axis] + b * n_x[1][axis]);
    }
    let mut q_xx = [[0.0_f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            q_xx[i][j] = 2.0
                * (n_x[0][i] * n_x[0][j]
                    + n_x[1][i] * n_x[1][j]
                    + a * n_xx[0][i][j]
                    + b * n_xx[1][i][j]);
        }
    }
    let join2 = NEUTRAL_JOIN_C * NEUTRAL_JOIN_C;
    let (w_q, w_qq) = if q >= join2 {
        (0.0, 0.0)
    } else {
        let r = q / join2;
        (
            (3.0 - 6.0 * r + 3.0 * r * r) / join2,
            (-6.0 + 6.0 * r) / (join2 * join2),
        )
    };
    let w_x = [w_q * q_x[0], w_q * q_x[1], w_q * q_x[2]];
    let mut w_xx = [[0.0_f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            w_xx[i][j] = w_qq * q_x[i] * q_x[j] + w_q * q_xx[i][j];
        }
    }
    (w_x, w_xx)
}

fn build_x_derivs(
    look: &Look,
    input: &LookInput,
    _values: &LayerValues,
) -> Result<XBundle, CurvatureError> {
    let y = input.y;
    let (du, d2u) = unit_derivatives(input.xyz)?;
    let ln2 = core::f64::consts::LN_2;
    let mut s_x = [0.0_f64; 3];
    s_x[1] = 1.0 / (y * ln2);
    let mut s_xx = [[0.0_f64; 3]; 3];
    s_xx[1][1] = -1.0 / (y * y * ln2);
    let lookup_d = looklab_model::basis::exposure_lookup_derivative(input.s);
    let lookup_dd = basis::exposure_lookup_second_derivative(input.s);
    let mut s_lookup_x = [0.0_f64; 3];
    for axis in 0..3 {
        s_lookup_x[axis] = lookup_d * s_x[axis];
    }
    let mut s_lookup_xx = [[0.0_f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            s_lookup_xx[i][j] = lookup_dd * s_x[i] * s_x[j] + lookup_d * s_xx[i][j];
        }
    }

    let xyz = input.unit_xyz.scale(0.18);
    let (neutral, neutral_jac) = look::neutral_chroma_jacobian(xyz);
    let neutral2 = unpack_neutral(&look::neutral_chroma_second(xyz));
    let (a_in, b_in) = (input.a_in, input.b_in);
    let mut a_x = [0.0_f64; 3];
    let mut b_x = [0.0_f64; 3];
    let mut a_xx = [[0.0_f64; 3]; 3];
    let mut b_xx = [[0.0_f64; 3]; 3];
    let mut c_x = [0.0_f64; 3];
    let mut c_xx = [[0.0_f64; 3]; 3];
    let mut h_x = [0.0_f64; 3];
    let mut h_xx = [[0.0_f64; 3]; 3];
    // The input chromaticity only enters weighted by the blend weight, which is
    // zero exactly at a neutral input, where the dual hue chain is undefined.
    if input.blend.w > 0.0 {
        let (ucs, jac) = look::ucs_xyz_jacobian(xyz);
        let ucs2 = unpack_second(&look::ucs_xyz_second(xyz));
        for axis in 0..3 {
            for a in 0..3 {
                a_x[axis] += 0.18 * jac[1][a] * du[a][axis];
                b_x[axis] += 0.18 * jac[2][a] * du[a][axis];
            }
            for axis2 in 0..3 {
                for a in 0..3 {
                    a_xx[axis][axis2] += 0.18 * jac[1][a] * d2u[a][axis][axis2];
                    b_xx[axis][axis2] += 0.18 * jac[2][a] * d2u[a][axis][axis2];
                }
                for a in 0..3 {
                    for b in 0..3 {
                        a_xx[axis][axis2] +=
                            0.18 * 0.18 * ucs2[a][b][1] * du[a][axis] * du[b][axis2];
                        b_xx[axis][axis2] +=
                            0.18 * 0.18 * ucs2[a][b][2] * du[a][axis] * du[b][axis2];
                    }
                }
            }
        }
        let c = input.c_in;
        let degrees = 180.0 / PI;
        if c > 0.0 {
            for axis in 0..3 {
                c_x[axis] = (ucs.a * a_x[axis] + ucs.b * b_x[axis]) / c;
            }
            for i in 0..3 {
                h_x[i] = degrees * (ucs.a * b_x[i] - ucs.b * a_x[i]) / (c * c);
                for j in 0..3 {
                    let numerator =
                        a_x[i] * a_x[j] + b_x[i] * b_x[j] + ucs.a * a_xx[i][j] + ucs.b * b_xx[i][j];
                    let dot_i = ucs.a * a_x[i] + ucs.b * b_x[i];
                    let dot_j = ucs.a * a_x[j] + ucs.b * b_x[j];
                    c_xx[i][j] = numerator / c - dot_i * dot_j / (c * c * c);
                    let angle = ucs.a * b_x[i] - ucs.b * a_x[i];
                    h_xx[i][j] = degrees
                        * ((a_x[j] * b_x[i] - b_x[j] * a_x[i] + ucs.a * b_xx[i][j]
                            - ucs.b * a_xx[i][j])
                            / (c * c)
                            - 2.0 * angle * dot_j / (c * c * c * c));
                }
            }
        }
    }

    let (w_x, w_xx) = blend_derivatives(input, &du, &d2u);
    let w = input.blend.w;
    let mut neutral_x = [[0.0_f64; 3]; 2];
    let mut neutral_xx = [[[0.0_f64; 3]; 3]; 2];
    for channel in 0..2 {
        for axis in 0..3 {
            for a in 0..3 {
                neutral_x[channel][axis] += 0.18 * neutral_jac[channel][a] * du[a][axis];
            }
            for axis2 in 0..3 {
                for a in 0..3 {
                    neutral_xx[channel][axis][axis2] +=
                        0.18 * neutral_jac[channel][a] * d2u[a][axis][axis2];
                }
                for a in 0..3 {
                    for b in 0..3 {
                        neutral_xx[channel][axis][axis2] +=
                            0.18 * 0.18 * neutral2[a][b][channel] * du[a][axis] * du[b][axis2];
                    }
                }
            }
        }
    }
    let mut residual_a_x = [0.0_f64; 3];
    let mut residual_b_x = [0.0_f64; 3];
    for axis in 0..3 {
        residual_a_x[axis] = w_x[axis] * a_in + w * a_x[axis] - w_x[axis] * neutral.a
            + (1.0 - w) * neutral_x[0][axis];
        residual_b_x[axis] = w_x[axis] * b_in + w * b_x[axis] - w_x[axis] * neutral.b
            + (1.0 - w) * neutral_x[1][axis];
    }
    let mut residual_a_xx = [[0.0_f64; 3]; 3];
    let mut residual_b_xx = [[0.0_f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            residual_a_xx[i][j] = w_xx[i][j] * (a_in - neutral.a)
                + w_x[i] * a_x[j]
                + w_x[j] * a_x[i]
                + w * a_xx[i][j]
                - w_x[i] * neutral_x[0][j]
                - w_x[j] * neutral_x[0][i]
                + (1.0 - w) * neutral_xx[0][i][j];
            residual_b_xx[i][j] = w_xx[i][j] * (b_in - neutral.b)
                + w_x[i] * b_x[j]
                + w_x[j] * b_x[i]
                + w * b_xx[i][j]
                - w_x[i] * neutral_x[1][j]
                - w_x[j] * neutral_x[1][i]
                + (1.0 - w) * neutral_xx[1][i][j];
        }
    }

    let (t_x, t_xx) = if look.tone.is_colorimetric() {
        let (_, dt_ds, d2t_ds2) = look::colorimetric_tone_derivatives(input.s);
        let mut t_x = [0.0_f64; 3];
        for axis in 0..3 {
            t_x[axis] = dt_ds * s_x[axis];
        }
        let mut t_xx = [[0.0_f64; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                t_xx[i][j] = d2t_ds2 * s_x[i] * s_x[j] + dt_ds * s_xx[i][j];
            }
        }
        (t_x, t_xx)
    } else {
        let beta = look.tone.betas().copied().unwrap_or([0.0; TONE_COEFS]);
        let d1 = basis::tone_basis_derivative(input.s_lookup);
        let d2 = basis::tone_basis_second_derivative(input.s_lookup);
        let mut first = 0.0;
        let mut second = 0.0;
        for k in 0..TONE_COEFS {
            first += beta[k] * d1[k];
            second += beta[k] * d2[k];
        }
        let mut t_x = [0.0_f64; 3];
        for axis in 0..3 {
            t_x[axis] = first * s_lookup_x[axis];
        }
        let mut t_xx = [[0.0_f64; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                t_xx[i][j] = second * s_lookup_x[i] * s_lookup_x[j] + first * s_lookup_xx[i][j];
            }
        }
        (t_x, t_xx)
    };

    Ok(XBundle {
        derivs: XDerivs {
            s_lookup_x,
            s_lookup_xx,
            h_x,
            h_xx,
            c_x,
            c_xx,
            neutral_x,
            neutral_xx,
            residual_a_x,
            residual_a_xx,
            residual_b_x,
            residual_b_xx,
            t_x,
            t_xx,
        },
        w_x,
        w_xx,
    })
}

struct LayerBuild {
    l_x: [[f64; 3]; 9],
    l_xx: [[[f64; 3]; 3]; 9],
    active: Vec<Active>,
    tint_a_curv: f64,
    tint_b_curv: f64,
}

fn response_params(active: &mut Active, terms: &[ResponseTerm], response_start: usize) {
    for term in terms {
        let derivative = match term.component {
            ResponseComponent::Tone => {
                if active.layer == 0 && active.local == term.feature {
                    Some(term.phi)
                } else {
                    None
                }
            }
            ResponseComponent::Tint => {
                let (layer, local) = if term.feature < TINT_COEFS as u32 {
                    (1u8, term.feature)
                } else {
                    (2u8, term.feature - TINT_COEFS as u32)
                };
                if active.layer == layer && active.local == local {
                    Some(term.phi)
                } else {
                    None
                }
            }
            ResponseComponent::Chroma | ResponseComponent::Hue | ResponseComponent::Lightness => {
                let (layer, kind) = match term.component {
                    ResponseComponent::Chroma => (3u8, LayerKind::Chroma),
                    ResponseComponent::Hue => (4u8, LayerKind::Hue),
                    _ => (5u8, LayerKind::Lightness),
                };
                if active.layer == layer {
                    let coefficient =
                        feature_coefficient(kind, term.feature, active.local as usize);
                    if coefficient != 0.0 {
                        Some(term.phi * coefficient)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            ResponseComponent::WbR | ResponseComponent::WbB => None,
        };
        if let Some(derivative) = derivative {
            active
                .params
                .push((response_start as u32 + term.index as u32, derivative));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn layer_build(
    look: &Look,
    layout: &Layout,
    input: &LookInput,
    values: &LayerValues,
    x: &XDerivs,
    w: f64,
    w_x: &[f64; 3],
    w_xx: &[[f64; 3]; 3],
    terms: &[ResponseTerm],
    beta_slope: &[f64; TONE_COEFS],
    tone_clamp: &[ToneClamp; TONE_COEFS],
) -> Result<LayerBuild, CurvatureError> {
    let mut l_x = [[0.0_f64; 3]; 9];
    let mut l_xx = [[[0.0_f64; 3]; 3]; 9];
    let mut active: Vec<Active> = Vec::new();

    let (tint_idx, tint_w, tint_d) = basis::tint_values(values.t);
    let (_, _, _, tint_dd) = tint_weights_second(values.t);
    let tint_a = &look.tint.a;
    let tint_b = &look.tint.b;
    let mut n_a_x = [0.0_f64; 3];
    let mut n_b_x = [0.0_f64; 3];
    let mut n_a_xx = [[0.0_f64; 3]; 3];
    let mut n_b_xx = [[0.0_f64; 3]; 3];
    let mut tint_a_curv = 0.0;
    let mut tint_b_curv = 0.0;
    let mut na_slope = 0.0;
    let mut nb_slope = 0.0;
    for m in 0..4 {
        let index = tint_idx[m];
        let a = tint_a[index];
        let b = tint_b[index];
        tint_a_curv += a * tint_dd[m];
        tint_b_curv += b * tint_dd[m];
        na_slope += a * tint_d[m];
        nb_slope += b * tint_d[m];
        for axis in 0..3 {
            n_a_x[axis] += a * tint_d[m] * x.t_x[axis];
            n_b_x[axis] += b * tint_d[m] * x.t_x[axis];
        }
        for i in 0..3 {
            for j in 0..3 {
                n_a_xx[i][j] += a * (tint_dd[m] * x.t_x[i] * x.t_x[j] + tint_d[m] * x.t_xx[i][j]);
                n_b_xx[i][j] += b * (tint_dd[m] * x.t_x[i] * x.t_x[j] + tint_d[m] * x.t_xx[i][j]);
            }
        }
    }
    l_x[0] = x.t_x;
    l_xx[0] = x.t_xx;
    l_x[1] = n_a_x;
    l_xx[1] = n_a_xx;
    l_x[2] = n_b_x;
    l_xx[2] = n_b_xx;

    if look.tone.betas().is_some() {
        let mut c0 = Active {
            base_theta: layout.tone_c0 as u32,
            beta_second: 0.0,
            layer: 0,
            local: 0,
            weight: input.tone_weights[0],
            slope: 0.0,
            dx: [[0.0_f64; 3]; 9],
            params: vec![(layout.tone_c0 as u32, 1.0)],
            spread: [1.0, na_slope, nb_slope],
        };
        for axis in 0..3 {
            for m in 0..4 {
                let index = tint_idx[m];
                c0.dx[1][axis] += tint_a[index] * tint_dd[m] * x.t_x[axis];
                c0.dx[2][axis] += tint_b[index] * tint_dd[m] * x.t_x[axis];
            }
        }
        response_params(&mut c0, terms, layout.response.start);
        active.push(c0);

        let d1 = basis::tone_basis_derivative(input.s_lookup);
        for k in 0..TONE_COEFS {
            match tone_clamp[k] {
                ToneClamp::Boundary => return Err(CurvatureError::Singular),
                ToneClamp::Clamped => continue,
                ToneClamp::Interior => {}
            }
            let weight = input.tone_weights[k + 1];
            let t_c = weight;
            let mut s_xc = [0.0_f64; 3];
            for axis in 0..3 {
                s_xc[axis] = d1[k] * x.s_lookup_x[axis];
            }
            let mut term = Active {
                base_theta: (layout.tone_beta.start + k) as u32,
                beta_second: beta_slope[k] * (1.0 - beta_slope[k]),
                layer: 0,
                local: (k + 1) as u32,
                weight,
                slope: 0.0,
                dx: [[0.0_f64; 3]; 9],
                params: vec![((layout.tone_beta.start + k) as u32, beta_slope[k])],
                spread: [1.0, na_slope, nb_slope],
            };
            for axis in 0..3 {
                term.dx[0][axis] = s_xc[axis];
                for m in 0..4 {
                    let index = tint_idx[m];
                    term.dx[1][axis] +=
                        tint_a[index] * (tint_dd[m] * t_c * x.t_x[axis] + tint_d[m] * s_xc[axis]);
                    term.dx[2][axis] +=
                        tint_b[index] * (tint_dd[m] * t_c * x.t_x[axis] + tint_d[m] * s_xc[axis]);
                }
            }
            response_params(&mut term, terms, layout.response.start);
            active.push(term);
        }
    }

    for m in 0..4 {
        let index = tint_idx[m];
        let mut term = Active {
            base_theta: (layout.tint_a.start + index) as u32,
            beta_second: 0.0,
            layer: 1,
            local: index as u32,
            weight: tint_w[m],
            slope: tint_d[m],
            dx: [[0.0_f64; 3]; 9],
            params: vec![((layout.tint_a.start + index) as u32, 1.0)],
            spread: [0.0, 1.0, 0.0],
        };
        for axis in 0..3 {
            term.dx[1][axis] = tint_d[m] * x.t_x[axis];
        }
        response_params(&mut term, terms, layout.response.start);
        active.push(term);

        let mut term = Active {
            base_theta: (layout.tint_b.start + index) as u32,
            beta_second: 0.0,
            layer: 2,
            local: index as u32,
            weight: tint_w[m],
            slope: tint_d[m],
            dx: [[0.0_f64; 3]; 9],
            params: vec![((layout.tint_b.start + index) as u32, 1.0)],
            spread: [0.0, 0.0, 1.0],
        };
        for axis in 0..3 {
            term.dx[2][axis] = tint_d[m] * x.t_x[axis];
        }
        response_params(&mut term, terms, layout.response.start);
        active.push(term);
    }

    let (s_idx, s_w, s_d, s_dd) = basis::s_values_second(input.s_lookup);
    let (hue_idx, hue_w, hue_d, hue_dd) = basis::hue_values_second(input.h_in);
    let (_, c_w, c_d, c_dd) = basis::c_values_second(input.c_in);
    let (core, core_d, core_dd) = basis::core_weights_derivatives(input.neutral.a, input.neutral.b);

    for (layer, range, component) in [
        (3u8, layout.g.clone(), &look.color.g),
        (4u8, layout.r.clone(), &look.color.r),
        (5u8, layout.lambda.clone(), &look.color.lambda),
    ] {
        for s in 0..4 {
            let local = s_idx[s];
            let weight = s_w[s];
            let mut dx = [0.0_f64; 3];
            let mut dxx = [[0.0_f64; 3]; 3];
            for axis in 0..3 {
                dx[axis] = s_d[s] * x.s_lookup_x[axis];
            }
            for i in 0..3 {
                for j in 0..3 {
                    dxx[i][j] =
                        s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j] + s_d[s] * x.s_lookup_xx[i][j];
                }
            }
            let coefficient = component.plane[local];
            for axis in 0..3 {
                l_x[layer as usize][axis] += coefficient * dx[axis];
            }
            for i in 0..3 {
                for j in 0..3 {
                    l_xx[layer as usize][i][j] += coefficient * dxx[i][j];
                }
            }
            let mut term = Active {
                base_theta: (range.start + local) as u32,
                beta_second: 0.0,
                layer,
                local: local as u32,
                weight,
                slope: 0.0,
                dx: [[0.0_f64; 3]; 9],
                params: vec![((range.start + local) as u32, 1.0)],
                spread: [0.0; 3],
            };
            term.dx[layer as usize] = dx;
            response_params(&mut term, terms, layout.response.start);
            active.push(term);
        }
        for h in 0..4 {
            for s in 0..4 {
                for c in 1..4 {
                    let local = S_COEFS + (hue_idx[h] * S_COEFS + s_idx[s]) * 3 + (c - 1);
                    let weight = hue_w[h] * s_w[s] * c_w[c];
                    let f = hue_w[h];
                    let g = s_w[s];
                    let k = c_w[c];
                    let mut fx = [0.0_f64; 3];
                    let mut gx = [0.0_f64; 3];
                    let mut kx = [0.0_f64; 3];
                    let mut fxx = [[0.0_f64; 3]; 3];
                    let mut gxx = [[0.0_f64; 3]; 3];
                    let mut kxx = [[0.0_f64; 3]; 3];
                    for axis in 0..3 {
                        fx[axis] = hue_d[h] * x.h_x[axis];
                        gx[axis] = s_d[s] * x.s_lookup_x[axis];
                        kx[axis] = c_d[c] * x.c_x[axis];
                    }
                    for i in 0..3 {
                        for j in 0..3 {
                            fxx[i][j] = hue_dd[h] * x.h_x[i] * x.h_x[j] + hue_d[h] * x.h_xx[i][j];
                            gxx[i][j] = s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j]
                                + s_d[s] * x.s_lookup_xx[i][j];
                            kxx[i][j] = c_dd[c] * x.c_x[i] * x.c_x[j] + c_d[c] * x.c_xx[i][j];
                        }
                    }
                    let mut dx = [0.0_f64; 3];
                    for axis in 0..3 {
                        dx[axis] = fx[axis] * g * k + f * gx[axis] * k + f * g * kx[axis];
                    }
                    let mut dxx = [[0.0_f64; 3]; 3];
                    for i in 0..3 {
                        for j in 0..3 {
                            dxx[i][j] = fxx[i][j] * g * k + f * gxx[i][j] * k + f * g * kxx[i][j];
                            dxx[i][j] += fx[i] * gx[j] * k + fx[j] * gx[i] * k;
                            dxx[i][j] += fx[i] * g * kx[j] + fx[j] * g * kx[i];
                            dxx[i][j] += f * gx[i] * kx[j] + f * gx[j] * kx[i];
                        }
                    }
                    let coefficient = component.volume[local - S_COEFS];
                    for axis in 0..3 {
                        l_x[layer as usize][axis] += coefficient * dx[axis];
                    }
                    for i in 0..3 {
                        for j in 0..3 {
                            l_xx[layer as usize][i][j] += coefficient * dxx[i][j];
                        }
                    }
                    let mut term = Active {
                        base_theta: (range.start + local) as u32,
                        beta_second: 0.0,
                        layer,
                        local: local as u32,
                        weight,
                        slope: 0.0,
                        dx: [[0.0_f64; 3]; 9],
                        params: vec![((range.start + local) as u32, 1.0)],
                        spread: [0.0; 3],
                    };
                    term.dx[layer as usize] = dx;
                    response_params(&mut term, terms, layout.response.start);
                    active.push(term);
                }
            }
        }
        for s in 0..4 {
            for term_index in 0..CORE_TERMS {
                let local = S_COEFS + VOLUME_COEFS + s_idx[s] * CORE_TERMS + term_index;
                let weight = s_w[s] * core[term_index];
                let mut dx = [0.0_f64; 3];
                let mut dxx = [[0.0_f64; 3]; 3];
                let mut core_x = [0.0_f64; 3];
                for axis in 0..3 {
                    core_x[axis] = core_d[0][term_index] * x.neutral_x[0][axis]
                        + core_d[1][term_index] * x.neutral_x[1][axis];
                    dx[axis] =
                        s_d[s] * x.s_lookup_x[axis] * core[term_index] + s_w[s] * core_x[axis];
                }
                for i in 0..3 {
                    for j in 0..3 {
                        dxx[i][j] = (s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j]
                            + s_d[s] * x.s_lookup_xx[i][j])
                            * core[term_index]
                            + s_w[s]
                                * (core_dd[0][0][term_index]
                                    * x.neutral_x[0][i]
                                    * x.neutral_x[0][j]
                                    + core_dd[0][1][term_index]
                                        * x.neutral_x[0][i]
                                        * x.neutral_x[1][j]
                                    + core_dd[1][0][term_index]
                                        * x.neutral_x[1][i]
                                        * x.neutral_x[0][j]
                                    + core_dd[1][1][term_index]
                                        * x.neutral_x[1][i]
                                        * x.neutral_x[1][j]
                                    + core_d[0][term_index] * x.neutral_xx[0][i][j]
                                    + core_d[1][term_index] * x.neutral_xx[1][i][j]);
                        dxx[i][j] +=
                            s_d[s] * (x.s_lookup_x[i] * core_x[j] + x.s_lookup_x[j] * core_x[i]);
                    }
                }
                let coefficient = component.core[local - S_COEFS - VOLUME_COEFS];
                for axis in 0..3 {
                    l_x[layer as usize][axis] += coefficient * dx[axis];
                }
                for i in 0..3 {
                    for j in 0..3 {
                        l_xx[layer as usize][i][j] += coefficient * dxx[i][j];
                    }
                }
                let mut term = Active {
                    base_theta: (range.start + local) as u32,
                    beta_second: 0.0,
                    layer,
                    local: local as u32,
                    weight,
                    slope: 0.0,
                    dx: [[0.0_f64; 3]; 9],
                    params: vec![((range.start + local) as u32, 1.0)],
                    spread: [0.0; 3],
                };
                term.dx[layer as usize] = dx;
                response_params(&mut term, terms, layout.response.start);
                active.push(term);
            }
        }
    }

    l_x[3] = [0.0_f64; 3];
    l_x[4] = [0.0_f64; 3];
    l_x[5] = [0.0_f64; 3];
    l_xx[3] = [[0.0_f64; 3]; 3];
    l_xx[4] = [[0.0_f64; 3]; 3];
    l_xx[5] = [[0.0_f64; 3]; 3];
    let c0 = c_w[0];
    let mut c0_x = [0.0_f64; 3];
    let mut c0_xx = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        c0_x[axis] = c_d[0] * x.c_x[axis];
    }
    for i in 0..3 {
        for j in 0..3 {
            c0_xx[i][j] = c_dd[0] * x.c_x[i] * x.c_x[j] + c_d[0] * x.c_xx[i][j];
        }
    }
    let a_value = w * c0 + (1.0 - w);
    let mut a_x = [0.0_f64; 3];
    let mut a_xx = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        a_x[axis] = w_x[axis] * (c0 - 1.0) + w * c0_x[axis];
    }
    for i in 0..3 {
        for j in 0..3 {
            a_xx[i][j] =
                w_xx[i][j] * (c0 - 1.0) + w_x[i] * c0_x[j] + w_x[j] * c0_x[i] + w * c0_xx[i][j];
        }
    }
    let near = 1.0 - w;
    for (layer, _range, component) in [
        (3u8, layout.g.clone(), &look.color.g),
        (4u8, layout.r.clone(), &look.color.r),
        (5u8, layout.lambda.clone(), &look.color.lambda),
    ] {
        let mut plane = 0.0_f64;
        let mut plane_x = [0.0_f64; 3];
        let mut plane_xx = [[0.0_f64; 3]; 3];
        for s in 0..4 {
            let local = s_idx[s];
            let coefficient = component.plane[local];
            let mut basis_x = [0.0_f64; 3];
            let mut basis_xx = [[0.0_f64; 3]; 3];
            for axis in 0..3 {
                basis_x[axis] = s_d[s] * x.s_lookup_x[axis];
            }
            for i in 0..3 {
                for j in 0..3 {
                    basis_xx[i][j] =
                        s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j] + s_d[s] * x.s_lookup_xx[i][j];
                }
            }
            plane += coefficient * s_w[s];
            for axis in 0..3 {
                plane_x[axis] += coefficient * basis_x[axis];
            }
            for i in 0..3 {
                for j in 0..3 {
                    plane_xx[i][j] += coefficient * basis_xx[i][j];
                }
            }
            if let Some(entry) = active
                .iter_mut()
                .find(|entry| entry.layer == layer && entry.local == local as u32)
            {
                entry.weight = a_value * s_w[s];
                for axis in 0..3 {
                    entry.dx[layer as usize][axis] = a_x[axis] * s_w[s] + a_value * basis_x[axis];
                }
            }
        }
        let mut volume = 0.0_f64;
        let mut volume_x = [0.0_f64; 3];
        let mut volume_xx = [[0.0_f64; 3]; 3];
        for h in 0..4 {
            for s in 0..4 {
                for c in 1..4 {
                    let local = S_COEFS + (hue_idx[h] * S_COEFS + s_idx[s]) * 3 + (c - 1);
                    let coefficient = component.volume[local - S_COEFS];
                    let f = hue_w[h];
                    let g = s_w[s];
                    let k = c_w[c];
                    let mut fx = [0.0_f64; 3];
                    let mut gx = [0.0_f64; 3];
                    let mut kx = [0.0_f64; 3];
                    let mut fxx = [[0.0_f64; 3]; 3];
                    let mut gxx = [[0.0_f64; 3]; 3];
                    let mut kxx = [[0.0_f64; 3]; 3];
                    for axis in 0..3 {
                        fx[axis] = hue_d[h] * x.h_x[axis];
                        gx[axis] = s_d[s] * x.s_lookup_x[axis];
                        kx[axis] = c_d[c] * x.c_x[axis];
                    }
                    for i in 0..3 {
                        for j in 0..3 {
                            fxx[i][j] = hue_dd[h] * x.h_x[i] * x.h_x[j] + hue_d[h] * x.h_xx[i][j];
                            gxx[i][j] = s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j]
                                + s_d[s] * x.s_lookup_xx[i][j];
                            kxx[i][j] = c_dd[c] * x.c_x[i] * x.c_x[j] + c_d[c] * x.c_xx[i][j];
                        }
                    }
                    let basis = f * g * k;
                    let mut basis_x = [0.0_f64; 3];
                    let mut basis_xx = [[0.0_f64; 3]; 3];
                    for axis in 0..3 {
                        basis_x[axis] = fx[axis] * g * k + f * gx[axis] * k + f * g * kx[axis];
                    }
                    for i in 0..3 {
                        for j in 0..3 {
                            basis_xx[i][j] = fxx[i][j] * g * k
                                + f * gxx[i][j] * k
                                + f * g * kxx[i][j]
                                + fx[i] * gx[j] * k
                                + fx[j] * gx[i] * k
                                + fx[i] * g * kx[j]
                                + fx[j] * g * kx[i]
                                + f * gx[i] * kx[j]
                                + f * gx[j] * kx[i];
                        }
                    }
                    volume += coefficient * basis;
                    for axis in 0..3 {
                        volume_x[axis] += coefficient * basis_x[axis];
                    }
                    for i in 0..3 {
                        for j in 0..3 {
                            volume_xx[i][j] += coefficient * basis_xx[i][j];
                        }
                    }
                    if let Some(entry) = active
                        .iter_mut()
                        .find(|entry| entry.layer == layer && entry.local == local as u32)
                    {
                        entry.weight = w * basis;
                        for axis in 0..3 {
                            entry.dx[layer as usize][axis] = w_x[axis] * basis + w * basis_x[axis];
                        }
                    }
                }
            }
        }
        let mut core_value = 0.0_f64;
        let mut core_x = [0.0_f64; 3];
        let mut core_xx = [[0.0_f64; 3]; 3];
        for s in 0..4 {
            for term_index in 0..CORE_TERMS {
                let local = S_COEFS + VOLUME_COEFS + s_idx[s] * CORE_TERMS + term_index;
                let coefficient = component.core[local - S_COEFS - VOLUME_COEFS];
                let mut basis_x = [0.0_f64; 3];
                let mut basis_xx = [[0.0_f64; 3]; 3];
                let mut basis_core_x = [0.0_f64; 3];
                for axis in 0..3 {
                    basis_core_x[axis] = core_d[0][term_index] * x.neutral_x[0][axis]
                        + core_d[1][term_index] * x.neutral_x[1][axis];
                    basis_x[axis] = s_d[s] * x.s_lookup_x[axis] * core[term_index]
                        + s_w[s] * basis_core_x[axis];
                }
                for i in 0..3 {
                    for j in 0..3 {
                        basis_xx[i][j] = (s_dd[s] * x.s_lookup_x[i] * x.s_lookup_x[j]
                            + s_d[s] * x.s_lookup_xx[i][j])
                            * core[term_index]
                            + s_w[s]
                                * (core_dd[0][0][term_index]
                                    * x.neutral_x[0][i]
                                    * x.neutral_x[0][j]
                                    + core_dd[0][1][term_index]
                                        * x.neutral_x[0][i]
                                        * x.neutral_x[1][j]
                                    + core_dd[1][0][term_index]
                                        * x.neutral_x[1][i]
                                        * x.neutral_x[0][j]
                                    + core_dd[1][1][term_index]
                                        * x.neutral_x[1][i]
                                        * x.neutral_x[1][j]
                                    + core_d[0][term_index] * x.neutral_xx[0][i][j]
                                    + core_d[1][term_index] * x.neutral_xx[1][i][j])
                            + s_d[s]
                                * (x.s_lookup_x[i] * basis_core_x[j]
                                    + x.s_lookup_x[j] * basis_core_x[i]);
                    }
                }
                let basis = s_w[s] * core[term_index];
                core_value += coefficient * basis;
                for axis in 0..3 {
                    core_x[axis] += coefficient * basis_x[axis];
                }
                for i in 0..3 {
                    for j in 0..3 {
                        core_xx[i][j] += coefficient * basis_xx[i][j];
                    }
                }
                if let Some(entry) = active
                    .iter_mut()
                    .find(|entry| entry.layer == layer && entry.local == local as u32)
                {
                    entry.weight = near * basis;
                    for axis in 0..3 {
                        entry.dx[layer as usize][axis] = -w_x[axis] * basis + near * basis_x[axis];
                    }
                }
            }
        }
        for axis in 0..3 {
            l_x[layer as usize][axis] = a_x[axis] * plane
                + a_value * plane_x[axis]
                + w_x[axis] * volume
                + w * volume_x[axis]
                - w_x[axis] * core_value
                + near * core_x[axis];
        }
        for i in 0..3 {
            for j in 0..3 {
                l_xx[layer as usize][i][j] = a_xx[i][j] * plane
                    + a_x[i] * plane_x[j]
                    + a_x[j] * plane_x[i]
                    + a_value * plane_xx[i][j]
                    + w_xx[i][j] * volume
                    + w_x[i] * volume_x[j]
                    + w_x[j] * volume_x[i]
                    + w * volume_xx[i][j]
                    - w_xx[i][j] * core_value
                    - w_x[i] * core_x[j]
                    - w_x[j] * core_x[i]
                    + near * core_xx[i][j];
            }
        }
    }

    let (indices, weights, first, second) =
        basis::residual_values_second(input.s_lookup, input.residual_a, input.residual_b);
    for m in 0..64 {
        let node = indices[m];
        let weight = weights[m];
        let mut dx = [0.0_f64; 3];
        let mut dxx = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            dx[axis] = first[0][m] * x.s_lookup_x[axis]
                + first[1][m] * x.residual_a_x[axis]
                + first[2][m] * x.residual_b_x[axis];
        }
        for i in 0..3 {
            for j in 0..3 {
                dxx[i][j] = second[0][0][m] * x.s_lookup_x[i] * x.s_lookup_x[j]
                    + first[0][m] * x.s_lookup_xx[i][j]
                    + second[1][1][m] * x.residual_a_x[i] * x.residual_a_x[j]
                    + first[1][m] * x.residual_a_xx[i][j]
                    + second[2][2][m] * x.residual_b_x[i] * x.residual_b_x[j]
                    + first[2][m] * x.residual_b_xx[i][j]
                    + second[0][1][m]
                        * (x.s_lookup_x[i] * x.residual_a_x[j]
                            + x.residual_a_x[i] * x.s_lookup_x[j])
                    + second[0][2][m]
                        * (x.s_lookup_x[i] * x.residual_b_x[j]
                            + x.residual_b_x[i] * x.s_lookup_x[j])
                    + second[1][2][m]
                        * (x.residual_a_x[i] * x.residual_b_x[j]
                            + x.residual_b_x[i] * x.residual_a_x[j]);
            }
        }
        for channel in 0..3 {
            let layer = 6 + channel;
            let coefficient = look.residual.values[node * 3 + channel];
            for axis in 0..3 {
                l_x[layer][axis] += coefficient * dx[axis];
            }
            for i in 0..3 {
                for j in 0..3 {
                    l_xx[layer][i][j] += coefficient * dxx[i][j];
                }
            }
            let mut term = Active {
                base_theta: (layout.residual.start + node * 3 + channel) as u32,
                beta_second: 0.0,
                layer: layer as u8,
                local: (node * 3 + channel) as u32,
                weight,
                slope: 0.0,
                dx: [[0.0_f64; 3]; 9],
                params: vec![((layout.residual.start + node * 3 + channel) as u32, 1.0)],
                spread: [0.0; 3],
            };
            term.dx[layer] = dx;
            active.push(term);
        }
    }

    let finite = l_x
        .iter()
        .flatten()
        .chain(l_xx.iter().flatten().flatten())
        .chain(active.iter().flat_map(|entry| entry.dx.iter().flatten()))
        .all(|value| value.is_finite());
    if !finite {
        return Err(CurvatureError::NonFinite);
    }
    Ok(LayerBuild {
        l_x,
        l_xx,
        active,
        tint_a_curv,
        tint_b_curv,
    })
}

struct MapDerivs {
    p: [[f64; 5]; 4],
    pp: [[[f64; 5]; 5]; 4],
    w: [f64; 4],
    pw: [[f64; 5]; 4],
    k: [f64; 4],
    pk: [[f64; 5]; 4],
    wk: [f64; 4],
    kk: [f64; 4],
}

impl MapDerivs {
    fn zero() -> Self {
        Self {
            p: [[0.0_f64; 5]; 4],
            pp: [[[0.0_f64; 5]; 5]; 4],
            w: [0.0_f64; 4],
            pw: [[0.0_f64; 5]; 4],
            k: [0.0_f64; 4],
            pk: [[0.0_f64; 5]; 4],
            wk: [0.0_f64; 4],
            kk: [0.0_f64; 4],
        }
    }
}

/// Path slots of the internal `(J', n_a, n_b)` coordinates.
const INNER_SLOT: [usize; 3] = [0, 3, 4];

fn build_post_maps(
    paths: &looklab_model::ChromaPaths,
    target: &Target,
    space: RgbSpace,
    clip_k: f64,
    outer_identity: bool,
) -> Result<MapDerivs, CurvatureError> {
    let w = paths.blend;
    let near = 1.0 - w;
    let (xyz_pre_o, jpre_o) = clip::preclip_xyz_jacobian(paths.ucs);
    let target_outer = [target.ucs.j, target.ucs.a, target.ucs.b];
    let target_inner = [target.ucs.j, target.internal[0], target.internal[1]];

    // The outer value is kept at zero weight: the blend's second derivative weights it.
    let outer = if outer_identity {
        // The unclipped composite F∘P is the identity; bypass the stage factors.
        let preclip_rgb = space.xyz_to_rgb_linear(xyz_pre_o);
        let preclip_channels = [preclip_rgb.r, preclip_rgb.g, preclip_rgb.b];
        let clipped = preclip_channels.map(|value| clip::soft_clip(value, clip_k));
        let xyz_post = space.rgb_linear_to_xyz(Rgb::new(clipped[0], clipped[1], clipped[2]));
        PostMap {
            value: solve::ucs_of_xyz(xyz_post).to_array(),
            p: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            pp: [[[0.0_f64; 3]; 3]; 3],
            k: [0.0; 3],
            pk: [[0.0; 3]; 3],
            kk: [0.0; 3],
        }
    } else {
        let outer_point =
            composite::evaluate(paths.ucs, space, clip_k).map_err(|_| CurvatureError::Domain)?;
        if w > 0.0 && matches!(outer_point.derivative, CompositeDerivative::Singular(_)) {
            return Err(CurvatureError::Singular);
        }
        let chart = outer_point.chart;
        let departure = outer_point.xyz_departure;
        let jpre2_o = clip::preclip_xyz_second(paths.ucs);
        post_chain(
            xyz_pre_o,
            &jpre_o,
            &jpre2_o,
            space,
            clip_k,
            |post| match chart {
                CompositeChart::WhiteRelative => {
                    let (ucs, jacobian) = look::ucs_xyz_jacobian_departure(departure);
                    (
                        ucs.to_array(),
                        jacobian,
                        unpack_second(&look::ucs_xyz_second_departure(departure)),
                    )
                }
                CompositeChart::Absolute => {
                    let (ucs, jacobian) = look::ucs_xyz_jacobian(post);
                    (
                        ucs.to_array(),
                        jacobian,
                        unpack_second(&look::ucs_xyz_second(post)),
                    )
                }
            },
        )
    };
    let outer_derivs = (w > 0.0).then_some(&outer);
    let inner = (near > 0.0).then(|| {
        let (xyz_pre_i, jpre_i) = clip::preclip_internal_xyz_jacobian(paths.internal, paths.ucs.j);
        let jpre2_i = clip::preclip_internal_xyz_second(paths.internal, paths.ucs.j);
        post_chain(xyz_pre_i, &jpre_i, &jpre2_i, space, clip_k, |post| {
            let (ucs, _) = look::ucs_xyz_jacobian(post);
            let (neutral, neutral_jacobian) = look::neutral_chroma_jacobian(post);
            let packed_ucs = look::ucs_xyz_second(post);
            let packed_neutral = look::neutral_chroma_second(post);
            let pairs = [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
            let mut jacobian = [[0.0_f64; 3]; 3];
            jacobian[0] = look::j_prime_xyz_gradient(post);
            jacobian[1] = neutral_jacobian[0];
            jacobian[2] = neutral_jacobian[1];
            let mut second = [[[0.0_f64; 3]; 3]; 3];
            for (slot, (a, b)) in pairs.into_iter().enumerate() {
                second[a][b] = [
                    packed_ucs[slot][0],
                    packed_neutral[slot][0],
                    packed_neutral[slot][1],
                ];
                second[b][a] = second[a][b];
            }
            ([ucs.j, neutral.a, neutral.b], jacobian, second)
        })
    });

    let mut maps = MapDerivs::zero();
    for channel in 0..3 {
        maps.w[channel] += outer.value[channel] - target_outer[channel];
        if let Some(map) = outer_derivs {
            for slot in 0..3 {
                maps.p[channel][slot] += w * map.p[slot][channel];
                maps.pw[channel][slot] += map.p[slot][channel];
                maps.pk[channel][slot] += w * map.pk[slot][channel];
                for other in 0..3 {
                    maps.pp[channel][slot][other] += w * map.pp[slot][other][channel];
                }
            }
            maps.k[channel] += w * map.k[channel];
            maps.wk[channel] += map.k[channel];
            maps.kk[channel] += w * map.kk[channel];
        }
        if let Some(map) = &inner {
            for slot in 0..3 {
                let at = INNER_SLOT[slot];
                maps.p[channel][at] += near * map.p[slot][channel];
                maps.pw[channel][at] -= map.p[slot][channel];
                maps.pk[channel][at] += near * map.pk[slot][channel];
                for other in 0..3 {
                    maps.pp[channel][at][INNER_SLOT[other]] += near * map.pp[slot][other][channel];
                }
            }
            maps.w[channel] -= map.value[channel] - target_inner[channel];
            maps.k[channel] += near * map.k[channel];
            maps.wk[channel] -= map.k[channel];
            maps.kk[channel] += near * map.kk[channel];
        }
    }
    let finite = maps
        .p
        .iter()
        .flatten()
        .chain(maps.pp.iter().flatten().flatten())
        .chain(maps.pw.iter().flatten())
        .chain(maps.pk.iter().flatten())
        .chain(maps.w.iter())
        .chain(maps.k.iter())
        .chain(maps.wk.iter())
        .chain(maps.kk.iter())
        .all(|value| value.is_finite());
    if !finite {
        return Err(CurvatureError::NonFinite);
    }
    Ok(maps)
}

pub(crate) fn failed(error: CurvatureError) -> PredictionCurvature {
    PredictionCurvature {
        data: SampleData {
            l_x: [[0.0; 3]; 9],
            l_xx: [[[0.0; 3]; 3]; 9],
            active: Vec::new(),
            tint_a_curv: 0.0,
            tint_b_curv: 0.0,
            x_map: XMap {
                exposure: Vec::new(),
                wb_r: Vec::new(),
                wb_b: Vec::new(),
                delta: [None; 6],
                x_full: [0.0; 3],
                x_r: [0.0; 3],
                x_b: [0.0; 3],
                x_delta: [[0.0; 3]; 6],
                x_r_delta: [[0.0; 3]; 6],
                x_b_delta: [[0.0; 3]; 6],
            },
            clip: Vec::new(),
            total: 0,
            error: Some(error),
        },
        f_x: [[0.0; 3]; 4],
        f_l: [[0.0; 9]; 4],
        f_k: [0.0; 4],
        f_xx: [[[0.0; 3]; 3]; 4],
        f_xl: [[[0.0; 9]; 3]; 4],
        f_xk: [[0.0; 3]; 4],
        f_ll: [[[0.0; 9]; 9]; 4],
        f_lk: [[0.0; 9]; 4],
        f_kk: [0.0; 4],
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    look: &Look,
    layout: &Layout,
    image: &ImageCtx,
    columns: &ColumnMap,
    rgb: [f64; 3],
    clip_k: f64,
    clip_slope: f64,
    beta_slope: &[f64; TONE_COEFS],
    terms: &[ResponseTerm],
    target: &Target,
    penalty: &PenaltyDerivatives,
    crossing: bool,
    outer_identity: bool,
) -> PredictionCurvature {
    match build_inner(
        look,
        layout,
        image,
        columns,
        rgb,
        clip_k,
        clip_slope,
        beta_slope,
        terms,
        target,
        penalty,
        crossing,
        outer_identity,
    ) {
        Ok(prediction) => prediction,
        Err(error) => failed(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_inner(
    look: &Look,
    layout: &Layout,
    image: &ImageCtx,
    columns: &ColumnMap,
    rgb: [f64; 3],
    clip_k: f64,
    clip_slope: f64,
    beta_slope: &[f64; TONE_COEFS],
    terms: &[ResponseTerm],
    target: &Target,
    penalty: &PenaltyDerivatives,
    crossing: bool,
    outer_identity: bool,
) -> Result<PredictionCurvature, CurvatureError> {
    let exposure_scale = image.exposure_scale;
    let cam = [
        rgb[0] * image.wb_r * exposure_scale,
        rgb[1] * exposure_scale,
        rgb[2] * image.wb_b * exposure_scale,
    ];
    let xyz = Xyz::from_array(image.transform.mul_vec(cam));
    let input = LookInput::new(xyz);
    if input.xyz.x != xyz.x || input.xyz.y != xyz.y || input.xyz.z != xyz.z {
        return Err(CurvatureError::Singular);
    }
    let values = look.layer_values(&input);
    if domain_penalty::check_reference_tone(&values).is_err() {
        return Err(CurvatureError::Domain);
    }
    let paths = look.chroma_paths(&input, &values);

    let xb = build_x_derivs(look, &input, &values)?;
    let layer = layer_build(
        look,
        layout,
        &input,
        &values,
        &xb.derivs,
        paths.blend,
        &xb.w_x,
        &xb.w_xx,
        terms,
        beta_slope,
        &image.tone_clamp,
    )?;
    // A clipped outer-neutral crossing has no classical residual Hessian: the row
    // keeps its first Jacobian and Gauss-Newton curvature plus the exact penalty.
    let maps = if crossing {
        MapDerivs::zero()
    } else {
        build_post_maps(&paths, target, image.space, clip_k, outer_identity)?
    };
    let outer_chroma = paths.blend > 0.0;
    let path = path_derivatives(look, &input, &values, outer_chroma, paths.blend < 1.0)?;
    let w_x = xb.w_x;
    let w_xx = xb.w_xx;

    let mut f_x = [[0.0_f64; 3]; 4];
    let mut f_l = [[0.0_f64; 9]; 4];
    let mut f_k = [0.0_f64; 4];
    let mut f_xx = [[[0.0_f64; 3]; 3]; 4];
    let mut f_xl = [[[0.0_f64; 9]; 3]; 4];
    let mut f_xk = [[0.0_f64; 3]; 4];
    let mut f_ll = [[[0.0_f64; 9]; 9]; 4];
    let mut f_lk = [[0.0_f64; 9]; 4];
    let mut f_kk = [0.0_f64; 4];
    for channel in 0..3 {
        for p in 0..5 {
            for i in 0..3 {
                f_x[channel][i] += maps.p[channel][p] * path.x[p][i];
            }
            for j in 0..9 {
                f_l[channel][j] += maps.p[channel][p] * path.p[p][j];
            }
        }
        for i in 0..3 {
            f_x[channel][i] += maps.w[channel] * w_x[i];
        }
        f_k[channel] = maps.k[channel];
        for a in 0..5 {
            for b in 0..5 {
                for i in 0..3 {
                    for j in 0..3 {
                        f_xx[channel][i][j] += path.x[a][i] * maps.pp[channel][a][b] * path.x[b][j];
                    }
                }
            }
        }
        for p in 0..5 {
            for i in 0..3 {
                for j in 0..3 {
                    f_xx[channel][i][j] += maps.p[channel][p] * path.xx[p][i][j];
                }
            }
            for i in 0..3 {
                for j in 0..3 {
                    f_xx[channel][i][j] +=
                        maps.pw[channel][p] * (path.x[p][i] * w_x[j] + path.x[p][j] * w_x[i]);
                }
            }
        }
        for i in 0..3 {
            for j in 0..3 {
                f_xx[channel][i][j] += maps.w[channel] * w_xx[i][j];
            }
        }
        for a in 0..5 {
            for b in 0..5 {
                for i in 0..3 {
                    for j in 0..9 {
                        f_xl[channel][i][j] += path.x[a][i] * maps.pp[channel][a][b] * path.p[b][j];
                    }
                }
            }
        }
        for p in 0..5 {
            for i in 0..3 {
                for j in 0..9 {
                    f_xl[channel][i][j] += maps.p[channel][p] * path.xl[p][i][j];
                    f_xl[channel][i][j] += maps.pw[channel][p] * path.p[p][j] * w_x[i];
                }
            }
        }
        for a in 0..5 {
            for b in 0..5 {
                for j in 0..9 {
                    for k in 0..9 {
                        f_ll[channel][j][k] += path.p[a][j] * maps.pp[channel][a][b] * path.p[b][k];
                    }
                }
            }
        }
        for p in 0..5 {
            for j in 0..9 {
                for k in 0..9 {
                    f_ll[channel][j][k] += maps.p[channel][p] * path.pp[p][j * 9 + k];
                }
            }
        }
        for p in 0..5 {
            for i in 0..3 {
                f_xk[channel][i] += maps.pk[channel][p] * path.x[p][i];
            }
            for j in 0..9 {
                f_lk[channel][j] += maps.pk[channel][p] * path.p[p][j];
            }
        }
        for i in 0..3 {
            f_xk[channel][i] += maps.wk[channel] * w_x[i];
        }
        f_kk[channel] = maps.kk[channel];
    }

    let mut ax_x = [[0.0_f64; 3]; DOMAIN_AXES];
    let mut ax_xx = [[[0.0_f64; 3]; 3]; DOMAIN_AXES];
    let mut ax_l = [[0.0_f64; 9]; DOMAIN_AXES];
    let mut ax_ll = [[0.0_f64; 81]; DOMAIN_AXES];
    let mut ax_xl = [[[0.0_f64; 9]; 3]; DOMAIN_AXES];
    ax_l[0][0] = 1.0;
    ax_x[1..6].copy_from_slice(&path.x);
    ax_xx[1..6].copy_from_slice(&path.xx);
    ax_l[1..6].copy_from_slice(&path.p);
    ax_ll[1..6].copy_from_slice(&path.pp);
    ax_xl[1..6].copy_from_slice(&path.xl);
    ax_x[6] = w_x;
    ax_xx[6] = w_xx;
    let gradient = penalty.gradient;
    let hessian = penalty.hessian;
    for a in 0..DOMAIN_AXES {
        let first = gradient[a];
        if first != 0.0 {
            for i in 0..3 {
                f_x[3][i] += first * ax_x[a][i];
                for j in 0..3 {
                    f_xx[3][i][j] += first * ax_xx[a][i][j];
                }
                for j in 0..9 {
                    f_xl[3][i][j] += first * ax_xl[a][i][j];
                }
            }
            for j in 0..9 {
                f_l[3][j] += first * ax_l[a][j];
                for k in 0..9 {
                    f_ll[3][j][k] += first * ax_ll[a][j * 9 + k];
                }
            }
        }
        for b in 0..DOMAIN_AXES {
            let second = hessian[a][b];
            if second == 0.0 {
                continue;
            }
            for i in 0..3 {
                for j in 0..3 {
                    f_xx[3][i][j] += second * ax_x[a][i] * ax_x[b][j];
                }
                for j in 0..9 {
                    f_xl[3][i][j] += second * ax_x[a][i] * ax_l[b][j];
                }
            }
            for j in 0..9 {
                for k in 0..9 {
                    f_ll[3][j][k] += second * ax_l[a][j] * ax_l[b][k];
                }
            }
        }
    }

    let mut x_map = XMap {
        exposure: Vec::new(),
        wb_r: Vec::new(),
        wb_b: Vec::new(),
        delta: [None; 6],
        x_full: [xyz.x, xyz.y, xyz.z],
        x_r: [
            image.transform.0[0][0] * cam[0],
            image.transform.0[1][0] * cam[0],
            image.transform.0[2][0] * cam[0],
        ],
        x_b: [
            image.transform.0[0][2] * cam[2],
            image.transform.0[1][2] * cam[2],
            image.transform.0[2][2] * cam[2],
        ],
        x_delta: [[0.0_f64; 3]; 6],
        x_r_delta: [[0.0_f64; 3]; 6],
        x_b_delta: [[0.0_f64; 3]; 6],
    };
    for index in [columns.image, columns.mode, columns.exposure_extra]
        .into_iter()
        .flatten()
    {
        x_map.exposure.push((index as u32, LN_2));
    }
    for index in [columns.wb_r, columns.wb_r_extra].into_iter().flatten() {
        x_map.wb_r.push((index as u32, 1.0));
    }
    for index in [columns.wb_b, columns.wb_b_extra].into_iter().flatten() {
        x_map.wb_b.push((index as u32, 1.0));
    }
    for term in terms {
        match term.component {
            ResponseComponent::WbR => x_map
                .wb_r
                .push(((layout.response.start + term.index) as u32, term.phi)),
            ResponseComponent::WbB => x_map
                .wb_b
                .push(((layout.response.start + term.index) as u32, term.phi)),
            _ => {}
        }
    }
    if let Some(indices) = columns.delta {
        x_map.delta = indices.map(|index| Some(index as u32));
    }
    for slot in 0..6 {
        let basis_matrix = looklab_body::Delta6::basis(slot);
        x_map.x_delta[slot] = image.a0.mul_vec(basis_matrix.mul_vec(cam));
        x_map.x_r_delta[slot] = image.a0.mul_vec(basis_matrix.mul_vec([cam[0], 0.0, 0.0]));
        x_map.x_b_delta[slot] = image.a0.mul_vec(basis_matrix.mul_vec([0.0, 0.0, cam[2]]));
    }

    let clip_second = match columns.clip_geometry {
        ClipGeometry::Logistic => clip_slope * (1.0 - 2.0 * (clip_k - 0.005) / 0.095),
        ClipGeometry::Linear => 0.0,
    };
    let mut clip_group = Vec::new();
    if let Some(index) = columns.clip {
        clip_group.push((index as u32, clip_slope, clip_second));
    }
    if let Some(index) = columns.clip_extra {
        clip_group.push((index as u32, clip_slope, clip_second));
    }

    let finite = f_x
        .iter()
        .flatten()
        .chain(f_l.iter().flatten())
        .chain(f_k.iter())
        .chain(f_xx.iter().flatten().flatten())
        .chain(f_xl.iter().flatten().flatten())
        .chain(f_xk.iter().flatten())
        .chain(f_ll.iter().flatten().flatten())
        .chain(f_lk.iter().flatten())
        .chain(f_kk.iter())
        .all(|value| value.is_finite());
    if !finite {
        return Err(CurvatureError::NonFinite);
    }

    Ok(PredictionCurvature {
        data: SampleData {
            l_x: layer.l_x,
            l_xx: layer.l_xx,
            active: layer.active,
            tint_a_curv: layer.tint_a_curv,
            tint_b_curv: layer.tint_b_curv,
            x_map,
            clip: clip_group,
            total: layout.total,
            error: None,
        },
        f_x,
        f_l,
        f_k,
        f_xx,
        f_xl,
        f_xk,
        f_ll,
        f_lk,
        f_kk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Params, softplus_slope};
    use crate::predict::{Jac, evaluate_sample};
    use looklab_color::Mat3;
    use looklab_color::ucs::Ucs;
    use looklab_model::realization::Delta;

    fn effective_look(params: &Params, layout: &Layout, terms: &[ResponseTerm]) -> Look {
        let base = params.look();
        if terms.is_empty() {
            return base;
        }
        let mut delta = Delta::default();
        for term in terms {
            if let Some(layer) = term.component.layer() {
                let value = params.theta[layout.response.start + term.index];
                delta.add(layer, term.feature, value * term.phi);
            }
        }
        delta.apply(&base).unwrap_or(base)
    }

    fn context(params: &Params, layout: &Layout, terms: &[ResponseTerm]) -> ImageCtx {
        let image = params.image(0);
        let mut log_r = image[1];
        let mut log_b = image[2];
        for term in terms {
            let value = params.theta[layout.response.start + term.index] * term.phi;
            match term.component {
                ResponseComponent::WbR => log_r += value,
                ResponseComponent::WbB => log_b += value,
                _ => {}
            }
        }
        ImageCtx {
            transform: Mat3::IDENTITY,
            a0: Mat3::IDENTITY,
            space: RgbSpace::Srgb,
            exposure_scale: libm::pow(2.0, image[0]),
            wb_r: libm::exp(log_r),
            wb_b: libm::exp(log_b),
            tone_clamp: [ToneClamp::Interior; TONE_COEFS],
        }
    }

    fn slopes(params: &Params) -> [f64; TONE_COEFS] {
        let mut out = [0.0_f64; TONE_COEFS];
        for (k, value) in out.iter_mut().enumerate() {
            *value = softplus_slope(params.theta[params.layout.tone_beta.start + k]);
        }
        out
    }

    #[derive(Clone, Copy)]
    struct Adjoints {
        residual: [f64; 3],
        penalty: f64,
    }

    fn first_gradient(
        params: &Params,
        layout: &Layout,
        terms: &[ResponseTerm],
        columns: &ColumnMap,
        rgb: [f64; 3],
        target: &Target,
        adjoint: Adjoints,
    ) -> (Vec<f64>, Vec<f64>) {
        let look = effective_look(params, layout, terms);
        let context = context(params, layout, terms);
        let clip_k = params.clip_k();
        let clip_slope = params.clip_slope();
        let slopes = slopes(params);
        let mut jac = Jac::default();
        let out = evaluate_sample(
            &look, layout, &context, columns, rgb, clip_k, clip_slope, &slopes, terms, target,
            &mut jac,
        );
        let penalty = out
            .penalty
            .expect("the fixture stays in the inverse domain");
        let mut gradient = vec![0.0_f64; layout.total];
        let mut scales = vec![0.0_f64; layout.total];
        for (index, column) in jac.cols.iter().enumerate() {
            let mut axis = 0.0_f64;
            let mut scale = 0.0_f64;
            for slot in 0..DOMAIN_AXES {
                let term = penalty.gradient[slot] * out.axis_terms[index][slot];
                axis += term;
                scale += (adjoint.penalty * term).abs();
            }
            let mut value = adjoint.penalty * axis;
            for channel in 0..3 {
                let term = adjoint.residual[channel] * jac.derivs[index][channel];
                value += term;
                scale += term.abs();
            }
            gradient[*column as usize] += value;
            scales[*column as usize] += scale;
        }
        (gradient, scales)
    }

    fn perturb(params: &Params, direction: &[f64], epsilon: f64) -> Params {
        let mut out = params.clone();
        for (value, delta) in out.theta.iter_mut().zip(direction.iter()) {
            *value += epsilon * delta;
        }
        out
    }

    const VALUE_LADDER: [f64; 11] = [
        8e-3, 4e-3, 2e-3, 1e-3, 5e-4, 2.5e-4, 1.25e-4, 6.25e-5, 3.125e-5, 1.5625e-5, 7.8125e-6,
    ];
    const HVP_LADDER: [f64; 13] = [
        8e-3,
        4e-3,
        2e-3,
        1e-3,
        5e-4,
        2.5e-4,
        1.25e-4,
        6.25e-5,
        3.125e-5,
        1.5625e-5,
        7.8125e-6,
        3.90625e-6,
        1.953125e-6,
    ];
    const RTOL: f64 = 1e-4;
    const ERROR_FLOOR: f64 = 1e-6;
    const ROUNDOFF_FACTOR: f64 = 64.0;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Ordinary,
        NeutralZeroTint,
        NeutralTint,
        Blend,
    }

    impl Kind {
        fn label(self) -> &'static str {
            match self {
                Self::Ordinary => "ordinary chromatic",
                Self::NeutralZeroTint => "D65 neutral zero tint",
                Self::NeutralTint => "D65 neutral nonzero tint",
                Self::Blend => "blend transition",
            }
        }
    }

    fn terms() -> Vec<ResponseTerm> {
        vec![
            ResponseTerm {
                index: 0,
                phi: 0.1,
                component: ResponseComponent::WbR,
                feature: 0,
            },
            ResponseTerm {
                index: 1,
                phi: 0.2,
                component: ResponseComponent::Tone,
                feature: 3,
            },
            ResponseTerm {
                index: 2,
                phi: 0.15,
                component: ResponseComponent::Tint,
                feature: 2,
            },
            ResponseTerm {
                index: 3,
                phi: 0.12,
                component: ResponseComponent::Chroma,
                feature: 1,
            },
        ]
    }

    struct Case {
        label: &'static str,
        layout: Layout,
        params: Params,
        terms: Vec<ResponseTerm>,
        columns: ColumnMap,
        rgb: [f64; 3],
        target: Target,
    }

    impl Case {
        fn new(kind: Kind) -> Self {
            let layout = Layout::with_response(1, 0, 1, 4);
            let image = layout.image(0);
            let columns = ColumnMap {
                image: Some(image.start),
                mode: None,
                wb_r: Some(image.start + 1),
                wb_b: Some(image.start + 2),
                clip: Some(layout.clip),
                exposure_extra: None,
                wb_r_extra: None,
                wb_b_extra: None,
                clip_extra: None,
                delta: None,
                second: false,
                curvature: false,
                clip_geometry: ClipGeometry::Logistic,
            };
            let mut params = Params::initial(layout.clone(), 1);
            let mut state = 0x9e37_79b9_7f4a_7c15_u64;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 11) as f64 / (1u64 << 53) as f64
            };
            for index in 0..layout.total {
                params.theta[index] += 0.05 * (next() - 0.5);
            }
            params.theta[layout.response.start + 1] = 1.0;
            let neutral = matches!(
                kind,
                Kind::NeutralZeroTint | Kind::NeutralTint | Kind::Blend
            );
            if neutral {
                params.theta[image.start] = 0.0;
                params.theta[image.start + 1] = 0.0;
                params.theta[image.start + 2] = 0.0;
                params.theta[layout.response.start] = 0.0;
                params.theta[layout.response.start + 2] = if kind == Kind::NeutralZeroTint {
                    0.0
                } else {
                    1.0
                };
                params.theta[layout.response.start + 3] = 0.0;
                for range in [layout.g.clone(), layout.r.clone(), layout.lambda.clone()] {
                    for index in range {
                        params.theta[index] = 0.0;
                    }
                }
                for index in layout.tint_a.clone() {
                    params.theta[index] = if kind == Kind::NeutralZeroTint {
                        0.0
                    } else {
                        0.5
                    };
                }
                for index in layout.tint_b.clone() {
                    params.theta[index] = if kind == Kind::NeutralZeroTint {
                        0.0
                    } else {
                        0.2
                    };
                }
                for index in layout.residual.clone() {
                    params.theta[index] = 0.0;
                }
            }
            let terms = terms();
            let white = looklab_model::solve::reference_white();
            let rgb = match kind {
                Kind::Ordinary => [0.31, 0.42, 0.12],
                Kind::NeutralZeroTint | Kind::NeutralTint => {
                    [white.x * 0.18, white.y * 0.18, white.z * 0.18]
                }
                Kind::Blend => [white.x * 0.18 * 1.003, white.y * 0.18, white.z * 0.18],
            };
            let weight = LookInput::new(Xyz::from_array(rgb)).blend.w;
            match kind {
                Kind::NeutralZeroTint | Kind::NeutralTint => assert_eq!(
                    weight, 0.0,
                    "the neutral fixture must be the verified D65 white at Y = 0.18"
                ),
                Kind::Blend => assert!(
                    weight > 0.0 && weight < 1.0,
                    "the blend fixture weight is {weight}"
                ),
                Kind::Ordinary => assert!(
                    weight > 0.0,
                    "the ordinary fixture must stay on the outer path"
                ),
            }
            Self {
                label: kind.label(),
                layout,
                params,
                terms,
                columns,
                rgb,
                target: Target::new(Ucs::new(52.0, 4.0, -3.0)),
            }
        }

        fn value(&self, params: &Params, residual_adjoint: [f64; 3], penalty_adjoint: f64) -> f64 {
            let look = effective_look(params, &self.layout, &self.terms);
            let context = context(params, &self.layout, &self.terms);
            let mut jac = Jac::default();
            let out = evaluate_sample(
                &look,
                &self.layout,
                &context,
                &self.columns,
                self.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(params),
                &self.terms,
                &self.target,
                &mut jac,
            );
            let penalty = out
                .penalty
                .expect("the fixture stays in the inverse domain");
            dot3(residual_adjoint, out.residual) + penalty_adjoint * penalty.value
        }

        fn gradient(
            &self,
            params: &Params,
            residual_adjoint: [f64; 3],
            penalty_adjoint: f64,
        ) -> (Vec<f64>, Vec<f64>) {
            first_gradient(
                params,
                &self.layout,
                &self.terms,
                &self.columns,
                self.rgb,
                &self.target,
                Adjoints {
                    residual: residual_adjoint,
                    penalty: penalty_adjoint,
                },
            )
        }

        fn hvp(
            &self,
            params: &Params,
            residual_adjoint: [f64; 3],
            penalty_adjoint: f64,
            direction: &[f64],
        ) -> Vec<f64> {
            let look = effective_look(params, &self.layout, &self.terms);
            let context = context(params, &self.layout, &self.terms);
            let mut jac = Jac::default();
            let out = evaluate_sample(
                &look,
                &self.layout,
                &context,
                &ColumnMap {
                    curvature: true,
                    ..self.columns
                },
                self.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(params),
                &self.terms,
                &self.target,
                &mut jac,
            );
            let Some(curvature) = out.curvature else {
                panic!("{}: curvature missing: {:?}", self.label, out.violation);
            };
            let sample = curvature
                .contract(residual_adjoint, penalty_adjoint)
                .expect("contract");
            let mut product = vec![0.0_f64; self.layout.total];
            let mut scratch = CurvatureScratch::new();
            sample
                .prepare_scratch(&mut scratch)
                .expect("prepare_scratch");
            sample
                .add_hvp(direction, &mut product, &mut scratch)
                .expect("hvp");
            product
        }

        fn blend(&self, params: &Params) -> f64 {
            let context = context(params, &self.layout, &self.terms);
            let cam = [
                self.rgb[0] * context.wb_r * context.exposure_scale,
                self.rgb[1] * context.exposure_scale,
                self.rgb[2] * context.wb_b * context.exposure_scale,
            ];
            let xyz = context.transform.mul_vec(cam);
            LookInput::new(Xyz::from_array(xyz)).blend.w
        }

        fn check_step(
            &self,
            params: &Params,
            label: &str,
        ) -> looklab_model::domain_penalty::PenaltyDerivatives {
            let look = effective_look(params, &self.layout, &self.terms);
            let context = context(params, &self.layout, &self.terms);
            let mut jac = Jac::default();
            let out = evaluate_sample(
                &look,
                &self.layout,
                &context,
                &self.columns,
                self.rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(params),
                &self.terms,
                &self.target,
                &mut jac,
            );
            let blend = self.blend(params);
            assert!(
                out.penalty.is_some() && out.violation.is_none(),
                "{label}: infeasible sample violation {:?} blend {blend:.17e}",
                out.violation
            );
            assert!(
                blend.is_finite() && (0.0..=1.0).contains(&blend),
                "{label}: invalid blend weight {blend:.17e}"
            );
            out.penalty
                .expect("the checked sample has a feasible penalty")
        }
    }

    fn direction(layout: &Layout, seed: u64) -> Vec<f64> {
        let mut state = seed;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..layout.total).map(|_| 0.05 * (next() - 0.5)).collect()
    }

    fn coordinate_direction(layout: &Layout, index: usize) -> Vec<f64> {
        let mut out = vec![0.0_f64; layout.total];
        out[index] = 1.0;
        out
    }

    #[derive(Clone, Debug)]
    struct Resolved {
        step: f64,
        order: f64,
        estimate: f64,
        uncertainty: f64,
    }

    fn resolve(values: &[f64], steps: &[f64], scale: f64) -> Option<Resolved> {
        let roundoff = |step: f64| ROUNDOFF_FACTOR * f64::EPSILON * scale / step;
        if values.iter().all(|value| *value == 0.0) {
            return Some(Resolved {
                step: steps[0],
                order: 2.0,
                estimate: 0.0,
                uncertainty: roundoff(steps[0]),
            });
        }
        let floor = RTOL * ERROR_FLOOR;
        let magnitude = values
            .iter()
            .fold(0.0_f64, |largest, value| largest.max(value.abs()));
        if magnitude <= floor {
            return Some(Resolved {
                step: steps[0],
                order: 2.0,
                estimate: 0.0,
                uncertainty: magnitude + roundoff(steps[0]),
            });
        }
        let per_level_roundoff: Vec<f64> = steps.iter().map(|step| roundoff(*step)).collect();
        let mut best: Option<Resolved> = None;
        for start in 0..values.len() {
            for end in start + 2..values.len() {
                let estimate = values[end];
                let mut max_deviation = 0.0_f64;
                let mut consistent = true;
                for i in start..=end {
                    let deviation = (values[i] - estimate).abs();
                    max_deviation = max_deviation.max(deviation);
                    if deviation > per_level_roundoff[i] + per_level_roundoff[end] {
                        consistent = false;
                        break;
                    }
                }
                if !consistent {
                    continue;
                }
                let uncertainty = per_level_roundoff[end] + max_deviation;
                let candidate = Resolved {
                    step: steps[end],
                    order: 2.0,
                    estimate,
                    uncertainty,
                };
                if best
                    .as_ref()
                    .is_none_or(|current| uncertainty < current.uncertainty)
                {
                    best = Some(candidate);
                }
            }
        }
        for index in 1..values.len().saturating_sub(1) {
            let coarse = values[index - 1] - values[index];
            let fine = values[index] - values[index + 1];
            let local_roundoff = roundoff(steps[index + 1]);
            if fine.abs() <= local_roundoff || coarse.abs() <= local_roundoff {
                continue;
            }
            let order = libm::log2(coarse.abs() / fine.abs());
            if !(0.2..=3.0).contains(&order) {
                continue;
            }
            let denominator = libm::pow(2.0, order) - 1.0;
            let estimate = values[index + 1] - fine / denominator;
            let ratio = steps[index + 1] / steps[index];
            let roundoff_estimate = local_roundoff * (1.0 + (1.0 + ratio) / denominator);
            if index + 2 >= values.len() {
                continue;
            }
            let coarse_next = values[index] - values[index + 1];
            let fine_next = values[index + 1] - values[index + 2];
            let next_roundoff = roundoff(steps[index + 2]);
            if fine_next.abs() <= next_roundoff || coarse_next.abs() <= next_roundoff {
                continue;
            }
            let next_order = libm::log2(coarse_next.abs() / fine_next.abs());
            if !(0.2..=3.0).contains(&next_order) {
                continue;
            }
            let next_denominator = libm::pow(2.0, next_order) - 1.0;
            let next = values[index + 2] - fine_next / next_denominator;
            let uncertainty = (estimate - next).abs() + roundoff_estimate;
            let candidate = Resolved {
                step: steps[index + 1],
                order,
                estimate,
                uncertainty,
            };
            if best
                .as_ref()
                .is_none_or(|current| uncertainty < current.uncertainty)
            {
                best = Some(candidate);
            }
        }
        best
    }

    #[derive(Clone, Debug)]
    struct Record {
        label: String,
        step: f64,
        order: f64,
        analytic: f64,
        estimate: f64,
        absolute: f64,
        normalized: f64,
        uncertainty: f64,
    }

    impl core::fmt::Display for Record {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(
                formatter,
                "{}: step {:.17e} order {:.17e} analytic {:.17e} estimate {:.17e} absolute {:.17e} normalized {:.17e} uncertainty {:.17e}",
                self.label,
                self.step,
                self.order,
                self.analytic,
                self.estimate,
                self.absolute,
                self.normalized,
                self.uncertainty,
            )
        }
    }

    #[derive(Default)]
    struct Report {
        worst_normalized: Option<Record>,
        worst_absolute: Option<Record>,
        unresolved: Vec<String>,
    }

    impl Report {
        fn push(&mut self, record: Record) {
            if self
                .worst_normalized
                .as_ref()
                .is_none_or(|worst| record.normalized > worst.normalized)
            {
                self.worst_normalized = Some(record.clone());
            }
            if self
                .worst_absolute
                .as_ref()
                .is_none_or(|worst| record.absolute > worst.absolute)
            {
                self.worst_absolute = Some(record);
            }
        }

        fn check(&self, label: &str) {
            let normalized = self
                .worst_normalized
                .as_ref()
                .expect("at least one checked coordinate");
            let absolute = self
                .worst_absolute
                .as_ref()
                .expect("at least one checked coordinate");
            assert!(
                self.unresolved.is_empty() && normalized.normalized <= 1.0,
                "{label}: unresolved {:?}; worst normalized {normalized}; worst absolute {absolute}",
                self.unresolved
            );
            eprintln!("{label}: worst normalized {normalized}; worst absolute {absolute}");
        }
    }

    fn check_sequence(
        report: &mut Report,
        label: &str,
        analytic: f64,
        values: &[f64],
        steps: &[f64],
        scale: f64,
    ) {
        let Some(resolved) = resolve(values, steps, scale) else {
            report.unresolved.push(format!(
                "{label}: no resolved window; analytic {analytic:.9e} values {values:?}"
            ));
            return;
        };
        let accuracy = RTOL * analytic.abs().max(resolved.estimate.abs()).max(ERROR_FLOOR);
        let absolute = (resolved.estimate - analytic).abs();
        let record = Record {
            label: label.to_string(),
            step: resolved.step,
            order: resolved.order,
            analytic,
            estimate: resolved.estimate,
            absolute,
            normalized: absolute / accuracy,
            uncertainty: resolved.uncertainty,
        };
        if resolved.uncertainty > accuracy {
            report.unresolved.push(format!(
                "{record}: uncertainty {:.3e} > accuracy {accuracy:.3e}",
                resolved.uncertainty
            ));
        } else {
            report.push(record);
        }
    }

    fn check_case(
        case: &Case,
        residual_adjoint: [f64; 3],
        penalty_adjoint: f64,
        label: &str,
        report: &mut Report,
    ) {
        let (base_gradient, _) = case.gradient(&case.params, residual_adjoint, penalty_adjoint);
        let directions = [
            ("mixed", direction(&case.layout, 0x51ed_270b_9a3c_4d1f)),
            (
                "coordinate",
                coordinate_direction(&case.layout, case.layout.tint_a.start),
            ),
        ];
        for (direction_label, direction) in directions {
            let analytic: f64 = base_gradient
                .iter()
                .zip(direction.iter())
                .map(|(left, right)| left * right)
                .sum();
            let mut values = Vec::with_capacity(VALUE_LADDER.len());
            let mut value_scale = 0.0_f64;
            for &step in &VALUE_LADDER {
                let plus = perturb(&case.params, &direction, step);
                let minus = perturb(&case.params, &direction, -step);
                case.check_step(
                    &plus,
                    &format!("{label} {direction_label} value->gradient plus h {step:e}"),
                );
                case.check_step(
                    &minus,
                    &format!("{label} {direction_label} value->gradient minus h {step:e}"),
                );
                let value_plus = case.value(&plus, residual_adjoint, penalty_adjoint);
                let value_minus = case.value(&minus, residual_adjoint, penalty_adjoint);
                value_scale = value_scale.max(value_plus.abs()).max(value_minus.abs());
                values.push((value_plus - value_minus) / (2.0 * step));
            }
            check_sequence(
                report,
                &format!("{label} {direction_label} value->gradient"),
                analytic,
                &values,
                &VALUE_LADDER,
                value_scale,
            );

            let product = case.hvp(&case.params, residual_adjoint, penalty_adjoint, &direction);
            let mut gradient_values = vec![Vec::with_capacity(HVP_LADDER.len()); case.layout.total];
            let mut gradient_scales = vec![0.0_f64; case.layout.total];
            for &step in &HVP_LADDER {
                let plus = perturb(&case.params, &direction, step);
                let minus = perturb(&case.params, &direction, -step);
                case.check_step(
                    &plus,
                    &format!("{label} {direction_label} gradient->hvp plus h {step:e}"),
                );
                case.check_step(
                    &minus,
                    &format!("{label} {direction_label} gradient->hvp minus h {step:e}"),
                );
                let (gradient_plus, scales_plus) =
                    case.gradient(&plus, residual_adjoint, penalty_adjoint);
                let (gradient_minus, scales_minus) =
                    case.gradient(&minus, residual_adjoint, penalty_adjoint);
                for index in 0..case.layout.total {
                    gradient_scales[index] = gradient_scales[index]
                        .max(scales_plus[index])
                        .max(scales_minus[index]);
                    gradient_values[index]
                        .push((gradient_plus[index] - gradient_minus[index]) / (2.0 * step));
                }
            }
            for index in 0..case.layout.total {
                check_sequence(
                    report,
                    &format!("{label} {direction_label} gradient->hvp coordinate {index}"),
                    product[index],
                    &gradient_values[index],
                    &HVP_LADDER,
                    gradient_scales[index],
                );
            }
        }
    }

    fn check_contracts(kind: Kind) {
        let case = Case::new(kind);
        let base = case.check_step(&case.params, case.label);
        eprintln!(
            "{}: input blend {:.17e}, minimum inverse margin {:.17e}",
            case.label,
            case.blend(&case.params),
            base.minimum_margin,
        );
        let mut report = Report::default();
        for (label, residual_adjoint, penalty_adjoint) in [
            ("residual", [0.7, -0.4, 0.2], 0.0),
            ("barrier", [0.0; 3], 0.9),
            ("combined", [0.7, -0.4, 0.2], 0.9),
        ] {
            check_case(
                &case,
                residual_adjoint,
                penalty_adjoint,
                &format!("{} {label}", case.label),
                &mut report,
            );
        }
        report.check(case.label);
    }

    #[test]
    fn ordinary_chromatic_scalar_contracts_hold() {
        check_contracts(Kind::Ordinary);
    }

    #[test]
    fn d65_neutral_zero_tint_scalar_contracts_hold() {
        check_contracts(Kind::NeutralZeroTint);
    }

    #[test]
    fn d65_neutral_nonzero_tint_scalar_contracts_hold() {
        check_contracts(Kind::NeutralTint);
    }

    #[test]
    fn blend_join_scalar_contracts_hold() {
        check_contracts(Kind::Blend);
    }

    #[test]
    fn richardson_estimator_resolves_known_limits_and_keeps_zero_roundoff() {
        let steps = HVP_LADDER;
        for (order, coefficient, limit) in [(0.9_f64, 3.0_f64, 2.5_f64), (2.0, -1.5, -0.75)] {
            let values: Vec<f64> = steps
                .iter()
                .map(|step| limit + coefficient * libm::pow(*step, order))
                .collect();
            let resolved = resolve(&values, &steps, 1.0).expect("known limit resolves");
            assert!(
                (resolved.estimate - limit).abs() <= 1e-9,
                "order {order}: estimate {:.17e} limit {limit:.17e}",
                resolved.estimate
            );
            assert!(
                resolved.uncertainty <= 1e-7,
                "order {order}: uncertainty {:.3e}",
                resolved.uncertainty
            );
        }
        let zeros = vec![0.0_f64; HVP_LADDER.len()];
        let resolved = resolve(&zeros, &HVP_LADDER, 10.0).expect("zero ladder resolves");
        assert_eq!(resolved.estimate, 0.0);
        assert!(
            resolved.uncertainty > 0.0,
            "the zero ladder dropped the supplied roundoff bound"
        );

        let sub_floor: Vec<f64> = HVP_LADDER
            .iter()
            .map(|step| 1e-20 * libm::pow(*step / HVP_LADDER[0], 5.45))
            .collect();
        let resolved = resolve(&sub_floor, &HVP_LADDER, 1.0).expect("sub-floor resolves");
        assert_eq!(resolved.estimate, 0.0);
        assert!(
            resolved.uncertainty <= RTOL * ERROR_FLOOR,
            "sub-floor uncertainty {:.3e} exceeds the absolute floor",
            resolved.uncertainty
        );

        let constant = vec![2.5_f64; HVP_LADDER.len()];
        let resolved = resolve(&constant, &HVP_LADDER, 1.0).expect("constant resolves");
        assert_eq!(resolved.estimate, 2.5);
        assert!(
            resolved.uncertainty <= 1e-10,
            "constant uncertainty {:.3e} does not use the per-level plateau",
            resolved.uncertainty
        );

        let truth = 1.25_f64;
        let values: Vec<f64> = HVP_LADDER
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let noise =
                    ((index % 3) as f64 - 1.0) * 0.5 * (ROUNDOFF_FACTOR * f64::EPSILON / *step);
                truth + 0.5 * (*step) * (*step) + noise
            })
            .collect();
        let resolved =
            resolve(&values, &HVP_LADDER, 1.0).expect("truncation with growing roundoff resolves");
        assert!(
            (resolved.estimate - truth).abs() <= 1e-7,
            "truncation/growing-roundoff estimate {:.17e} truth {truth:.17e}",
            resolved.estimate
        );
        assert!(
            resolved.uncertainty <= 1e-6,
            "truncation/growing-roundoff uncertainty {:.3e}",
            resolved.uncertainty
        );

        let limit = 1.25_f64;
        let k = (1.0 - libm::pow(2.0, -0.9)) / (1.0 - 0.25);
        let two_sample: Vec<f64> = HVP_LADDER
            .iter()
            .map(|step| {
                let t = *step / HVP_LADDER[0];
                limit + libm::pow(t, 0.9) - k * t * t
            })
            .collect();
        let tolerance = RTOL * limit.abs().max(ERROR_FLOOR);
        if let Some(resolved) = resolve(&two_sample, &HVP_LADDER, 1.0) {
            assert!(
                resolved.uncertainty > tolerance || (resolved.estimate - limit).abs() <= tolerance,
                "two-sample plateau accepted: {resolved:?}"
            );
        }
    }

    #[test]
    fn clipped_crossing_uses_positive_data_gn_and_returned_curvature() {
        let (layout, params, columns, rgb, target, _) =
            crate::predict::tests::fitter_crossing(RgbSpace::Srgb, 0.05, 8.0);
        let look = params.look();
        let context = context(&params, &layout, &[]);
        let evaluate = || {
            let mut jac = Jac::default();
            let out = evaluate_sample(
                &look,
                &layout,
                &context,
                &ColumnMap {
                    curvature: true,
                    ..columns
                },
                rgb,
                params.clip_k(),
                params.clip_slope(),
                &slopes(&params),
                &[],
                &target,
                &mut jac,
            );
            (out, jac)
        };
        let (out, jac) = evaluate();
        assert!(out.crossing, "the exact fixture is not the crossing branch");
        let mut directions = vec![
            ("mixed", direction(&layout, 0x1234_5678_9abc_def0)),
            (
                "tint unit",
                coordinate_direction(&layout, layout.tint_a.start),
            ),
        ];
        if let Some(first) = jac.cols.first() {
            directions.push((
                "active column",
                coordinate_direction(&layout, *first as usize),
            ));
        }
        let mut nonvacuous = false;
        for (label, direction) in directions {
            let mut projected = [0.0_f64; 3];
            for (position, column) in jac.cols.iter().enumerate() {
                let value = direction[*column as usize];
                for channel in 0..3 {
                    projected[channel] += jac.derivs[position][channel] * value;
                }
            }
            let quadratic = dot3(projected, projected);
            assert!(
                quadratic.is_finite() && quadratic >= 0.0,
                "{label} direction {direction:?}: DATA quadratic {quadratic:.9e} is not finite nonnegative"
            );
            if quadratic > 0.0 {
                nonvacuous = true;
            }
        }
        assert!(
            nonvacuous,
            "no nonvacuous active direction in the DATA Gauss-Newton model; columns {:?}",
            jac.cols
        );
        let curvature = out.curvature.expect("the exact crossing returns curvature");
        let direction = direction(&layout, 0x0bad_c0de_1234_5678);
        let sample = curvature
            .contract([1.0, -0.5, 0.25], 0.0)
            .expect("contract");
        let mut product = vec![0.0_f64; layout.total];
        let mut scratch = CurvatureScratch::new();
        sample
            .prepare_scratch(&mut scratch)
            .expect("prepare_scratch");
        sample
            .add_hvp(&direction, &mut product, &mut scratch)
            .expect("hvp");
        assert!(
            product.iter().all(|value| *value == 0.0),
            "the crossing row retains residual second curvature: {product:?}"
        );
        let (out, _) = evaluate();
        let curvature = out.curvature.expect("the exact crossing returns curvature");
        let sample = curvature.contract([0.0; 3], 0.9).expect("contract");
        let mut penalty_product = vec![0.0_f64; layout.total];
        let mut scratch = CurvatureScratch::new();
        sample
            .prepare_scratch(&mut scratch)
            .expect("prepare_scratch");
        sample
            .add_hvp(&direction, &mut penalty_product, &mut scratch)
            .expect("hvp");
        assert!(
            penalty_product.iter().any(|value| value.abs() > 1e-12),
            "the crossing row dropped the exact penalty curvature"
        );
    }

    #[test]
    fn soft_clip_second_derivatives_match_differences() {
        for k in [0.01_f64, 0.03, 0.08] {
            for x in [k - 0.05, k + 0.001, 0.5, 1.0 - k - 0.001, 1.0 - k + 0.05] {
                let derivatives = clip::soft_clip_derivatives(x, k);
                let step = 1e-6;
                let d_plus = clip::soft_clip_derivatives(x + step, k).first;
                let d_minus = clip::soft_clip_derivatives(x - step, k).first;
                let numerical = (d_plus - d_minus) / (2.0 * step);
                let scale = numerical.abs().max(1e-4);
                assert!(
                    (derivatives.second - numerical).abs() / scale < 1e-4,
                    "d2 x={x} k={k}: {:.6e} vs {numerical:.6e}",
                    derivatives.second
                );
                let k_plus = clip::soft_clip_derivatives(x, k + step).first_k;
                let k_minus = clip::soft_clip_derivatives(x, k - step).first_k;
                let numerical = (k_plus - k_minus) / (2.0 * step);
                let scale = numerical.abs().max(1e-4);
                assert!(
                    (derivatives.second_k - numerical).abs() / scale < 1e-4,
                    "dkk x={x} k={k}: {:.6e} vs {numerical:.6e}",
                    derivatives.second_k
                );
                let d_plus = clip::soft_clip_derivatives(x, k + step).first;
                let d_minus = clip::soft_clip_derivatives(x, k - step).first;
                let numerical = (d_plus - d_minus) / (2.0 * step);
                let scale = numerical.abs().max(1e-4);
                assert!(
                    (derivatives.mixed - numerical).abs() / scale < 1e-4,
                    "dxk x={x} k={k}: {:.6e} vs {numerical:.6e}",
                    derivatives.mixed
                );
            }
        }
    }
}
