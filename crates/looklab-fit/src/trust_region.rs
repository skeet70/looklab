// SPDX-License-Identifier: AGPL-3.0-or-later
//! Variable-metric Steihaug truncated conjugate gradients for the final refinement.
//!
//! Minimizes `q(d) = g^T d + 0.5 d^T H d` subject to `||d||_D <= radius`, where `D` is a
//! strictly positive diagonal metric and `H` is an indefinite symmetric operator supplied
//! only through analytic Hessian-vector products. The callback owns the full Hessian,
//! including every penalty; this module adds no regularization. The trust norm and the
//! boundary root use `D`. The conjugate-gradient preconditioner is a separate caller-supplied
//! SPD operator, so an existing positive-GN factorization can be reused without eliminating
//! the indefinite `H`. `Stationary` is decided by the Euclidean relative residual
//! `||g + H d|| / ||g||` of the returned step, recomputed from a fresh Hessian-vector product.

use core::fmt;

/// A positive finite trust-region radius.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, serde::Serialize)]
pub struct TrustRadius(f64);

impl TrustRadius {
    /// Returns `None` unless `value` is finite and strictly positive.
    pub fn checked(value: f64) -> Option<Self> {
        (value.is_finite() && value > 0.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// Multiplies by a finite factor in `(0, 1)`; `None` if the result is not positive finite.
    pub fn shrink(self, factor: f64) -> Option<Self> {
        if !(factor.is_finite() && factor > 0.0 && factor < 1.0) {
            return None;
        }
        Self::checked(self.0 * factor)
    }

    /// Multiplies by a finite factor greater than one; `None` if the result is not positive finite.
    pub fn expand(self, factor: f64) -> Option<Self> {
        if !(factor.is_finite() && factor > 1.0) {
            return None;
        }
        Self::checked(self.0 * factor)
    }
}

impl<'de> serde::Deserialize<'de> for TrustRadius {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
        Self::checked(value)
            .ok_or_else(|| serde::de::Error::custom("expected a positive finite trust radius"))
    }
}

/// Why the truncated solve stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustRegionTermination {
    /// The fresh Euclidean residual `||g + H d|| / ||g||` met `relative_tolerance`.
    ///
    /// This is stationary in the explored Krylov subspace; it does not certify a minimum of
    /// an indefinite model.
    Stationary,
    /// The model minimizer along the final direction lay outside the region; the step is on
    /// the boundary.
    Boundary,
    /// The final direction had nonpositive curvature; the boundary step is the model's best
    /// point along it.
    NegativeCurvature,
    /// The gradient is zero. No curvature was probed, so a minimum is not certified.
    ZeroGradient,
}

/// Result of a completed truncated solve. `step` borrows the caller's scratch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrustRegionSolution<'a> {
    pub step: &'a [f64],
    /// `q(0) - q(step)` for the returned step.
    pub predicted_decrease: f64,
    /// `sqrt(step^T D step)`.
    pub metric_norm: f64,
    /// Number of Hessian-vector products applied.
    pub iterations: usize,
    /// Euclidean norm of `g + H step`.
    pub residual_norm: f64,
    /// `residual_norm / ||g||`; zero for a zero gradient.
    pub relative_residual: f64,
    pub termination: TrustRegionTermination,
}

/// Typed solver failure.
#[derive(Debug)]
pub enum TrustRegionError<E, P> {
    /// `gradient.len()` and `metric_diagonal.len()` differ.
    DimensionMismatch { gradient: usize, metric: usize },
    /// The metric diagonal has a non-positive or non-finite entry at `index`.
    InvalidMetric { index: usize },
    /// The relative residual tolerance must be finite and in `[0, 1)`.
    InvalidTolerance { tolerance: f64 },
    /// The preconditioner produced `r^T M^-1 r <= 0`.
    InvalidPreconditioner,
    /// The work limit of Hessian-vector products was exhausted.
    WorkLimit { iterations: usize },
    /// The solver arithmetic, a Hessian-vector product or a preconditioner output produced a
    /// non-finite value.
    Nonfinite,
    /// A search direction lost its descent property or its metric norm vanished.
    Breakdown,
    /// The analytic Hessian-vector product failed.
    Hessian(E),
    /// The analytic preconditioner failed.
    Preconditioner(P),
}

impl<E: fmt::Display, P: fmt::Display> fmt::Display for TrustRegionError<E, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DimensionMismatch { gradient, metric } => write!(
                f,
                "gradient dimension {gradient} differs from metric dimension {metric}"
            ),
            Self::InvalidMetric { index } => {
                write!(f, "metric diagonal entry {index} is not positive finite")
            }
            Self::InvalidTolerance { tolerance } => write!(
                f,
                "relative residual tolerance {tolerance} is not in [0, 1)"
            ),
            Self::InvalidPreconditioner => {
                write!(
                    f,
                    "preconditioner did not produce a positive residual curvature"
                )
            }
            Self::WorkLimit { iterations } => write!(
                f,
                "work limit exhausted after {iterations} Hessian-vector products"
            ),
            Self::Nonfinite => write!(f, "non-finite arithmetic in the trust-region solve"),
            Self::Breakdown => {
                write!(f, "trust-region search direction lost its descent property")
            }
            Self::Hessian(error) => write!(f, "Hessian-vector product failed: {error}"),
            Self::Preconditioner(error) => write!(f, "preconditioner failed: {error}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display, P: fmt::Debug + fmt::Display> std::error::Error
    for TrustRegionError<E, P>
{
}

/// Caller-owned buffers reused across solves. No allocation occurs inside an iteration.
#[derive(Clone, Debug, Default)]
pub struct TrustRegionScratch {
    step: Vec<f64>,
    hessian_step: Vec<f64>,
    residual: Vec<f64>,
    preconditioned: Vec<f64>,
    direction: Vec<f64>,
    product: Vec<f64>,
}

impl TrustRegionScratch {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    fn prepare(&mut self, dim: usize) {
        prepare_buffer(&mut self.step, dim);
        prepare_buffer(&mut self.hessian_step, dim);
        prepare_buffer(&mut self.residual, dim);
        prepare_buffer(&mut self.preconditioned, dim);
        prepare_buffer(&mut self.direction, dim);
        prepare_buffer(&mut self.product, dim);
    }
}

#[cfg(feature = "synth")]
impl TrustRegionScratch {
    /// The step, including the finite partial iterate left by `WorkLimit`.
    pub fn diagnostic_step(&self) -> &[f64] {
        &self.step
    }

    /// Recursively maintained `H step` for the current step.
    pub fn diagnostic_hessian_step(&self) -> &[f64] {
        &self.hessian_step
    }
}

#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug)]
pub struct DiagnosticTrustRegionInput<'a> {
    pub gradient: &'a [f64],
    pub metric_diagonal: &'a [f64],
    pub radius: TrustRadius,
    pub work_limit: usize,
    pub relative_tolerance: f64,
}

/// Euclidean residual norm and its ratio to the initial gradient norm.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TrustRegionResidualProgress {
    /// `||r||`.
    pub residual_norm: f64,
    /// `||r|| / ||g||`.
    pub relative_residual: f64,
}

/// Why the solver recomputed the residual from a fresh Hessian-vector product.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustRegionRefreshReason {
    /// Nonpositive curvature drove the step to the metric boundary.
    NegativeCurvatureBoundary,
    /// A positive-curvature step reached the metric boundary.
    Boundary,
    /// The preconditioned or recursive residual met the tolerance.
    StationarityCheck,
}

