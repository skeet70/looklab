// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compact regressions for the coupled positive-GN joint operator, its Schur solve and the
//! stationarity certificate, with a dense oracle assembled from the frozen pass data.

use super::tone_prior_tests::colorimetric_center;
use super::*;

const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

struct Fixture {
    layout: Layout,
    problem: Problem,
    options: FitOptions,
    params: Params,
    roles: Vec<Role>,
    lambdas: StageLambdas,
    blocks: Vec<BlockAcc>,
    penalties: Vec<Vec<f64>>,
    pass: PassOutput,
    mask: ResidualMask,
}

/// Keep the strongest residual node and one grid neighbour.
fn prune_residual_support(f: &mut Fixture) {
    let original = ResidualMask::from_samples(&f.pass.residual_samples);
    let mut keep: Vec<u32> = Vec::new();
    for sample in &f.pass.residual_samples {
        let mut candidates: Vec<(f64, u32)> = sample
            .nodes
            .iter()
            .zip(sample.weights.iter())
            .filter(|(node, weight)| **weight > 0.0 && original.free()[**node as usize * 3])
            .map(|(node, weight)| (*weight, *node))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_by(|left, right| right.0.total_cmp(&left.0));
        let first = candidates[0].1;
        keep.push(first);
        let adjacent = residual_neighbours(first as usize * 3)
            .into_iter()
            .map(|full| (full / 3) as u32)
            .find(|node| candidates.iter().any(|(_, candidate)| candidate == node));
        if let Some(node) = adjacent {
            keep.push(node);
        } else if let Some(&(_, second)) = candidates.get(1) {
            keep.push(second);
        }
        break;
    }
    for sample in &mut f.pass.residual_samples {
        for (slot, node) in sample.nodes.iter().enumerate() {
            if !keep.contains(node) {
                sample.weights[slot] = 0.0;
            }
        }
    }
    f.mask = ResidualMask::from_samples(&f.pass.residual_samples);
}

fn data_scale(pass: &PassOutput) -> f64 {
    pass.active_samples.max(1) as f64 / pass.hessian_rows.max(1) as f64
}

fn fixture(images: usize, samples: usize, identity_scale: f64, seed: u64) -> Fixture {
    let data = crate::synth::dataset(&crate::synth::SynthConfig {
        seed,
        images,
        samples_per_image: samples,
        noise_ucs: 0.5,
        ..crate::synth::SynthConfig::default()
    });
    let options = FitOptions {
        seed,
        hessian_samples: usize::MAX,
        run_gcv: false,
        identity_scale,
        ..FitOptions::default()
    };
    let problem = Problem::new_with(&data.images, None, &options).expect("problem");
    let layout = problem.layout.clone();
    let params = initial_params(&problem, &options);
    let roles = prior_roles(&problem, &layout);
    let lambdas = StageLambdas {
        tone: 0.7,
        residual: 0.4,
        ..StageLambdas::default()
    };
    let mut blocks = vec![
        make_joint_tone_block(&problem, ActiveColumns::ALL),
        make_block(&problem, BlockKind::Residual, ActiveColumns::ALL),
    ];
    let penalties: Vec<Vec<f64>> = blocks
        .iter()
        .map(|block| block_penalty(&problem, block))
        .collect();
    let pass = problem.pass_with_rows(
        &params,
        ActiveColumns::ALL,
        &mut blocks,
        false,
        false,
        false,
        IntentFilter::NativeOnly,
        true,
        true,
    );
    let mask = ResidualMask::from_samples(&pass.residual_samples);
    let mut fixture = Fixture {
        layout,
        problem,
        options,
        params,
        roles,
        lambdas,
        blocks,
        penalties,
        pass,
        mask,
    };
    prune_residual_support(&mut fixture);
    fixture
}

impl Fixture {
    fn data_scale(&self) -> f64 {
        data_scale(&self.pass)
    }

    fn model(&self) -> JointModel<'_> {
        JointModel::new(
            &self.layout,
            &self.problem,
            &self.params,
            &self.options,
            &self.blocks,
            &self.penalties,
            &self.lambdas,
            &self.roles,
            &self.pass,
            self.data_scale(),
            false,
            false,
            &self.mask,
        )
        .expect("joint model")
    }

    fn report(&self) -> StationarityReport {
        let mut warm = None;
        stationarity_bound(
            &self.layout,
            &self.problem,
            &self.params,
            &self.options,
            &self.blocks,
            &self.penalties,
            &self.lambdas,
            &self.roles,
            &self.pass,
            self.data_scale(),
            &mut warm,
            &self.mask,
            CertificateOrigin::FinalStage,
        )
    }
}

fn rebuild_pass(f: &mut Fixture) {
    for block in &mut f.blocks {
        block.reset();
    }
    f.pass = f.problem.pass_with_rows(
        &f.params,
        ActiveColumns::ALL,
        &mut f.blocks,
        false,
        false,
        false,
        IntentFilter::NativeOnly,
        true,
        true,
    );
    prune_residual_support(f);
}

fn perturb(f: &mut Fixture) {
    f.params.theta[f.layout.tone_c0] += 0.7;
    f.params.theta[f.layout.image(0).start] += 0.4;
    f.params.theta[f.layout.clip] += 0.2;
    rebuild_pass(f);
}