/// One completed Hessian refresh.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TrustRegionRefresh {
    /// Inner Hessian-vector products charged when this refresh completed.
    pub hessian_products: usize,
    pub reason: TrustRegionRefreshReason,
    /// Fresh Euclidean residual after the refresh, when finite.
    pub fresh_residual: Option<TrustRegionResidualProgress>,
}

/// Progress after one applied conjugate-gradient step.
#[cfg(feature = "synth")]
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TrustRegionIterationProgress {
    /// Inner Hessian-vector products charged when this step was applied.
    pub hessian_products: usize,
    /// `sqrt(rho / rho_0)` from the preconditioned residual.
    pub preconditioned_relative_residual: f64,
    /// Euclidean residual from the recursively updated residual, when finite.
    pub recursive_residual: Option<TrustRegionResidualProgress>,
}

#[cfg(feature = "synth")]
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct TrustRegionTrace {
    /// `||g||`.
    pub initial_residual_norm: f64,
    /// `rho_0 = g^T M^-1 g`.
    pub initial_preconditioned_rho: f64,
    /// Total inner Hessian-vector products, including every refresh.
    pub hessian_products: usize,
    pub iterations: Vec<TrustRegionIterationProgress>,
    pub refreshes: Vec<TrustRegionRefresh>,
    /// Conjugate-gradient direction resets after a refresh missed the fresh tolerance.
    pub restarts: usize,
}

#[derive(Clone, Copy, Debug)]
struct ResidualProgress {
    residual_norm: f64,
    relative_residual: f64,
}

trait TrustRegionObserver {
    #[inline]
    fn initial(&mut self, _residual_norm: f64, _preconditioned_rho: f64) {}

    #[inline]
    fn iteration(
        &mut self,
        _hessian_products: usize,
        _preconditioned_rho: f64,
        _initial_preconditioned_rho: f64,
        _recursive_residual: Option<ResidualProgress>,
    ) {
    }

    #[inline]
    fn refresh(&mut self, _hessian_products: usize, _reason: TrustRegionRefreshReason) {}

    #[inline]
    fn fresh_residual(&mut self, _progress: Option<ResidualProgress>) {}

    #[inline]
    fn restart(&mut self) {}
}

struct NoObserver;

impl TrustRegionObserver for NoObserver {}

#[cfg(feature = "synth")]
impl From<ResidualProgress> for TrustRegionResidualProgress {
    fn from(progress: ResidualProgress) -> Self {
        Self {
            residual_norm: progress.residual_norm,
            relative_residual: progress.relative_residual,
        }
    }
}

#[cfg(feature = "synth")]
impl TrustRegionObserver for TrustRegionTrace {
    fn initial(&mut self, residual_norm: f64, preconditioned_rho: f64) {
        self.initial_residual_norm = residual_norm;
        self.initial_preconditioned_rho = preconditioned_rho;
    }

    fn iteration(
        &mut self,
        hessian_products: usize,
        preconditioned_rho: f64,
        initial_preconditioned_rho: f64,
        recursive_residual: Option<ResidualProgress>,
    ) {
        self.hessian_products = hessian_products;
        self.iterations.push(TrustRegionIterationProgress {
            hessian_products,
            preconditioned_relative_residual: libm::sqrt(
                preconditioned_rho / initial_preconditioned_rho,
            ),
            recursive_residual: recursive_residual.map(Into::into),
        });
    }

    fn refresh(&mut self, hessian_products: usize, reason: TrustRegionRefreshReason) {
        self.hessian_products = hessian_products;
        self.refreshes.push(TrustRegionRefresh {
            hessian_products,
            reason,
            fresh_residual: None,
        });
    }

    fn fresh_residual(&mut self, progress: Option<ResidualProgress>) {
        if let Some(refresh) = self.refreshes.last_mut() {
            refresh.fresh_residual = progress.map(Into::into);
        }
    }

    fn restart(&mut self) {
        self.restarts += 1;
    }
}

fn prepare_buffer(buffer: &mut Vec<f64>, dim: usize) {
    buffer.clear();
    buffer.resize(dim, 0.0);
}

/// Solve the trust-region subproblem.
///
/// `hessian_product` applies the full symmetric `H`; `precondition` applies an SPD
/// preconditioner `M^-1` to a residual. `D` defines only the trust norm and boundary. The
/// caller owns acceptance, ratio updates and radius policy. `relative_tolerance` bounds the
/// Euclidean relative residual reported with `Stationary`.
///
/// Returns `Err(WorkLimit)` rather than a partial step when the product budget is exhausted.
/// The final Hessian-vector product used for the returned diagnostics is charged to
/// `work_limit`.
/// `ZeroGradient` returns an unchanged zero step and is not a minimum certificate.
#[allow(clippy::too_many_arguments)]
pub fn solve<'s, E, P>(
    gradient: &[f64],
    metric_diagonal: &[f64],
    radius: TrustRadius,
    work_limit: usize,
    relative_tolerance: f64,
    hessian_product: impl FnMut(&[f64], &mut [f64]) -> Result<(), E>,
    precondition: impl FnMut(&[f64], &mut [f64]) -> Result<(), P>,
    scratch: &'s mut TrustRegionScratch,
) -> Result<TrustRegionSolution<'s>, TrustRegionError<E, P>> {
    solve_observed(
        gradient,
        metric_diagonal,
        radius,
        work_limit,
        relative_tolerance,
        hessian_product,
        precondition,
        scratch,
        &mut NoObserver,
    )
}

/// Solve as [`solve`] does, recording preconditioned, recursive and fresh residual progress.
#[cfg(feature = "synth")]
pub fn solve_diagnostic<'s, E, P>(
    input: DiagnosticTrustRegionInput<'_>,
    mut hessian_product: impl FnMut(&[f64], &mut [f64]) -> Result<(), E>,
    precondition: impl FnMut(&[f64], &mut [f64]) -> Result<(), P>,
    scratch: &'s mut TrustRegionScratch,
    trace: &mut TrustRegionTrace,
) -> Result<TrustRegionSolution<'s>, TrustRegionError<E, P>> {
    *trace = TrustRegionTrace::default();
    let mut products = 0usize;
    let outcome = solve_observed(
        input.gradient,
        input.metric_diagonal,
        input.radius,
        input.work_limit,
        input.relative_tolerance,
        |direction, out| {
            products += 1;
            hessian_product(direction, out)
        },
        precondition,
        scratch,
        trace,
    );
    trace.hessian_products = products;
    outcome
}