/// A frozen linearization with zero data curvature at the prior-only minimum.
fn prior_only(f: &mut Fixture) {
    for block in &mut f.blocks {
        block.reset();
    }
    f.pass.coupled_rows.clear();
    f.pass.residual_samples.clear();
    f.pass.residual_gradient.fill(0.0);
    let layout = &f.layout;
    let center = colorimetric_center(layout);
    let width = 1 + TONE_COEFS;
    let index = f
        .blocks
        .iter()
        .position(|block| block.kind == BlockKind::Tone)
        .expect("the fixture carries a tone block");
    let tone = {
        let block = &f.blocks[index];
        let penalty = &f.penalties[index];
        let n = block.len();
        let precision = layout.identity_scale * identity_weight(BlockKind::Tone);
        if precision > 0.0 {
            let mut matrix = vec![0.0_f64; width * width];
            let mut rhs = vec![0.0_f64; width];
            for row in 0..width {
                matrix[row * width + row] = precision;
                for column in 0..width {
                    matrix[row * width + column] += f.lambdas.tone * penalty[row * n + column];
                }
                rhs[row] = precision * center[block.params[row]];
            }
            dense_solve(&matrix, &rhs)
        } else {
            vec![0.0_f64; width]
        }
    };
    for (row, &global) in f.blocks[index].params[..width].iter().enumerate() {
        f.params.theta[global] = tone[row];
    }
    for index in 0..problem_image_count(layout) {
        let range = layout.image(index);
        f.params.theta[range.start] = f.problem.images[index].exposure_hint;
        f.params.theta[range.start + 1] = 0.0;
        f.params.theta[range.start + 2] = 0.0;
    }
    for index in layout.modes.clone() {
        f.params.theta[index] = 0.0;
    }
    f.params.theta[layout.clip] = zeta_from_clip(f.options.clip_prior.0);
    for index in layout.residual.clone() {
        f.params.theta[index] = 0.0;
    }
    f.mask = ResidualMask::from_samples(&f.pass.residual_samples);
}

/// A symmetric, diagonally dominant penalty over the seven domain axes.
fn synthetic_penalty() -> [[f64; DOMAIN_AXES]; DOMAIN_AXES] {
    let mut penalty = [[0.0_f64; DOMAIN_AXES]; DOMAIN_AXES];
    for (row, values) in penalty.iter_mut().enumerate() {
        for (column, value) in values.iter_mut().enumerate() {
            *value = if row == column {
                0.02 + 0.003 * row as f64
            } else {
                0.001 * ((row + column) % 5) as f64 - 0.002
            };
        }
    }
    penalty
}

fn inject_penalty(f: &mut Fixture) {
    let penalty = synthetic_penalty();
    for (index, row) in f.pass.coupled_rows.iter_mut().enumerate() {
        row.penalty = penalty;
        row.penalty_gn = penalty;
        for (position, (global, _)) in row.columns.iter().enumerate() {
            row.axis_terms[position] = core::array::from_fn(|axis| {
                0.002 * (((*global as i64 * 7 + index as i64 + axis as i64) % 11) - 5) as f64
            });
        }
        for (local, _, _, axis) in &mut row.residual_columns {
            *axis = core::array::from_fn(|slot| {
                0.002 * (((*local as i64 * 5 + index as i64 + slot as i64) % 7) - 3) as f64
            });
        }
    }
    let residual = [
        [0.03_f64, 0.002, -0.001],
        [0.002, 0.025, 0.001],
        [-0.001, 0.001, 0.028],
    ];
    for sample in &mut f.pass.residual_samples {
        sample.penalty_curvature = residual;
        sample.penalty_gn = residual;
    }
}

fn residual_neighbours(full: usize) -> Vec<usize> {
    let node = full / 3;
    let channel = full % 3;
    let x = node / (RESIDUAL_GRID * RESIDUAL_GRID);
    let y = (node / RESIDUAL_GRID) % RESIDUAL_GRID;
    let z = node % RESIDUAL_GRID;
    let at = |x: usize, y: usize, z: usize| {
        (x * RESIDUAL_GRID * RESIDUAL_GRID + y * RESIDUAL_GRID + z) * 3 + channel
    };
    let mut out = Vec::new();
    if x > 0 {
        out.push(at(x - 1, y, z));
    }
    if x + 1 < RESIDUAL_GRID {
        out.push(at(x + 1, y, z));
    }
    if y > 0 {
        out.push(at(x, y - 1, z));
    }
    if y + 1 < RESIDUAL_GRID {
        out.push(at(x, y + 1, z));
    }
    if z > 0 {
        out.push(at(x, y, z - 1));
    }
    if z + 1 < RESIDUAL_GRID {
        out.push(at(x, y, z + 1));
    }
    out
}

fn residual_laplacian(theta: &[f64], full: usize, diagonal: &[f64]) -> f64 {
    let mut value = diagonal[full] * theta[full];
    for neighbour in residual_neighbours(full) {
        value -= theta[neighbour];
    }
    value
}

fn residual_diagonal_from_samples(samples: &[ResidualSample], full: usize) -> f64 {
    let node = full / 3;
    let channel = full % 3;
    let mut sum = 0.0;
    for sample in samples {
        for (slot, candidate) in sample.nodes.iter().enumerate() {
            if *candidate as usize != node {
                continue;
            }
            let weight = sample.weights[slot];
            if weight == 0.0 {
                continue;
            }
            let mut clip = 0.0;
            for row in 0..3 {
                clip += sample.clip.0[row][channel] * sample.clip.0[row][channel];
            }
            sum += weight * weight * (sample.alpha * clip + sample.penalty_gn[channel][channel]);
        }
    }
    sum
}

fn expected_index(f: &Fixture, model: &JointModel<'_>) -> (Vec<usize>, Vec<usize>) {
    let mut seen = vec![false; f.layout.total];
    let mut outer = Vec::new();
    let mut border = Vec::new();
    let mut images = vec![Vec::new(); f.problem.images.len()];
    for block in &f.blocks {
        if block.kind == BlockKind::Residual {
            continue;
        }
        for &global in &block.params {
            if seen[global] {
                continue;
            }
            seen[global] = true;
            match f.roles[global] {
                Role::Exposure(image) | Role::Wb(image) => images[image].push(global),
                Role::Mode(_) | Role::Clip => border.push(global),
                Role::None => outer.push(global),
            }
        }
    }
    let mut globals = outer;
    globals.extend(border);
    for list in images {
        globals.extend(list);
    }
    let start = f.layout.residual.start;
    for (full, free) in f.mask.free().iter().enumerate() {
        if *free {
            globals.push(start + full);
        }
    }
    let mut sorted_expected = globals.clone();
    sorted_expected.sort_unstable();
    let mut sorted_actual = model.globals.clone();
    sorted_actual.sort_unstable();
    assert_eq!(
        sorted_expected, sorted_actual,
        "joint coordinates are not the free blocks plus the free residual coordinates"
    );
    assert_eq!(model.dim, globals.len());
    let mut index_of = vec![usize::MAX; f.layout.total];
    for (index, &global) in globals.iter().enumerate() {
        index_of[global] = index;
    }
    (globals, index_of)
}