#[allow(clippy::too_many_arguments)]
fn solve_observed<'s, E, P, O>(
    gradient: &[f64],
    metric_diagonal: &[f64],
    radius: TrustRadius,
    work_limit: usize,
    relative_tolerance: f64,
    mut hessian_product: impl FnMut(&[f64], &mut [f64]) -> Result<(), E>,
    mut precondition: impl FnMut(&[f64], &mut [f64]) -> Result<(), P>,
    scratch: &'s mut TrustRegionScratch,
    observer: &mut O,
) -> Result<TrustRegionSolution<'s>, TrustRegionError<E, P>>
where
    O: TrustRegionObserver,
{
    let dim = gradient.len();
    if metric_diagonal.len() != dim {
        return Err(TrustRegionError::DimensionMismatch {
            gradient: dim,
            metric: metric_diagonal.len(),
        });
    }
    if let Some(index) = metric_diagonal
        .iter()
        .position(|value| !(value.is_finite() && *value > 0.0))
    {
        return Err(TrustRegionError::InvalidMetric { index });
    }
    if !(0.0..1.0).contains(&relative_tolerance) {
        return Err(TrustRegionError::InvalidTolerance {
            tolerance: relative_tolerance,
        });
    }

    scratch.prepare(dim);
    let TrustRegionScratch {
        step,
        hessian_step,
        residual,
        preconditioned,
        direction,
        product,
    } = scratch;

    if gradient.iter().all(|value| *value == 0.0) {
        return Ok(TrustRegionSolution {
            step: &step[..],
            predicted_decrease: 0.0,
            metric_norm: 0.0,
            iterations: 0,
            residual_norm: 0.0,
            relative_residual: 0.0,
            termination: TrustRegionTermination::ZeroGradient,
        });
    }

    residual.copy_from_slice(gradient);
    let initial_residual = euclidean_norm(residual);
    if !initial_residual.is_finite() {
        return Err(TrustRegionError::Nonfinite);
    }

    let mut rho =
        precondition_residual::<E, P>(&residual[..], &mut precondition, &mut preconditioned[..])?;
    if rho <= 0.0 {
        return Err(TrustRegionError::InvalidPreconditioner);
    }
    let rho0 = rho;
    observer.initial(initial_residual, rho0);
    for (p, z) in direction.iter_mut().zip(preconditioned.iter()) {
        *p = -*z;
    }
    let mut iterations = 0usize;

    loop {
        if iterations >= work_limit {
            return Err(TrustRegionError::WorkLimit { iterations });
        }

        hessian_product(&direction[..], &mut product[..]).map_err(TrustRegionError::Hessian)?;
        if product.iter().any(|value| !value.is_finite()) {
            return Err(TrustRegionError::Nonfinite);
        }
        iterations += 1;

        let curvature = dot(direction, product);
        if !curvature.is_finite() {
            return Err(TrustRegionError::Nonfinite);
        }
        let directional = dot(residual, direction);
        if !directional.is_finite() {
            return Err(TrustRegionError::Nonfinite);
        }
        // `q(d + tau p) - q(d) = tau (r.p) + 0.5 tau^2 (p.Hp)`.
        if directional > 0.0 || (directional == 0.0 && curvature >= 0.0) {
            return Err(TrustRegionError::Breakdown);
        }

        if curvature <= 0.0 {
            let Some(roots) = boundary_roots(metric_diagonal, step, direction, radius.get()) else {
                return Err(TrustRegionError::Nonfinite);
            };
            let (Some(positive_scale), Some(negative_scale)) = (roots.positive, roots.negative)
            else {
                return Err(TrustRegionError::Nonfinite);
            };
            let positive_q = endpoint_quadratic(
                gradient,
                step,
                hessian_step,
                direction,
                product,
                positive_scale,
                roots.direction_norm,
            );
            let negative_q = endpoint_quadratic(
                gradient,
                step,
                hessian_step,
                direction,
                product,
                negative_scale,
                roots.direction_norm,
            );
            let scale = match (positive_q, negative_q) {
                (Some(positive), Some(negative)) => {
                    if negative < positive {
                        negative_scale
                    } else {
                        positive_scale
                    }
                }
                _ => return Err(TrustRegionError::Nonfinite),
            };
            if !apply_boundary_step(
                step,
                hessian_step,
                residual,
                direction,
                product,
                scale,
                roots.direction_norm,
            ) {
                return Err(TrustRegionError::Nonfinite);
            }
            refresh_hessian::<E, P>(
                gradient,
                &mut hessian_product,
                &step[..],
                &mut hessian_step[..],
                &mut residual[..],
                &mut iterations,
                work_limit,
            )?;
            observer.refresh(
                iterations,
                TrustRegionRefreshReason::NegativeCurvatureBoundary,
            );
            let fresh = euclidean_residual(&residual[..], initial_residual);
            observer.fresh_residual(fresh);
            return finish(
                &step[..],
                gradient,
                &hessian_step[..],
                metric_diagonal,
                fresh,
                iterations,
                TrustRegionTermination::NegativeCurvature,
            )
            .ok_or(TrustRegionError::Nonfinite);
        }

        let alpha = -directional / curvature;
        if alpha.is_nan() || alpha < 0.0 {
            return Err(TrustRegionError::Nonfinite);
        }
        if alpha == 0.0 {
            return Err(TrustRegionError::Breakdown);
        }

        let boundary = if alpha.is_infinite() {
            true
        } else {
            let candidate = combination_metric_norm(metric_diagonal, step, direction, alpha);
            !candidate.is_finite() || candidate >= radius.get()
        };
        if boundary {
            let Some(roots) = boundary_roots(metric_diagonal, step, direction, radius.get()) else {
                return Err(TrustRegionError::Nonfinite);
            };
            let Some(scale) = roots.positive else {
                return Err(TrustRegionError::Nonfinite);
            };
            if !apply_boundary_step(
                step,
                hessian_step,
                residual,
                direction,
                product,
                scale,
                roots.direction_norm,
            ) {
                return Err(TrustRegionError::Nonfinite);
            }
            refresh_hessian::<E, P>(
                gradient,
                &mut hessian_product,
                &step[..],
                &mut hessian_step[..],
                &mut residual[..],
                &mut iterations,
                work_limit,
            )?;
            observer.refresh(iterations, TrustRegionRefreshReason::Boundary);
            let fresh = euclidean_residual(&residual[..], initial_residual);
            observer.fresh_residual(fresh);
            return finish(
                &step[..],
                gradient,
                &hessian_step[..],
                metric_diagonal,
                fresh,
                iterations,
                TrustRegionTermination::Boundary,
            )
            .ok_or(TrustRegionError::Nonfinite);
        }

        if !apply_step(step, hessian_step, residual, direction, product, alpha) {
            return Err(TrustRegionError::Nonfinite);
        }
        let rho_next = precondition_residual::<E, P>(
            &residual[..],
            &mut precondition,
            &mut preconditioned[..],
        )?;
        if rho_next == 0.0 && residual.iter().any(|value| *value != 0.0) {
            return Err(TrustRegionError::InvalidPreconditioner);
        }
        let recursive_residual = euclidean_residual(&residual[..], initial_residual);
        let accumulated_candidate = recursive_residual
            .is_some_and(|progress| progress.relative_residual <= relative_tolerance);
        observer.iteration(iterations, rho_next, rho0, recursive_residual);
        if preconditioned_candidate(rho_next, rho0, relative_tolerance) || accumulated_candidate {
            refresh_hessian::<E, P>(
                gradient,
                &mut hessian_product,
                &step[..],
                &mut hessian_step[..],
                &mut residual[..],
                &mut iterations,
                work_limit,
            )?;
            observer.refresh(iterations, TrustRegionRefreshReason::StationarityCheck);
            let rho_refreshed = precondition_residual::<E, P>(
                &residual[..],
                &mut precondition,
                &mut preconditioned[..],
            )?;
            if rho_refreshed == 0.0 && residual.iter().any(|value| *value != 0.0) {
                return Err(TrustRegionError::InvalidPreconditioner);
            }
            let fresh = euclidean_residual(&residual[..], initial_residual);
            observer.fresh_residual(fresh);
            if fresh.is_some_and(|progress| progress.relative_residual <= relative_tolerance) {
                return finish(
                    &step[..],
                    gradient,
                    &hessian_step[..],
                    metric_diagonal,
                    fresh,
                    iterations,
                    TrustRegionTermination::Stationary,
                )
                .ok_or(TrustRegionError::Nonfinite);
            }
            for (p, z) in direction.iter_mut().zip(preconditioned.iter()) {
                *p = -*z;
            }
            observer.restart();
            rho = rho_refreshed;
            continue;
        }
        let beta = rho_next / rho;
        if !beta.is_finite() {
            return Err(TrustRegionError::Nonfinite);
        }
        for (p, z) in direction.iter_mut().zip(preconditioned.iter()) {
            *p = -*z + beta * *p;
        }
        if direction.iter().any(|value| !value.is_finite()) {
            return Err(TrustRegionError::Nonfinite);
        }
        rho = rho_next;
    }
}

/// Euclidean Cauchy ray with a fixed coordinate cap.
/// Nocedal & Wright, *Numerical Optimization*, 2nd ed., §4.1.
#[derive(Clone, Copy)]
pub(crate) struct CauchyRay {
    metric_gradient_norm: f64,
    maximum_length: f64,
}

impl CauchyRay {
    pub(crate) fn new(
        gradient: &[f64],
        metric_diagonal: &[f64],
        max_step: f64,
        hessian_gradient: &[f64],
    ) -> Option<Self> {
        let dim = gradient.len();
        if metric_diagonal.len() != dim || hessian_gradient.len() != dim {
            return None;
        }
        if !(max_step.is_finite() && max_step > 0.0) {
            return None;
        }
        let mut gradient_squared = 0.0_f64;
        let mut metric_squared = 0.0_f64;
        let mut curvature = 0.0_f64;
        let mut largest = 0.0_f64;
        for ((value, metric), product) in gradient.iter().zip(metric_diagonal).zip(hessian_gradient)
        {
            if !(value.is_finite() && metric.is_finite() && *metric > 0.0 && product.is_finite()) {
                return None;
            }
            gradient_squared += value * value;
            metric_squared += metric * value * value;
            curvature += value * product;
            largest = largest.max(value.abs());
        }
        if !(gradient_squared.is_finite()
            && gradient_squared > 0.0
            && metric_squared.is_finite()
            && metric_squared > 0.0
            && curvature.is_finite()
            && largest.is_finite()
            && largest > 0.0)
        {
            return None;
        }
        let cap = max_step / largest;
        let maximum_length = if curvature > 0.0 {
            cap.min(gradient_squared / curvature)
        } else {
            cap
        };
        Some(Self {
            metric_gradient_norm: libm::sqrt(metric_squared),
            maximum_length,
        })
    }

    pub(crate) fn step_length(self, radius: TrustRadius) -> Option<f64> {
        let alpha = (radius.get() / self.metric_gradient_norm).min(self.maximum_length);
        (alpha.is_finite() && alpha > 0.0).then_some(alpha)
    }
}

/// `q(0) - q(step)` for `q(d) = g^T d + 0.5 d^T H d`, given `hessian_step = H step`.
/// Returns `None` for mismatched lengths or a non-finite result.
pub fn predicted_decrease(gradient: &[f64], step: &[f64], hessian_step: &[f64]) -> Option<f64> {
    if gradient.len() != step.len() || hessian_step.len() != step.len() {
        return None;
    }
    let linear = dot(gradient, step);
    let quadratic = dot(step, hessian_step);
    let value = -(linear + 0.5 * quadratic);
    value.is_finite().then_some(value)
}

fn finish<'a>(
    step: &'a [f64],
    gradient: &[f64],
    hessian_step: &[f64],
    metric_diagonal: &[f64],
    fresh: Option<ResidualProgress>,
    iterations: usize,
    termination: TrustRegionTermination,
) -> Option<TrustRegionSolution<'a>> {
    let predicted_decrease = predicted_decrease(gradient, step, hessian_step)?;
    let metric_norm = metric_norm(metric_diagonal, step);
    if !metric_norm.is_finite() {
        return None;
    }
    let ResidualProgress {
        residual_norm,
        relative_residual,
    } = fresh?;
    Some(TrustRegionSolution {
        step,
        predicted_decrease,
        metric_norm,
        iterations,
        residual_norm,
        relative_residual,
        termination,
    })
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn max_abs(values: &[f64]) -> f64 {
    values
        .iter()
        .fold(0.0_f64, |max, value| max.max(value.abs()))
}

fn euclidean_norm(values: &[f64]) -> f64 {
    let scale = max_abs(values);
    if scale == 0.0 {
        return 0.0;
    }
    if !scale.is_finite() {
        return f64::INFINITY;
    }
    let sum: f64 = values
        .iter()
        .map(|value| {
            let entry = value / scale;
            entry * entry
        })
        .sum();
    scale * libm::sqrt(sum)
}

fn euclidean_residual(residual: &[f64], initial_residual: f64) -> Option<ResidualProgress> {
    let residual_norm = euclidean_norm(residual);
    if !residual_norm.is_finite() {
        return None;
    }
    let relative_residual = residual_norm / initial_residual;
    relative_residual.is_finite().then_some(ResidualProgress {
        residual_norm,
        relative_residual,
    })
}

fn metric_norm(diagonal: &[f64], values: &[f64]) -> f64 {
    let scale = max_abs(values);
    if scale == 0.0 {
        return 0.0;
    }
    if !scale.is_finite() {
        return f64::INFINITY;
    }
    let mut max = 0.0_f64;
    for (weight, value) in diagonal.iter().zip(values) {
        let entry = libm::sqrt(*weight) * (value / scale);
        max = max.max(entry.abs());
    }
    if max == 0.0 {
        return 0.0;
    }
    let sum: f64 = diagonal
        .iter()
        .zip(values)
        .map(|(weight, value)| {
            let entry = libm::sqrt(*weight) * (value / scale) / max;
            entry * entry
        })
        .sum();
    scale * max * libm::sqrt(sum)
}

fn metric_cosine(diagonal: &[f64], a: &[f64], b: &[f64]) -> f64 {
    let scale_a = max_abs(a);
    let scale_b = max_abs(b);
    if scale_a == 0.0 || scale_b == 0.0 {
        return 0.0;
    }
    if !scale_a.is_finite() || !scale_b.is_finite() {
        return f64::NAN;
    }
    let mut max_a = 0.0_f64;
    let mut max_b = 0.0_f64;
    for ((weight, left), right) in diagonal.iter().zip(a).zip(b) {
        let root = libm::sqrt(*weight);
        let left = root * (left / scale_a);
        let right = root * (right / scale_b);
        max_a = max_a.max(left.abs());
        max_b = max_b.max(right.abs());
    }
    if max_a == 0.0 || max_b == 0.0 {
        return 0.0;
    }
    let mut product = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for ((weight, left), right) in diagonal.iter().zip(a).zip(b) {
        let root = libm::sqrt(*weight);
        let left = root * (left / scale_a) / max_a;
        let right = root * (right / scale_b) / max_b;
        product += left * right;
        norm_a += left * left;
        norm_b += right * right;
    }
    let denominator = libm::sqrt(norm_a) * libm::sqrt(norm_b);
    if denominator == 0.0 {
        return 0.0;
    }
    (product / denominator).clamp(-1.0, 1.0)
}

fn combination_metric_norm(diagonal: &[f64], a: &[f64], b: &[f64], amount: f64) -> f64 {
    if !amount.is_finite() {
        return f64::INFINITY;
    }
    let scale = a.iter().zip(b).fold(0.0_f64, |max, (left, right)| {
        max.max(left.abs()).max((amount * right).abs())
    });
    if scale == 0.0 {
        return 0.0;
    }
    if !scale.is_finite() {
        return f64::INFINITY;
    }
    let mut max = 0.0_f64;
    for ((weight, left), right) in diagonal.iter().zip(a).zip(b) {
        let entry = libm::sqrt(*weight) * (left / scale + amount * (right / scale));
        max = max.max(entry.abs());
    }
    if max == 0.0 {
        return 0.0;
    }
    let sum: f64 = diagonal
        .iter()
        .zip(a)
        .zip(b)
        .map(|((weight, left), right)| {
            let entry = libm::sqrt(*weight) * (left / scale + amount * (right / scale)) / max;
            entry * entry
        })
        .sum();
    scale * max * libm::sqrt(sum)
}