struct DenseSystem {
    globals: Vec<usize>,
    index_of: Vec<usize>,
    matrix: Vec<f64>,
    rhs: Vec<f64>,
    damping_diagonal: Vec<f64>,
    floor: Vec<f64>,
}

impl DenseSystem {
    fn multiply(&self, v: &[f64]) -> Vec<f64> {
        let n = self.globals.len();
        let mut out = vec![0.0_f64; n];
        for row in 0..n {
            let mut sum = 0.0;
            for column in 0..n {
                sum += self.matrix[row * n + column] * v[column];
            }
            out[row] = sum;
        }
        out
    }
}

fn finite_difference_identity_gradient(
    layout: &Layout,
    block: &BlockAcc,
    theta: &[f64],
    global: usize,
) -> f64 {
    const STEP: f64 = 1e-6;
    let mut plus = theta.to_vec();
    let mut minus = theta.to_vec();
    plus[global] += STEP;
    minus[global] -= STEP;
    (identity_cost(layout, block, &plus) - identity_cost(layout, block, &minus)) / (2.0 * STEP)
}

/// Assemble `B + damping·diag(data)` and the declared right-hand side densely from the
/// frozen pass rows, the residual samples and the block priors.
fn dense_system(f: &Fixture, model: &JointModel<'_>, damping: f64) -> DenseSystem {
    let (globals, index_of) = expected_index(f, model);
    let n = globals.len();
    let scale = f.data_scale();
    let start = f.layout.residual.start;
    let mut matrix = vec![0.0_f64; n * n];
    let mut rhs = vec![0.0_f64; n];
    let mut damping_diagonal = vec![0.0_f64; n];
    let mut floor = vec![0.0_f64; n];

    for row in &f.pass.coupled_rows {
        let weight = row.weight * scale;
        let penalty: [[f64; DOMAIN_AXES]; DOMAIN_AXES] = row
            .penalty_gn
            .map(|entries| entries.map(|value| value * scale));
        let mut columns: Vec<(usize, [f64; 3], [f64; DOMAIN_AXES])> = Vec::new();
        for (position, (global, derivative)) in row.columns.iter().enumerate() {
            let global = *global as usize;
            assert!(
                index_of[global] != usize::MAX,
                "retained row column {global} is not a joint coordinate"
            );
            columns.push((index_of[global], *derivative, row.axis_terms[position]));
        }
        let mut residual_columns: Vec<(usize, [f64; 3], [f64; DOMAIN_AXES])> = Vec::new();
        for (global, derivative, _, axis) in &row.residual_columns {
            let global = *global as usize;
            if index_of[global] == usize::MAX {
                continue;
            }
            residual_columns.push((index_of[global], *derivative, *axis));
        }
        for &(i, left, left_axis) in &columns {
            for &(j, right, right_axis) in &columns {
                matrix[i * n + j] +=
                    weight * dot3(left, right) + penalty_pair(&penalty, &left_axis, &right_axis);
            }
            for &(j, right, right_axis) in &residual_columns {
                let value =
                    weight * dot3(left, right) + penalty_pair(&penalty, &left_axis, &right_axis);
                matrix[i * n + j] += value;
                matrix[j * n + i] += value;
            }
        }
    }

    for sample in &f.pass.residual_samples {
        let mut nodes: Vec<(usize, f64, usize)> = Vec::new();
        for (slot, node) in sample.nodes.iter().enumerate() {
            let weight = sample.weights[slot];
            if weight == 0.0 {
                continue;
            }
            for channel in 0..3 {
                let full = *node as usize * 3 + channel;
                if !f.mask.free()[full] {
                    continue;
                }
                nodes.push((index_of[start + full], weight, channel));
            }
        }
        let mut curvature = [[0.0_f64; 3]; 3];
        for left in 0..3 {
            for right in 0..3 {
                let mut clip = 0.0;
                for row in 0..3 {
                    clip += sample.clip.0[row][left] * sample.clip.0[row][right];
                }
                curvature[left][right] =
                    sample.alpha * clip + sample.penalty_curvature[left][right];
            }
        }
        for &(i, left_weight, left_channel) in &nodes {
            for &(j, right_weight, right_channel) in &nodes {
                matrix[i * n + j] +=
                    scale * left_weight * right_weight * curvature[left_channel][right_channel];
            }
        }
    }

    for (index, block) in f.blocks.iter().enumerate() {
        if block.kind == BlockKind::Residual {
            continue;
        }
        let width = block.len();
        let (prior_gradient, prior_curvature) =
            prior_terms(&f.problem, &f.roles, &f.params, &f.options, block);
        let identity = identity_curvature(&f.layout, block);
        let lambda = block_lambda(block, &f.lambdas);
        for local in 0..width {
            let global = block.params[local];
            let i = index_of[global];
            assert!(
                i != usize::MAX,
                "block coordinate {global} is not a joint coordinate"
            );
            let fixed = prior_curvature[local] + identity[local];
            floor[i] += fixed;
            matrix[i * n + i] += fixed;
            damping_diagonal[i] = block.full_diagonal[local].max(0.0);
            let mut gradient = block.gradient[local]
                + prior_gradient[local]
                + finite_difference_identity_gradient(&f.layout, block, &f.params.theta, global);
            for other in 0..width {
                let j = index_of[block.params[other]];
                let value = lambda * f.penalties[index][local * width + other];
                matrix[i * n + j] += value;
                gradient += value * f.params.theta[block.params[other]];
            }
            rhs[i] = -gradient;
        }
    }

    let ridge = 2.0 * lam(&f.lambdas, BlockKind::Residual);
    let identity = identity_weight(BlockKind::Residual);
    let penalty_diagonal = residual_penalty_diagonal();
    let theta = &f.params.theta[f.layout.residual.clone()];
    for full in 0..f.layout.residual.len() {
        if !f.mask.free()[full] {
            continue;
        }
        let global = start + full;
        let i = index_of[global];
        floor[i] += identity;
        matrix[i * n + i] += identity + ridge * penalty_diagonal[full];
        for neighbour in residual_neighbours(full) {
            if f.mask.free()[neighbour] {
                matrix[i * n + index_of[start + neighbour]] += -ridge;
            }
        }
        rhs[i] = f.pass.residual_gradient[full]
            - ridge * residual_laplacian(theta, full, &penalty_diagonal)
            - identity * f.params.theta[global];
        damping_diagonal[i] =
            scale * residual_diagonal_from_samples(&f.pass.residual_samples, full);
    }

    for index in 0..n {
        matrix[index * n + index] += damping * damping_diagonal[index].max(1e-12);
    }

    DenseSystem {
        globals,
        index_of,
        matrix,
        rhs,
        damping_diagonal,
        floor,
    }
}

fn dense_solve(matrix: &[f64], rhs: &[f64]) -> Vec<f64> {
    let n = rhs.len();
    let mut a = matrix.to_vec();
    let mut x = rhs.to_vec();
    for column in 0..n {
        let mut pivot = column;
        for row in column + 1..n {
            if a[row * n + column].abs() > a[pivot * n + column].abs() {
                pivot = row;
            }
        }
        assert!(
            a[pivot * n + column].abs() > 1e-14,
            "dense oracle matrix is singular at column {column}"
        );
        if pivot != column {
            for entry in 0..n {
                a.swap(column * n + entry, pivot * n + entry);
            }
            x.swap(column, pivot);
        }
        let diagonal = a[column * n + column];
        for row in column + 1..n {
            let factor = a[row * n + column] / diagonal;
            if factor != 0.0 {
                for entry in column..n {
                    a[row * n + entry] -= factor * a[column * n + entry];
                }
                x[row] -= factor * x[column];
            }
        }
    }
    for row in (0..n).rev() {
        let mut sum = x[row];
        for entry in row + 1..n {
            sum -= a[row * n + entry] * x[entry];
        }
        x[row] = sum / a[row * n + row];
    }
    x
}

fn dense_energy(system: &DenseSystem) -> (f64, Vec<f64>) {
    let delta = dense_solve(&system.matrix, &system.rhs);
    let energy = 0.5
        * system
            .rhs
            .iter()
            .zip(&delta)
            .map(|(gradient, step)| gradient * step)
            .sum::<f64>();
    (energy, delta)
}

fn copy_rhs(model: &mut JointModel<'_>, dense: &mut DenseSystem) {
    for joint in 0..model.dim {
        dense.rhs[dense.index_of[model.globals[joint]]] = model.rhs[joint];
    }
}

fn deterministic_rhs(model: &mut JointModel<'_>) {
    for joint in 0..model.dim {
        let global = model.globals[joint];
        model.rhs[joint] = 0.03 * (((global * 29 + 11) % 23) as f64 - 11.0);
    }
    if model.dim > 0 {
        model.rhs[0] += 0.5;
    }
    if model.border_count > 0 {
        model.rhs[model.outer_count] += 0.5;
    }
    for &(start, width) in &model.image_range {
        if width > 0 {
            model.rhs[start] += 0.5;
        }
    }
    if model.residual_count > 0 {
        model.rhs[model.residual_start] += 0.5;
    }
}

fn block_ranges(model: &JointModel<'_>) -> Vec<(usize, usize)> {
    let mut ranges = vec![
        (0, model.outer_count),
        (model.outer_count, model.outer_count + model.border_count),
        (
            model.residual_start,
            model.residual_start + model.residual_count,
        ),
    ];
    for &(start, width) in &model.image_range {
        if width > 0 {
            ranges.push((start, start + width));
        }
    }
    ranges
}