#[derive(Clone, Copy, Debug)]
struct BoundaryRoots {
    direction_norm: f64,
    positive: Option<f64>,
    negative: Option<f64>,
}

/// Both roots of `||d + tau p||_D^2 = radius^2`.
///
/// With `r = ||d||_D / radius`, `w = tau ||p||_D / radius` and
/// `c = <d,p>_D / (||d||_D ||p||_D)`, the roots are
/// `w = -c r +/- sqrt(1 - r^2 (1 - c^2))`.
fn boundary_roots(
    diagonal: &[f64],
    step: &[f64],
    direction: &[f64],
    radius: f64,
) -> Option<BoundaryRoots> {
    let step_norm = metric_norm(diagonal, step);
    let direction_norm = metric_norm(diagonal, direction);
    if !(step_norm.is_finite() && direction_norm.is_finite()) || direction_norm == 0.0 {
        return None;
    }
    if step_norm > radius {
        return None;
    }
    let ratio = (step_norm / radius).min(1.0);
    let cosine = metric_cosine(diagonal, step, direction);
    if !cosine.is_finite() {
        return None;
    }
    let sine_squared = (1.0 - cosine * cosine).max(0.0);
    let discriminant = (1.0 - ratio * ratio * sine_squared).max(0.0);
    let root = libm::sqrt(discriminant);
    let cross = cosine * ratio;
    let positive = (root - cross).max(0.0);
    let negative = (-root - cross).min(0.0);
    Some(BoundaryRoots {
        direction_norm,
        positive: boundary_scale(positive, radius),
        negative: boundary_scale(negative, radius),
    })
}

fn boundary_scale(w: f64, radius: f64) -> Option<f64> {
    if !w.is_finite() {
        return None;
    }
    if w == 0.0 {
        return Some(0.0);
    }
    let scale = w * radius;
    (scale.is_finite() && scale != 0.0).then_some(scale)
}

fn endpoint_quadratic(
    gradient: &[f64],
    step: &[f64],
    hessian_step: &[f64],
    direction: &[f64],
    product: &[f64],
    scale: f64,
    direction_norm: f64,
) -> Option<f64> {
    if !(scale.is_finite() && direction_norm.is_finite() && direction_norm > 0.0) {
        return None;
    }
    let mut linear = 0.0_f64;
    let mut quadratic = 0.0_f64;
    for index in 0..step.len() {
        let delta = scale * (direction[index] / direction_norm);
        let hessian_delta = scale * (product[index] / direction_norm);
        let candidate = step[index] + delta;
        let hessian_candidate = hessian_step[index] + hessian_delta;
        if !(candidate.is_finite() && hessian_candidate.is_finite()) {
            return None;
        }
        linear += gradient[index] * candidate;
        quadratic += candidate * hessian_candidate;
    }
    let value = linear + 0.5 * quadratic;
    value.is_finite().then_some(value)
}

fn apply_boundary_step(
    step: &mut [f64],
    hessian_step: &mut [f64],
    residual: &mut [f64],
    direction: &[f64],
    product: &[f64],
    scale: f64,
    direction_norm: f64,
) -> bool {
    if scale == 0.0 {
        return true;
    }
    if !(scale.is_finite() && direction_norm.is_finite() && direction_norm > 0.0) {
        return false;
    }
    let mut finite = true;
    for index in 0..step.len() {
        let delta = scale * (direction[index] / direction_norm);
        let hessian_delta = scale * (product[index] / direction_norm);
        step[index] += delta;
        hessian_step[index] += hessian_delta;
        residual[index] += hessian_delta;
        finite &= step[index].is_finite()
            && hessian_step[index].is_finite()
            && residual[index].is_finite();
    }
    finite
}

fn refresh_hessian<E, P>(
    gradient: &[f64],
    hessian_product: &mut impl FnMut(&[f64], &mut [f64]) -> Result<(), E>,
    step: &[f64],
    hessian_step: &mut [f64],
    residual: &mut [f64],
    iterations: &mut usize,
    work_limit: usize,
) -> Result<(), TrustRegionError<E, P>> {
    if *iterations >= work_limit {
        return Err(TrustRegionError::WorkLimit {
            iterations: *iterations,
        });
    }
    hessian_product(step, hessian_step).map_err(TrustRegionError::Hessian)?;
    *iterations += 1;
    if hessian_step.iter().any(|value| !value.is_finite()) {
        return Err(TrustRegionError::Nonfinite);
    }
    for ((entry, gradient), product) in residual
        .iter_mut()
        .zip(gradient.iter())
        .zip(hessian_step.iter())
    {
        *entry = *gradient + *product;
        if !entry.is_finite() {
            return Err(TrustRegionError::Nonfinite);
        }
    }
    Ok(())
}

fn precondition_residual<E, P>(
    residual: &[f64],
    precondition: &mut impl FnMut(&[f64], &mut [f64]) -> Result<(), P>,
    preconditioned: &mut [f64],
) -> Result<f64, TrustRegionError<E, P>> {
    precondition(residual, preconditioned).map_err(TrustRegionError::Preconditioner)?;
    if preconditioned.iter().any(|value| !value.is_finite()) {
        return Err(TrustRegionError::Nonfinite);
    }
    let rho = dot(residual, preconditioned);
    if !(rho.is_finite() && rho >= 0.0) {
        return Err(TrustRegionError::InvalidPreconditioner);
    }
    Ok(rho)
}

/// `rho <= 0 || sqrt(rho / initial) <= tolerance`.
fn preconditioned_candidate(rho: f64, initial: f64, tolerance: f64) -> bool {
    if rho <= 0.0 {
        return true;
    }
    let ratio = rho / initial;
    if !ratio.is_finite() {
        return false;
    }
    libm::sqrt(ratio) <= tolerance
}