#[test]
fn dense_oracle_matches_the_coupled_operator_with_residual_cross_and_self_terms() {
    let mut f = fixture(2, 8, 1.0, SEED);
    inject_penalty(&mut f);
    let model = f.model();
    let dense = dense_system(&f, &model, 0.0);
    assert_eq!(model.dim, dense.globals.len());

    let start = f.layout.residual.start;
    let residual: Vec<usize> = (0..f.layout.residual.len())
        .filter(|full| f.mask.free()[*full])
        .map(|full| dense.index_of[start + full])
        .collect();
    assert!(
        !residual.is_empty(),
        "the fixture has no free residual coordinates"
    );
    let mut cross = 0.0_f64;
    let mut self_term = 0.0_f64;
    for &left in &residual {
        for right in 0..dense.globals.len() {
            if !residual.contains(&right) {
                cross = cross.max(dense.matrix[left * dense.globals.len() + right].abs());
            }
        }
        for &right in &residual {
            self_term = self_term.max(dense.matrix[left * dense.globals.len() + right].abs());
        }
    }
    assert!(
        cross > 1e-10,
        "the dense residual cross curvature is missing"
    );
    assert!(
        self_term > 1e-10,
        "the dense residual self curvature is missing"
    );

    for (joint, &global) in model.globals.iter().enumerate() {
        let index = dense.index_of[global];
        let expected_rhs = dense.rhs[index];
        assert!(
            (model.rhs[joint] - expected_rhs).abs() <= 1e-9 * expected_rhs.abs().max(1.0),
            "right-hand side at joint {joint} (global {global}): model {:.9e} dense {expected_rhs:.9e}",
            model.rhs[joint]
        );
        let expected_floor = dense.floor[index];
        assert!(
            (model.fixed[joint] - expected_floor).abs() <= 1e-12 * expected_floor.abs().max(1.0),
            "prior floor at joint {joint} (global {global}): model {:.9e} dense {expected_floor:.9e}",
            model.fixed[joint]
        );
    }

    for seed in 0..3u64 {
        let mut v = vec![0.0_f64; model.dim];
        let mut dense_v = vec![0.0_f64; dense.globals.len()];
        for (joint, &global) in model.globals.iter().enumerate() {
            let value = (((global as u64 * 37 + seed * 101) % 29) as f64 - 14.0) * 0.01;
            v[joint] = value;
            dense_v[dense.index_of[global]] = value;
        }
        let mut product = vec![0.0_f64; model.dim];
        model.apply_undamped(&v, &mut product);
        let dense_product = dense.multiply(&dense_v);
        for (joint, &global) in model.globals.iter().enumerate() {
            let expected = dense_product[dense.index_of[global]];
            let actual = product[joint];
            assert!(
                (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
                "operator entry {joint} (global {global}) seed {seed}: model {actual:.9e} dense {expected:.9e}"
            );
        }
    }
}

#[test]
fn coupled_schur_solve_and_back_substitution_match_the_dense_solution() {
    let mut f = fixture(2, 8, 1.0, SEED ^ 0x11);
    inject_penalty(&mut f);
    let damping = 0.07;
    let mut model = f.model();
    deterministic_rhs(&mut model);
    let mut dense = dense_system(&f, &model, damping);
    copy_rhs(&mut model, &mut dense);
    let dense_delta = dense_solve(&dense.matrix, &dense.rhs);

    let solve = JointSolve::new(&model, damping).expect("factorization");
    let solution = solve
        .solve(&model, CERTIFICATE_TOLERANCE, None, None)
        .expect("resolved joint solve");
    assert!(
        solution.relative_residual < 1e-9,
        "joint residual {}",
        solution.relative_residual
    );

    let mut product = vec![0.0_f64; model.dim];
    model.apply_undamped(&solution.delta, &mut product);
    let mut rhs_norm = 0.0_f64;
    for joint in 0..model.dim {
        let diagonal = dense.damping_diagonal[dense.index_of[model.globals[joint]]];
        product[joint] += damping * diagonal.max(1e-12) * solution.delta[joint];
        rhs_norm += model.rhs[joint] * model.rhs[joint];
    }
    let rhs_norm = rhs_norm.sqrt().max(1.0);
    for joint in 0..model.dim {
        let residual = (product[joint] - model.rhs[joint]).abs();
        assert!(
            residual <= 1e-7 * rhs_norm,
            "full-system residual at joint {joint}: {residual:.3e}"
        );
    }

    for (start, end) in block_ranges(&model) {
        if start == end {
            continue;
        }
        let motion = (start..end)
            .map(|joint| solution.delta[joint].abs())
            .fold(0.0_f64, f64::max);
        assert!(
            motion > 1e-6,
            "block {start}..{end} has no recovered motion"
        );
    }
    for joint in 0..model.dim {
        let expected = dense_delta[dense.index_of[model.globals[joint]]];
        let actual = solution.delta[joint];
        assert!(
            (actual - expected).abs() <= 1e-5 * expected.abs().max(1.0),
            "Schur step at joint {joint}: solver {actual:.9e} dense {expected:.9e}"
        );
    }
}

#[test]
fn zero_reduced_rhs_still_recovers_eliminated_image_motion() {
    let mut f = fixture(2, 8, 1.0, SEED ^ 0x22);
    f.pass.coupled_rows.clear();
    let mut model = f.model();
    model.rhs.fill(0.0);
    let image_start = model.image_range[0].0;
    let image_width = model.image_range[0].1;
    model.rhs[image_start] = 2.0;
    let mut dense = dense_system(&f, &model, 0.0);
    copy_rhs(&mut model, &mut dense);
    let dense_delta = dense_solve(&dense.matrix, &dense.rhs);

    let solve = JointSolve::new(&model, 0.0).expect("factorization");
    assert!(
        solve.reduced_rhs.iter().all(|value| *value == 0.0),
        "reduced right-hand side is not zero: {:?}",
        solve.reduced_rhs
    );
    let solution = solve
        .solve(&model, CERTIFICATE_TOLERANCE, None, None)
        .expect("resolved joint solve");
    assert!(
        solution.delta[image_start].abs() > 1e-6,
        "eliminated image motion was dropped: {:?}",
        solution.delta
    );
    for joint in 0..model.dim {
        if joint >= image_start && joint < image_start + image_width {
            continue;
        }
        assert!(
            solution.delta[joint].abs() < 1e-10,
            "decoupled coordinate {joint} moved by {}",
            solution.delta[joint]
        );
    }
    for joint in 0..model.dim {
        let expected = dense_delta[dense.index_of[model.globals[joint]]];
        let actual = solution.delta[joint];
        assert!(
            (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "zero-RHS step at joint {joint}: solver {actual:.9e} dense {expected:.9e}"
        );
    }
}

#[test]
fn certificate_bounds_enclose_the_dense_quadratic_energy() {
    let mut f = fixture(2, 8, 1.0, SEED ^ 0x33);
    inject_penalty(&mut f);
    let mut model = f.model();
    deterministic_rhs(&mut model);
    let mut dense = dense_system(&f, &model, 0.0);
    copy_rhs(&mut model, &mut dense);
    let (energy, dense_delta) = dense_energy(&dense);
    assert!(
        energy > 1e-2,
        "fixture stationarity energy too small: {energy}"
    );

    let solve = JointSolve::new(&model, 0.0).expect("factorization");
    let solution = solve
        .solve(&model, CERTIFICATE_TOLERANCE, None, None)
        .expect("resolved joint solve");
    for joint in 0..model.dim {
        let expected = dense_delta[dense.index_of[model.globals[joint]]];
        let actual = solution.delta[joint];
        assert!(
            (actual - expected).abs() <= 1e-5 * expected.abs().max(1.0),
            "certificate step at joint {joint}: solver {actual:.9e} dense {expected:.9e}"
        );
    }

    let mut quadratic = 0.0_f64;
    let mut first = 0.0_f64;
    let mut correction = 0.0_f64;
    for joint in 0..model.dim {
        let residual = model.rhs[joint] - solution.curvature[joint];
        let floor = dense.floor[dense.index_of[model.globals[joint]]];
        assert!(
            floor > 0.0 && floor.is_finite(),
            "missing prior floor at joint {joint}"
        );
        quadratic += solution.delta[joint] * solution.curvature[joint];
        first += solution.delta[joint] * residual;
        correction += residual * residual / floor;
    }
    let lower = (0.5 * quadratic + first).max(0.0);
    let upper = lower + 0.5 * correction;
    let tolerance = 1e-5 * energy.max(1.0);
    assert!(
        lower <= energy + tolerance,
        "lower bound {lower:.9e} exceeds dense energy {energy:.9e}"
    );
    assert!(
        energy <= upper + tolerance,
        "dense energy {energy:.9e} exceeds upper bound {upper:.9e}"
    );
    let directional = solve.energy_lower_bound(&model);
    assert!(
        directional.is_finite() && directional <= energy + tolerance,
        "directional lower bound {directional:.9e} exceeds dense energy {energy:.9e}"
    );
}

#[test]
fn certificate_reports_nonstationary_with_a_lower_bound_below_the_dense_energy() {
    let mut f = fixture(2, 8, 1.0, SEED ^ 0x44);
    perturb(&mut f);
    let model = f.model();
    let dense = dense_system(&f, &model, 0.0);
    let (energy, _) = dense_energy(&dense);
    assert!(
        energy > 1e-2,
        "fixture stationarity energy too small: {energy}"
    );
    match f.report().certificate {
        StationarityCertificate::CertifiedNonstationary { lower } => {
            assert!(
                lower.get() <= energy + 1e-5 * energy.max(1.0),
                "reported lower bound {} exceeds dense energy {energy}",
                lower.get()
            );
        }
        other => panic!("expected a nonstationary certificate, got {other:?}"),
    }
}

#[test]
fn certificate_certifies_a_stationary_prior_only_state() {
    let mut f = fixture(1, 8, 1.0, SEED ^ 0x55);
    prior_only(&mut f);
    let report = f.report();
    match report.certificate {
        StationarityCertificate::CertifiedStationary { upper } => {
            assert!(
                upper.get() <= 1e-12,
                "prior-only state certified with upper bound {}",
                upper.get()
            );
        }
        other => panic!("expected a stationary certificate, got {other:?}"),
    }
    assert_eq!(report.origin, CertificateOrigin::FinalStage);
}

#[test]
fn certificate_is_unresolved_without_a_prior_floor() {
    let mut f = fixture(1, 8, 0.0, SEED ^ 0x66);
    prior_only(&mut f);
    match f.report().certificate {
        StationarityCertificate::Unresolved {
            termination: CertificateTermination::NoPriorFloor,
        } => {}
        other => panic!("expected a no-floor unresolved certificate, got {other:?}"),
    }
}

#[test]
fn certificate_bounds_track_the_refined_state_not_the_previous_one() {
    let mut f = fixture(1, 8, 1.0, SEED ^ 0x77);
    prior_only(&mut f);
    let previous = f.report();
    let previous_upper = match previous.certificate {
        StationarityCertificate::CertifiedStationary { upper } => upper.get(),
        other => panic!("expected the prior-only state to certify, got {other:?}"),
    };

    perturb(&mut f);
    let model = f.model();
    let dense = dense_system(&f, &model, 0.0);
    let (energy, _) = dense_energy(&dense);
    assert!(energy > 1e-2, "refined state energy too small: {energy}");
    match f.report().certificate {
        StationarityCertificate::CertifiedNonstationary { lower } => {
            assert!(
                lower.get() <= energy + 1e-5 * energy.max(1.0),
                "refined lower bound {} exceeds dense energy {energy}",
                lower.get()
            );
        }
        other => panic!("expected the refined state to be nonstationary, got {other:?}"),
    }
    assert!(
        previous_upper < energy,
        "the pre-refinement upper bound {previous_upper} still bounds the returned state energy {energy}"
    );
}

#[test]
fn sigma_state_refreshes_a_stale_log_sigma() {
    let mut f = fixture(1, 8, 1.0, SEED ^ 0x88);
    let pairs = f.pass.contributor_pairs[0].clone();
    let (low, high) =
        sigma_bracket(&pairs, f.options.sigma_prior, f.problem.nu).expect("data bracket");
    let root = 0.5 * (low + high);
    let sigma = f.layout.sigma_of(0);

    f.params.theta[sigma] = root + 1.0;
    match conditional_sigma_state(&f.problem, &mut f.params, &f.pass, &f.options) {
        SigmaState::Refresh => {}
        _ => panic!("a stale log sigma was not refreshed"),
    }
    let stored = f.params.theta[sigma];
    assert!(
        stored >= low && stored <= high,
        "refreshed log sigma {stored} is outside [{low}, {high}]"
    );

    f.params.theta[sigma] = root;
    match conditional_sigma_state(&f.problem, &mut f.params, &f.pass, &f.options) {
        SigmaState::Settled => {}
        _ => panic!("a bracketed log sigma was not settled"),
    }
}

#[test]
fn sigma_state_resolves_a_prior_only_contributor() {
    let mut f = fixture(1, 8, 1.0, SEED ^ 0x99);
    for pairs in &mut f.pass.contributor_pairs {
        pairs.clear();
    }
    let sigma = f.layout.sigma_of(0);
    for index in 0..f.layout.sigma.len() {
        f.params.theta[f.layout.sigma_of(index)] = -2.0;
    }
    let expected = 0.5 * libm::log(f.options.sigma_prior.1 / f.options.sigma_prior.0);
    match conditional_sigma_state(&f.problem, &mut f.params, &f.pass, &f.options) {
        SigmaState::Refresh => {}
        _ => panic!("a prior-only contributor did not refresh its log sigma"),
    }
    assert!(
        (f.params.theta[sigma] - expected).abs() < 1e-9,
        "prior-only root {} differs from {}",
        f.params.theta[sigma],
        expected
    );
    match conditional_sigma_state(&f.problem, &mut f.params, &f.pass, &f.options) {
        SigmaState::Settled => {}
        _ => panic!("the prior-only root did not settle"),
    }
}

#[test]
fn sigma_state_is_unresolved_when_the_bracket_is_empty() {
    let mut f = fixture(1, 8, 1.0, SEED ^ 0xaa);
    for pairs in &mut f.pass.contributor_pairs {
        pairs.clear();
    }
    let mut options = f.options;
    options.sigma_prior = (0.0, 4.5);
    match conditional_sigma_state(&f.problem, &mut f.params, &f.pass, &options) {
        SigmaState::Unresolved(CertificateTermination::SigmaUnbracketed) => {}
        _ => panic!("an empty prior-only bracket was not reported as unbracketed"),
    }
}

#[test]
fn residual_mask_pins_unsupported_coordinates() {
    let f = fixture(2, 8, 1.0, SEED ^ 0xbb);
    let mask = &f.mask;
    let total = f.layout.residual.len();
    assert!(
        mask.count() > 0,
        "the fixture has no free residual coordinates"
    );
    assert!(
        mask.count() < total,
        "the fixture pins no residual coordinate"
    );

    let mut theta = vec![0.0_f64; total];
    for (index, free) in mask.free().iter().enumerate() {
        if *free {
            theta[index] = 0.01 * ((index % 5) as f64 + 1.0);
        }
    }
    assert!(!mask.has_pinned_value(&theta));
    let pinned = (0..total)
        .find(|index| !mask.free()[*index])
        .expect("a pinned coordinate");
    theta[pinned] = 0.25;
    assert!(
        mask.has_pinned_value(&theta),
        "a nonzero pinned coordinate was not detected"
    );
    mask.pin(&mut theta);
    assert_eq!(theta[pinned], 0.0);
    assert!(!mask.has_pinned_value(&theta));

    let model = f.model();
    assert_eq!(model.residual_count, mask.count());
    for &global in &model.globals[model.residual_start..] {
        let full = global - f.layout.residual.start;
        assert!(
            full < total && mask.free()[full],
            "pinned residual coordinate {full} entered the joint model"
        );
    }
}

/// One Tone control with one non-default value, so the response block is small.
fn response_fixture(images: usize, samples: usize, seed: u64) -> Fixture {
    let mut data = crate::synth::dataset(&crate::synth::SynthConfig {
        seed,
        images,
        samples_per_image: samples,
        noise_ucs: 0.5,
        ..crate::synth::SynthConfig::default()
    });
    let schema = looklab_schema::validate_control_schema(include_str!(
        "../../../../data/bodies/fujifilm-x-t5.yaml"
    ))
    .expect("body schema validates");
    let sim = looklab_schema::SimKey::new("classic_chrome").expect("sim key");
    let mut model = looklab_model::realization::ControlResponseModel::zeroed(&schema, 1, &sim, 1)
        .expect("zeroed model");
    let index = model
        .controls
        .iter()
        .position(|control| !control.values.is_empty())
        .expect("a layer control with a non-default value");
    model.controls = vec![model.controls.swap_remove(index)];
    model.controls[0].values.truncate(1);
    model.controls[0].affects = vec![looklab_model::realization::Affect::Tone];
    model.wb.controls.clear();
    model.exposure.modes.clear();
    model.exposure.steps.clear();
    let key = model.controls[0].key.clone();
    let value = model.controls[0].values[0].value.clone();
    for image in &mut data.images {
        image.settings.set(key.clone(), value.clone());
    }
    let options = FitOptions {
        seed,
        hessian_samples: usize::MAX,
        run_gcv: false,
        ..FitOptions::default()
    };
    let problem = Problem::new_with(&data.images, Some(&model), &options).expect("problem");
    assert!(
        !problem.layout.response.is_empty(),
        "the response fixture has no response coordinates"
    );
    let layout = problem.layout.clone();
    let params = initial_params(&problem, &options);
    let roles = prior_roles(&problem, &layout);
    let lambdas = StageLambdas {
        tone: 0.7,
        residual: 0.4,
        response_tone: RESPONSE_LAMBDA_FLOOR,
        ..StageLambdas::default()
    };
    let (_, mut blocks, penalties) = stage_blocks(&STAGES[4], &problem, &options);
    let pass = problem.pass_with_rows(
        &params,
        ActiveColumns::ALL,
        &mut blocks,
        false,
        false,
        false,
        IntentFilter::NativeOnly,
        true,
        true,
    );
    let mask = ResidualMask::from_samples(&pass.residual_samples);
    let mut fixture = Fixture {
        layout,
        problem,
        options,
        params,
        roles,
        lambdas,
        blocks,
        penalties,
        pass,
        mask,
    };
    prune_residual_support(&mut fixture);
    fixture
}

fn response_joint(model: &JointModel<'_>, layout: &Layout) -> Vec<usize> {
    (0..model.dim)
        .filter(|joint| layout.response.contains(&model.globals[*joint]))
        .collect()
}

#[test]
fn stage_blocks_carry_the_active_response_into_the_joint_model() {
    let f = response_fixture(2, 8, SEED ^ 0x77);
    assert!(
        f.blocks
            .iter()
            .any(|block| block.kind == BlockKind::ResponseTone && !block.params.is_empty()),
        "the response block was filtered out of the stage blocks"
    );
    let model = f.model();
    let response = response_joint(&model, &f.layout);
    assert!(
        !response.is_empty(),
        "the joint model has no response coordinates"
    );
    for &joint in &response {
        assert_eq!(model.fixed[joint], 0.0);
        assert!((model.floor[joint] - 2.0 * RESPONSE_LAMBDA_FLOOR).abs() <= 1e-12);
    }
    assert!(
        response.iter().any(|&joint| model.rhs[joint].abs() > 1e-6),
        "the response data gradient is missing from the joint right-hand side"
    );
}

#[test]
fn dense_oracle_covers_response_coordinates_and_cross_terms() {
    let f = response_fixture(2, 8, SEED ^ 0x3c);
    let model = f.model();
    let dense = dense_system(&f, &model, 0.0);
    let response = response_joint(&model, &f.layout);
    assert!(!response.is_empty());
    let n = dense.globals.len();
    let response_globals: Vec<usize> = response.iter().map(|&joint| model.globals[joint]).collect();
    let mut cross = 0.0_f64;
    for &joint in &response {
        let index = dense.index_of[model.globals[joint]];
        assert!(
            (model.rhs[joint] - dense.rhs[index]).abs() <= 1e-9 * dense.rhs[index].abs().max(1.0),
            "response rhs at joint {joint}: model {:.9e} dense {:.9e}",
            model.rhs[joint],
            dense.rhs[index]
        );
        for (column, &global) in dense.globals.iter().enumerate() {
            if !response_globals.contains(&global) {
                cross = cross.max(dense.matrix[index * n + column].abs());
            }
        }
    }
    assert!(
        cross > 1e-10,
        "the dense response/look cross curvature is missing"
    );

    let mut v = vec![0.0_f64; model.dim];
    let mut dense_v = vec![0.0_f64; n];
    for (joint, &global) in model.globals.iter().enumerate() {
        let value = (((global as u64 * 37 + 7) % 29) as f64 - 14.0) * 0.01;
        v[joint] = value;
        dense_v[dense.index_of[global]] = value;
    }
    let mut product = vec![0.0_f64; model.dim];
    model.apply_undamped(&v, &mut product);
    let dense_product = dense.multiply(&dense_v);
    for (joint, &global) in model.globals.iter().enumerate() {
        let expected = dense_product[dense.index_of[global]];
        assert!(
            (product[joint] - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "response operator entry {joint} (global {global}): model {:.9e} dense {:.9e}",
            product[joint],
            expected
        );
    }
}

#[test]
fn certificate_rejects_a_free_response_gradient() {
    let mut f = response_fixture(1, 8, SEED ^ 0x5a);
    for block in &mut f.blocks {
        block.reset();
    }
    f.pass.coupled_rows.clear();
    f.pass.residual_samples.clear();
    f.pass.residual_gradient.fill(0.0);
    for index in f.layout.response.clone() {
        f.params.theta[index] = 0.0;
    }
    let mut block = f
        .blocks
        .into_iter()
        .find(|block| block.kind == BlockKind::ResponseTone && !block.params.is_empty())
        .expect("the stage blocks carry the response block");
    block.reset();
    block.gradient.fill(-1.0);
    f.blocks = vec![block];
    let problem = &f.problem;
    f.penalties = f
        .blocks
        .iter()
        .map(|block| block_penalty(problem, block))
        .collect();
    f.lambdas.response_tone = RESPONSE_LAMBDA_FLOOR;
    match f.report().certificate {
        StationarityCertificate::CertifiedNonstationary { lower } => {
            assert!(
                lower.get() > STATIONARY_DECREMENT_NATS,
                "response gradient lower bound {} did not exceed the threshold",
                lower.get()
            );
        }
        other => panic!("expected a nonstationary certificate, got {other:?}"),
    }
}

#[test]
fn certificate_is_unresolved_without_a_response_prior_floor() {
    let mut f = response_fixture(1, 8, SEED ^ 0x6d);
    for block in &mut f.blocks {
        block.reset();
    }
    f.pass.coupled_rows.clear();
    f.pass.residual_samples.clear();
    f.pass.residual_gradient.fill(0.0);
    for index in f.layout.response.clone() {
        f.params.theta[index] = 0.0;
    }
    let mut block = f
        .blocks
        .into_iter()
        .find(|block| block.kind == BlockKind::ResponseTone && !block.params.is_empty())
        .expect("the stage blocks carry the response block");
    block.reset();
    let n = block.len();
    for local in 0..n {
        block.hessian[local * n + local] = 1.0;
        block.full_diagonal[local] = 1.0;
    }
    f.blocks = vec![block];
    let problem = &f.problem;
    f.penalties = f
        .blocks
        .iter()
        .map(|block| block_penalty(problem, block))
        .collect();
    f.lambdas.response_tone = 0.0;
    match f.report().certificate {
        StationarityCertificate::Unresolved {
            termination: CertificateTermination::NoPriorFloor,
        } => {}
        other => panic!("expected a missing response floor, got {other:?}"),
    }
}

#[test]
fn frozen_response_pass_floors_every_response_lambda() {
    let mut f = response_fixture(1, 8, SEED ^ 0x6b);
    let mut lambdas = StageLambdas::default();
    let mut evaluations = 0usize;
    run_frozen_responses(
        &f.problem,
        &mut f.params,
        &f.options,
        &mut lambdas,
        &mut evaluations,
        IntentFilter::NativeOnly,
    )
    .expect("frozen response pass");
    for kind in [
        BlockKind::ResponseTone,
        BlockKind::ResponseTint,
        BlockKind::ResponseChroma,
        BlockKind::ResponseHue,
        BlockKind::ResponseLightness,
        BlockKind::ResponseWb,
    ] {
        assert!(
            lam(&lambdas, kind) >= RESPONSE_LAMBDA_FLOOR,
            "{kind:?} lambda {} is below the floor",
            lam(&lambdas, kind)
        );
    }
}