fn apply_step(
    step: &mut [f64],
    hessian_step: &mut [f64],
    residual: &mut [f64],
    direction: &[f64],
    product: &[f64],
    amount: f64,
) -> bool {
    let mut finite = true;
    for index in 0..step.len() {
        let delta = amount * direction[index];
        let hessian_delta = amount * product[index];
        step[index] += delta;
        hessian_step[index] += hessian_delta;
        residual[index] += hessian_delta;
        finite &= step[index].is_finite()
            && hessian_step[index].is_finite()
            && residual[index].is_finite();
    }
    finite
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CallbackError {
        Failed,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct OwnedSolution {
        step: Vec<f64>,
        predicted_decrease: f64,
        metric_norm: f64,
        iterations: usize,
        residual_norm: f64,
        relative_residual: f64,
        termination: TrustRegionTermination,
    }

    fn dense_apply(matrix: &[f64], dim: usize, vector: &[f64]) -> Vec<f64> {
        (0..dim)
            .map(|row| {
                (0..dim)
                    .map(|column| matrix[row * dim + column] * vector[column])
                    .sum()
            })
            .collect()
    }

    fn run(
        gradient: &[f64],
        metric: &[f64],
        matrix: &[f64],
        radius: f64,
        work_limit: usize,
        tolerance: f64,
    ) -> Result<OwnedSolution, TrustRegionError<CallbackError, CallbackError>> {
        let dim = gradient.len();
        let mut scratch = TrustRegionScratch::new();
        let radius = TrustRadius::checked(radius).expect("positive test radius");
        let result = solve(
            gradient,
            metric,
            radius,
            work_limit,
            tolerance,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                for row in 0..dim {
                    out[row] = (0..dim)
                        .map(|column| matrix[row * dim + column] * direction[column])
                        .sum();
                }
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                for index in 0..dim {
                    out[index] = residual[index] / metric[index];
                }
                Ok(())
            },
            &mut scratch,
        )?;
        Ok(OwnedSolution {
            step: result.step.to_vec(),
            predicted_decrease: result.predicted_decrease,
            metric_norm: result.metric_norm,
            iterations: result.iterations,
            residual_norm: result.residual_norm,
            relative_residual: result.relative_residual,
            termination: result.termination,
        })
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        let scale = expected.abs().max(actual.abs());
        let bound = if scale == 0.0 { 0.0 } else { tolerance * scale };
        assert!(
            (actual - expected).abs() <= bound,
            "expected {expected:.17e}, got {actual:.17e}"
        );
    }

    #[test]
    fn spd_interior_step_matches_the_dense_minimizer() {
        let matrix = [4.0, 1.0, 1.0, 3.0];
        let gradient = [-4.0, -3.0];
        let result = run(&gradient, &[1.0, 1.0], &matrix, 10.0, 10, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Stationary);
        assert_close(result.step[0], 9.0 / 11.0, 1e-10);
        assert_close(result.step[1], 8.0 / 11.0, 1e-10);
        assert_close(result.predicted_decrease, 30.0 / 11.0, 1e-10);
        let expected_norm = libm::hypot(9.0 / 11.0, 8.0 / 11.0);
        assert_close(result.metric_norm, expected_norm, 1e-10);
        assert!(result.relative_residual <= 1e-10);
    }

    #[test]
    fn boundary_step_lands_on_the_metric_boundary() {
        let matrix = [1.0, 0.0, 0.0, 1.0];
        let gradient = [-1.0, 0.0];
        let result = run(&gradient, &[1.0, 1.0], &matrix, 0.5, 10, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        assert_close(result.step[0], 0.5, 1e-12);
        assert_close(result.step[1], 0.0, 1e-12);
        assert_close(result.metric_norm, 0.5, 1e-12);
        assert_close(result.predicted_decrease, 0.375, 1e-12);
        assert!(result.relative_residual > 0.4);
    }

    #[test]
    fn negative_curvature_takes_the_boundary_with_a_real_decrease() {
        let matrix = [1.0, 0.0, 0.0, -1.0];
        let gradient = [0.0, 1.0];
        let result = run(&gradient, &[1.0, 1.0], &matrix, 2.0, 10, 1e-12).unwrap();
        assert_eq!(
            result.termination,
            TrustRegionTermination::NegativeCurvature
        );
        assert_close(result.step[0], 0.0, 1e-12);
        assert_close(result.step[1], -2.0, 1e-12);
        assert_close(result.metric_norm, 2.0, 1e-12);
        assert_close(result.predicted_decrease, 4.0, 1e-12);
        assert!(result.relative_residual > 1.0);
    }

    #[test]
    fn indefinite_curvature_terminates_on_the_boundary_after_a_positive_first_step() {
        let matrix = [1.0, 0.0, 0.0, -1.0];
        let gradient = [1.0, 0.1];
        let result = run(&gradient, &[1.0, 1.0], &matrix, 100.0, 10, 1e-12).unwrap();
        assert_eq!(
            result.termination,
            TrustRegionTermination::NegativeCurvature
        );
        assert_close(result.metric_norm, 100.0, 1e-10);
        assert!(result.predicted_decrease > 0.0);
        let hessian_step = dense_apply(&matrix, 2, &result.step);
        let decrease = predicted_decrease(&gradient, &result.step, &hessian_step).unwrap();
        assert_close(result.predicted_decrease, decrease, 1e-10);
    }

    #[test]
    fn zero_gradient_returns_the_zero_step_without_certifying_a_minimum() {
        let matrix = [1.0, 0.0, 0.0, -1.0];
        let result = run(&[0.0, 0.0], &[1.0, 1.0], &matrix, 1.0, 10, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::ZeroGradient);
        assert_eq!(result.step, vec![0.0, 0.0]);
        assert_eq!(result.predicted_decrease, 0.0);
        assert_eq!(result.metric_norm, 0.0);
        assert_eq!(result.iterations, 0);
        assert_eq!(result.residual_norm, 0.0);
        assert_eq!(result.relative_residual, 0.0);
    }

    #[test]
    fn anisotropic_metric_interior_step_uses_the_d_metric() {
        let matrix = [1.0, 0.0, 0.0, 1.0];
        let gradient = [-1.0, -1.0];
        let metric = [4.0, 1.0];
        let result = run(&gradient, &metric, &matrix, 10.0, 10, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Stationary);
        assert_close(result.step[0], 1.0, 1e-12);
        assert_close(result.step[1], 1.0, 1e-12);
        assert_close(result.metric_norm, 5.0_f64.sqrt(), 1e-12);
        assert_close(result.predicted_decrease, 1.0, 1e-12);
    }

    #[test]
    fn anisotropic_metric_boundary_uses_the_d_norm() {
        let matrix = [1.0, 0.0, 0.0, 1.0];
        let gradient = [-1.0, -1.0];
        let metric = [4.0, 1.0];
        let radius = 1.0;
        let result = run(&gradient, &metric, &matrix, radius, 10, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        let p = [0.25, 1.0];
        let pp = metric[0] * p[0] * p[0] + metric[1] * p[1] * p[1];
        let tau = (radius * radius / pp).sqrt();
        let expected = [tau * p[0], tau * p[1]];
        assert_close(result.step[0], expected[0], 1e-10);
        assert_close(result.step[1], expected[1], 1e-10);
        assert_close(result.metric_norm, radius, 1e-10);
        let expected_decrease = -(gradient[0] * expected[0]
            + gradient[1] * expected[1]
            + 0.5 * (expected[0] * expected[0] + expected[1] * expected[1]));
        assert_close(result.predicted_decrease, expected_decrease, 1e-10);
    }

    #[test]
    fn a_separate_preconditioner_does_not_change_the_interior_minimizer() {
        let matrix = [4.0, 1.0, 1.0, 3.0];
        let gradient = [-4.0, -3.0];
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &[1.0, 1.0],
            TrustRadius::checked(10.0).unwrap(),
            10,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = matrix[0] * direction[0] + matrix[1] * direction[1];
                out[1] = matrix[2] * direction[0] + matrix[3] * direction[1];
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = 2.0 * residual[0];
                out[1] = 0.5 * residual[1];
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Stationary);
        assert_close(result.step[0], 9.0 / 11.0, 1e-10);
        assert_close(result.step[1], 8.0 / 11.0, 1e-10);
    }

    #[test]
    fn anisotropic_preconditioner_cannot_certify_a_large_euclidean_residual() {
        // `M^-1 = diag(1, 1e-30)` drives the preconditioned ratio to 1e-15 after one step
        // while `||g + H d|| / ||g||` is `1/sqrt(2)`.
        let gradient = [1.0, 1.0];
        let hessian = [1.0, 0.0, 0.0, 2.0];
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &[1.0, 1.0],
            TrustRadius::checked(10.0).unwrap(),
            10,
            1e-10,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = hessian[0] * direction[0] + hessian[1] * direction[1];
                out[1] = hessian[2] * direction[0] + hessian[3] * direction[1];
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = residual[0];
                out[1] = 1e-30 * residual[1];
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Stationary);
        assert_close(result.step[0], -1.0, 1e-12);
        assert_close(result.step[1], -0.5, 1e-12);
        assert_close(result.predicted_decrease, 0.75, 1e-12);
        let hessian_step = dense_apply(&hessian, 2, result.step);
        let residual = [gradient[0] + hessian_step[0], gradient[1] + hessian_step[1]];
        assert_close(
            result.residual_norm,
            libm::hypot(residual[0], residual[1]),
            1e-12,
        );
        assert!(result.relative_residual <= 1e-10);
    }

    #[test]
    fn trust_boundary_uses_d_with_a_separate_preconditioner() {
        let gradient = [-1.0, -1.0];
        let metric = [4.0, 1.0];
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &metric,
            TrustRadius::checked(1.0).unwrap(),
            10,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        assert_close(result.metric_norm, 1.0, 1e-10);
    }

    #[test]
    fn hessian_product_failure_is_reported_with_its_type() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, -1.0],
            &[1.0, 1.0],
            TrustRadius::checked(1.0).unwrap(),
            4,
            1e-12,
            |_: &[f64], _: &mut [f64]| -> Result<(), CallbackError> { Err(CallbackError::Failed) },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::Hessian(CallbackError::Failed))
        ));
    }

    #[test]
    fn preconditioner_failure_is_reported_with_its_type() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, -1.0],
            &[1.0, 1.0],
            TrustRadius::checked(1.0).unwrap(),
            4,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |_: &[f64], _: &mut [f64]| -> Result<(), CallbackError> { Err(CallbackError::Failed) },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::Preconditioner(CallbackError::Failed))
        ));
    }

    #[test]
    fn nonfinite_hessian_product_is_a_failure() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, -1.0],
            &[1.0, 1.0],
            TrustRadius::checked(1.0).unwrap(),
            4,
            1e-12,
            |_: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = f64::NAN;
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(result, Err(TrustRegionError::Nonfinite)));
    }

    #[test]
    fn nonpositive_preconditioned_residual_is_rejected() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, -1.0],
            &[1.0, 1.0],
            TrustRadius::checked(1.0).unwrap(),
            4,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |_: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.fill(0.0);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::InvalidPreconditioner)
        ));
    }

    #[test]
    fn work_limit_is_an_explicit_failure() {
        let matrix = [1.0, 0.0, 0.0, 2.0];
        let gradient = [-1.0, -1.0];
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &[1.0, 1.0],
            TrustRadius::checked(10.0).unwrap(),
            1,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = matrix[0] * direction[0] + matrix[1] * direction[1];
                out[1] = matrix[2] * direction[0] + matrix[3] * direction[1];
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::WorkLimit { iterations: 1 })
        ));
    }

    #[test]
    fn zero_work_limit_fails_before_any_product() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0],
            &[1.0],
            TrustRadius::checked(1.0).unwrap(),
            0,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::WorkLimit { iterations: 0 })
        ));
    }

    #[test]
    fn invalid_metric_is_rejected_before_solving() {
        let cases = [
            ([1.0, 0.0], 1usize),
            ([1.0, -1.0], 1),
            ([1.0, f64::NAN], 1),
            ([1.0, f64::INFINITY], 1),
        ];
        for (metric, index) in cases {
            let mut scratch = TrustRegionScratch::new();
            let result = solve(
                &[-1.0, -1.0],
                &metric,
                TrustRadius::checked(1.0).unwrap(),
                4,
                1e-12,
                |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                    out.copy_from_slice(direction);
                    Ok(())
                },
                |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                    out.copy_from_slice(residual);
                    Ok(())
                },
                &mut scratch,
            );
            assert!(matches!(
                result,
                Err(TrustRegionError::InvalidMetric { index: found }) if found == index
            ));
        }
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, -1.0],
            &[1.0],
            TrustRadius::checked(1.0).unwrap(),
            4,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::DimensionMismatch {
                gradient: 2,
                metric: 1
            })
        ));
    }

    #[test]
    fn invalid_tolerance_is_rejected() {
        for tolerance in [f64::NAN, -0.1, 1.0, f64::INFINITY] {
            let mut scratch = TrustRegionScratch::new();
            let result = solve(
                &[-1.0],
                &[1.0],
                TrustRadius::checked(1.0).unwrap(),
                4,
                tolerance,
                |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                    out.copy_from_slice(direction);
                    Ok(())
                },
                |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                    out.copy_from_slice(residual);
                    Ok(())
                },
                &mut scratch,
            );
            assert!(matches!(
                result,
                Err(TrustRegionError::InvalidTolerance { .. })
            ));
        }
    }

    #[test]
    fn boundary_root_survives_extreme_metric_scales() {
        let metric = [1e-300, 1e300];
        let matrix = [1e-300, 0.0, 0.0, 1e-300];
        let gradient = [-1e-150, -1e150];
        let result = run(&gradient, &metric, &matrix, 1.0, 4, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        let expected = 1.0 / 2.0_f64.sqrt();
        assert_close(result.step[0], expected * 1e150, 1e-10);
        assert_close(result.step[1], expected * 1e-150, 1e-10);
        assert_close(result.metric_norm, 1.0, 1e-10);
        let hessian_step = dense_apply(&matrix, 2, &result.step);
        let decrease = predicted_decrease(&gradient, &result.step, &hessian_step).unwrap();
        assert_close(result.predicted_decrease, decrease, 1e-10);
        assert!(result.predicted_decrease > 1.0);
    }

    #[test]
    fn boundary_root_handles_a_tiny_radius() {
        let matrix = [1.0, 0.0, 0.0, 1.0];
        let gradient = [-1.0, -1.0];
        let radius = 1e-300;
        let result = run(&gradient, &[1.0, 1.0], &matrix, radius, 4, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        assert_close(result.metric_norm, radius, 1e-10);
        assert!(result.step[0] > 0.0 && result.step[0].is_finite());
        assert!(result.predicted_decrease > 0.0);
    }

    #[test]
    fn boundary_root_handles_large_vectors_without_overflow() {
        let matrix = [1.0, 0.0, 0.0, 1.0];
        let gradient = [-1e150, -1e150];
        let radius = 1e149;
        let result = run(&gradient, &[1.0, 1.0], &matrix, radius, 4, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        assert_close(result.metric_norm, radius, 1e-10);
        assert!(result.step[0].is_finite() && result.step[1].is_finite());
        assert!(result.predicted_decrease > 0.0);
    }

    #[test]
    fn stale_accumulated_residual_does_not_report_stationary() {
        let matrix = [1.0, 1.0, 1.0, 1.0 + f64::EPSILON];
        let gradient = [3.0, 0.0];
        match run(&gradient, &[1.0, 1.0], &matrix, 1e17, 20, 1e-12) {
            Ok(solution) => {
                let hessian_step = dense_apply(&matrix, 2, &solution.step);
                let residual = [gradient[0] + hessian_step[0], gradient[1] + hessian_step[1]];
                let residual_norm = libm::hypot(residual[0], residual[1]);
                assert_close(solution.residual_norm, residual_norm, 1e-10);
                let decrease =
                    predicted_decrease(&gradient, &solution.step, &hessian_step).unwrap();
                assert_close(solution.predicted_decrease, decrease, 1e-10);
                if solution.termination == TrustRegionTermination::Stationary {
                    assert!(solution.relative_residual <= 1e-12 + 1e-15);
                }
            }
            Err(TrustRegionError::WorkLimit { .. }) => {}
            Err(error) => panic!("unexpected solver failure: {error:?}"),
        }
    }

    #[test]
    fn boundary_coefficient_underflow_does_not_return_a_zero_step() {
        let matrix = [1.0];
        let gradient = [-1e150];
        let radius = 1e-300;
        let result = run(&gradient, &[1.0], &matrix, radius, 4, 1e-12).unwrap();
        assert_eq!(result.termination, TrustRegionTermination::Boundary);
        assert_close(result.step[0], radius, 1e-10);
        assert_close(result.metric_norm, radius, 1e-10);
        assert!(result.predicted_decrease > 0.0);
    }

    #[test]
    fn negative_curvature_chooses_the_lower_boundary_endpoint() {
        let matrix = [2.0, 0.0, 0.0, -1.0];
        let gradient = [1.0, 1.0];
        let metric = [9.0, 1.0];
        let radius = 97.0_f64.sqrt();
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &metric,
            TrustRadius::checked(radius).unwrap(),
            10,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out[0] = matrix[0] * direction[0] + matrix[1] * direction[1];
                out[1] = matrix[2] * direction[0] + matrix[3] * direction[1];
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(
            result.termination,
            TrustRegionTermination::NegativeCurvature
        );
        assert_close(result.step[0], 31.0 / 13.0, 1e-10);
        assert_close(result.step[1], 88.0 / 13.0, 1e-10);
        assert_close(result.metric_norm, radius, 1e-10);
        assert_close(result.predicted_decrease, 1364.0 / 169.0, 1e-10);
    }

    #[test]
    fn a_subnormal_gradient_is_not_accepted_at_zero_budget() {
        let gradient = [libm::scalbn(1.0, -537)];
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &gradient,
            &[1.0],
            TrustRadius::checked(1.0).unwrap(),
            0,
            0.9,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::WorkLimit { iterations: 0 })
        ));
    }

    #[test]
    fn boundary_termination_charges_the_diagnostic_product() {
        let mut scratch = TrustRegionScratch::new();
        let result = solve(
            &[-1.0, 0.0],
            &[1.0, 1.0],
            TrustRadius::checked(0.5).unwrap(),
            1,
            1e-12,
            |direction: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(direction);
                Ok(())
            },
            |residual: &[f64], out: &mut [f64]| -> Result<(), CallbackError> {
                out.copy_from_slice(residual);
                Ok(())
            },
            &mut scratch,
        );
        assert!(matches!(
            result,
            Err(TrustRegionError::WorkLimit { iterations: 1 })
        ));
    }

    #[cfg(feature = "synth")]
    #[test]
    fn diagnostic_boundary_work_limit_counts_the_last_direction_product() {
        for curvature in [1.0, -1.0] {
            let mut scratch = TrustRegionScratch::new();
            let mut trace = TrustRegionTrace::default();
            let mut products = 0usize;
            let result = solve_diagnostic(
                DiagnosticTrustRegionInput {
                    gradient: &[-1.0, 0.0],
                    metric_diagonal: &[1.0, 1.0],
                    radius: TrustRadius::checked(0.5).unwrap(),
                    work_limit: 1,
                    relative_tolerance: 1e-12,
                },
                |direction, out| -> Result<(), CallbackError> {
                    products += 1;
                    for (product, value) in out.iter_mut().zip(direction) {
                        *product = curvature * *value;
                    }
                    Ok(())
                },
                |residual, out| -> Result<(), CallbackError> {
                    out.copy_from_slice(residual);
                    Ok(())
                },
                &mut scratch,
                &mut trace,
            );
            assert!(matches!(
                result,
                Err(TrustRegionError::WorkLimit { iterations: 1 })
            ));
            assert_eq!(products, 1);
            assert_eq!(trace.hessian_products, 1);
            assert_eq!(scratch.diagnostic_step(), &[0.5, 0.0]);
        }
    }

    #[test]
    fn cauchy_length_matches_the_feasible_euclidean_minimizer() {
        let epsilon = 0.001;
        let gradient = [epsilon, 1.0];
        let metric = [epsilon * epsilon, 1.0];
        let hessian_gradient = [epsilon * epsilon * epsilon, 1.0];
        let alpha = CauchyRay::new(&gradient, &metric, 2.0, &hessian_gradient)
            .unwrap()
            .step_length(TrustRadius::checked(2.0).unwrap())
            .unwrap();
        let expected = (1.0 + epsilon * epsilon) / (1.0 + epsilon * epsilon * epsilon * epsilon);
        assert_close(alpha, expected, 1e-12);
    }

    #[test]
    fn cauchy_length_is_capped_by_the_step_limit() {
        let gradient = [0.0, 1.0];
        let metric = [1.0, 1.0];
        let hessian_gradient = [0.0, 0.0];
        let alpha = CauchyRay::new(&gradient, &metric, 2.0, &hessian_gradient)
            .unwrap()
            .step_length(TrustRadius::checked(100.0).unwrap())
            .unwrap();
        assert_close(alpha, 2.0, 1e-15);
    }

    #[test]
    fn cauchy_length_is_capped_by_the_metric_ball() {
        let gradient = [1.0];
        let metric = [1.0];
        let hessian_gradient = [1.0];
        let alpha = CauchyRay::new(&gradient, &metric, 100.0, &hessian_gradient)
            .unwrap()
            .step_length(TrustRadius::checked(0.5).unwrap())
            .unwrap();
        assert_close(alpha, 0.5, 1e-15);
    }

    #[test]
    fn predicted_decrease_reports_the_quadratic_reduction() {
        let gradient = [-1.0, -2.0];
        let step = [0.5, 0.25];
        let hessian_step = [0.5, 0.25];
        let value = predicted_decrease(&gradient, &step, &hessian_step).unwrap();
        let expected = -((gradient[0] * step[0] + gradient[1] * step[1])
            + 0.5 * (step[0] * hessian_step[0] + step[1] * hessian_step[1]));
        assert_close(value, expected, 1e-15);
    }

    #[test]
    fn trust_radius_shrink_and_expand_cannot_produce_invalid_values() {
        let radius = TrustRadius::checked(4.0).unwrap();
        assert_eq!(radius.shrink(0.5).map(TrustRadius::get), Some(2.0));
        assert_eq!(radius.expand(2.0).map(TrustRadius::get), Some(8.0));
        assert_eq!(radius.shrink(0.0), None);
        assert_eq!(radius.shrink(1.0), None);
        assert_eq!(radius.shrink(-0.5), None);
        assert_eq!(radius.shrink(f64::NAN), None);
        assert_eq!(radius.expand(1.0), None);
        assert_eq!(radius.expand(0.5), None);
        assert_eq!(radius.expand(f64::INFINITY), None);
        let largest = TrustRadius::checked(f64::MAX).unwrap();
        assert_eq!(largest.expand(2.0), None);
    }

    #[test]
    fn trust_radius_serde_roundtrip_validates_positive_finite_values() {
        let radius = TrustRadius::checked(2.5).unwrap();
        let text = serde_json::to_string(&radius).unwrap();
        let decoded: TrustRadius = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, radius);
        for invalid in ["0.0", "-1.0", "null"] {
            assert!(serde_json::from_str::<TrustRadius>(invalid).is_err());
        }
    }

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct SavedState {
        #[serde(default = "unit_radius")]
        radius: TrustRadius,
        theta: Vec<f64>,
    }

    fn unit_radius() -> TrustRadius {
        TrustRadius::checked(1.0).unwrap()
    }

    #[test]
    fn saved_state_roundtrips_the_trust_radius_and_rejects_invalid_values() {
        let state = SavedState {
            radius: TrustRadius::checked(0.25).unwrap(),
            theta: vec![1.0, -2.0],
        };
        let text = serde_json::to_string(&state).unwrap();
        let decoded: SavedState = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, state);
        let missing: SavedState = serde_json::from_str(r#"{"theta":[1.0]}"#).unwrap();
        assert_eq!(missing.radius, unit_radius());
        let invalid = serde_json::from_str::<SavedState>(r#"{"radius":0.0,"theta":[]}"#);
        assert!(invalid.is_err());
    }
}
