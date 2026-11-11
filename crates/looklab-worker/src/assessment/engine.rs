// SPDX-License-Identifier: AGPL-3.0-or-later
//! P4.3 known-reference engine qualification (docs/02 §5.1, docs/05 §8.1).

use std::collections::BTreeSet;

use looklab_schema::{
    AssessmentOutcome, CellId, EnginePrerequisite, MetricKey, ReportMetrics, RuleRevision,
    SourceRef, SupportedMask,
};

use crate::assessment::{ENGINE_COVERAGE_RULE, P43_BAND_COVERAGE, limitation, median, metric};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineVersion {
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
    pub rule_revision: RuleRevision,
    engine: SourceRef,
}

impl EngineVersion {
    /// # Errors
    /// A composite engine identity outside the identifier grammar or over its length bound.
    pub fn new(
        fitter: SourceRef,
        evaluator: SourceRef,
        model: SourceRef,
        rule_revision: RuleRevision,
    ) -> Result<Self, String> {
        let engine = SourceRef::new(format!(
            "{}|{}|{}",
            fitter.as_str(),
            evaluator.as_str(),
            model.as_str()
        ))
        .map_err(|error| error.to_string())?;
        Ok(Self {
            fitter,
            evaluator,
            model,
            rule_revision,
            engine,
        })
    }

    pub fn source(&self) -> &SourceRef {
        &self.engine
    }
}

#[derive(Clone, Debug)]
pub struct EngineTrial {
    pub body: looklab_schema::BodyId,
    pub cell: CellId,
    pub true_error_de: f64,
    pub band_90_de: f64,
    pub recovered: bool,
    pub held_out_body: bool,
    pub out_of_gamut: bool,
    pub marked_unsupported: bool,
}

#[derive(Clone, Debug)]
pub struct EngineAssessment {
    pub prerequisite: Option<EnginePrerequisite>,
    pub outcome: AssessmentOutcome,
    pub coverage: f64,
    pub recovery_rate: f64,
    pub covered: usize,
    pub recovered: usize,
    pub trials: usize,
    pub bodies: usize,
}

pub fn qualify_engine(version: &EngineVersion, trials: &[EngineTrial]) -> EngineAssessment {
    let mut metrics = ReportMetrics::new();
    let mut limitations = Vec::new();
    let mut execution = false;
    let mut insufficient = false;
    let mut assessed_cells = SupportedMask::new();
    let mut supported_cells = SupportedMask::new();
    let mut bodies = BTreeSet::new();
    let mut covered = 0usize;
    let mut recovered = 0usize;
    let mut valid = 0usize;
    let mut held_out = 0usize;
    let mut out_of_gamut = 0usize;
    let mut out_of_gamut_marked = 0usize;
    let mut errors = Vec::new();

    for trial in trials {
        if !trial.true_error_de.is_finite()
            || !trial.band_90_de.is_finite()
            || trial.band_90_de < 0.0
        {
            execution = true;
            continue;
        }
        valid += 1;
        assessed_cells.insert(trial.cell.clone());
        if trial.out_of_gamut {
            out_of_gamut += 1;
            if trial.marked_unsupported {
                out_of_gamut_marked += 1;
            }
        }
        if !trial.held_out_body {
            continue;
        }
        held_out += 1;
        supported_cells.insert(trial.cell.clone());
        bodies.insert(trial.body.clone());
        errors.push(trial.true_error_de);
        if trial.true_error_de <= trial.band_90_de {
            covered += 1;
        }
        if trial.recovered {
            recovered += 1;
        }
    }
    if valid == 0 {
        insufficient = !execution;
        if !execution {
            limitations.push(limitation("no known-reference engine trials are available"));
        }
    }
    if held_out == 0 && valid > 0 {
        insufficient = true;
        limitations.push(limitation("no held-out-body engine evidence is available"));
    }
    if out_of_gamut == 0 && valid > 0 {
        insufficient = true;
        limitations.push(limitation("no out-of-gamut engine evidence is available"));
    }
    if bodies.len() < 2 && held_out > 0 {
        insufficient = true;
        limitations.push(limitation(
            "engine evidence does not cover at least two held-out bodies",
        ));
    }
    let coverage = if held_out == 0 {
        0.0
    } else {
        covered as f64 / held_out as f64
    };
    let recovery_rate = if held_out == 0 {
        0.0
    } else {
        recovered as f64 / held_out as f64
    };
    let unsupported_failure = out_of_gamut > 0 && out_of_gamut_marked < out_of_gamut;
    let failed = held_out > 0
        && (coverage < P43_BAND_COVERAGE
            || recovery_rate < P43_BAND_COVERAGE
            || unsupported_failure);
    if coverage < P43_BAND_COVERAGE && held_out > 0 {
        limitations.push(limitation(format!(
            "known-reference coverage {coverage:.3} is below the {P43_BAND_COVERAGE:.3} bound"
        )));
    }
    if recovery_rate < P43_BAND_COVERAGE && held_out > 0 {
        limitations.push(limitation(format!(
            "known-reference recovery {recovery_rate:.3} is below the {P43_BAND_COVERAGE:.3} bound"
        )));
    }
    if unsupported_failure {
        limitations.push(limitation(
            "an out-of-gamut known-reference target was not marked unsupported",
        ));
    }
    if let Some(value) = median(&errors) {
        metric(&mut metrics, MetricKey::MedianDe, value);
    }
    metric(&mut metrics, MetricKey::Coverage, coverage);
    metric(&mut metrics, MetricKey::RecoveryRate, recovery_rate);
    metric(
        &mut metrics,
        MetricKey::AssessedCells,
        assessed_cells.len() as f64,
    );
    metric(
        &mut metrics,
        MetricKey::SupportedCells,
        supported_cells.len() as f64,
    );
    let outcome = crate::assessment::outcome(execution, failed, insufficient);
    let prerequisite = match outcome {
        AssessmentOutcome::Pass | AssessmentOutcome::Fail => Some(EnginePrerequisite {
            engine: version.source().clone(),
            rule_revision: version.rule_revision.clone(),
            outcome,
            metrics,
            supported_mask: supported_cells,
            limitations,
        }),
        AssessmentOutcome::InsufficientEvidence | AssessmentOutcome::ExecutionFailure => None,
    };
    EngineAssessment {
        prerequisite,
        outcome,
        coverage,
        recovery_rate,
        covered,
        recovered,
        trials: held_out,
        bodies: bodies.len(),
    }
}

/// Missing or stale engine evidence is insufficient, not a camera pass.
pub(crate) fn engine_prerequisite(
    expected: &EngineVersion,
    provided: Option<&EnginePrerequisite>,
) -> Result<EnginePrerequisite, (AssessmentOutcome, String)> {
    let Some(provided) = provided else {
        return Err((
            AssessmentOutcome::InsufficientEvidence,
            "known-reference engine qualification is missing".to_owned(),
        ));
    };
    if provided.engine.as_str() != expected.source().as_str()
        || provided.rule_revision.as_str() != expected.rule_revision.as_str()
        || expected.rule_revision.as_str() != ENGINE_COVERAGE_RULE
    {
        return Err((
            AssessmentOutcome::InsufficientEvidence,
            "known-reference engine qualification is for a different engine version".to_owned(),
        ));
    }
    if let Err(error) = provided.validate() {
        return Err((AssessmentOutcome::ExecutionFailure, error.to_string()));
    }
    match provided.outcome {
        AssessmentOutcome::Pass => Ok(provided.clone()),
        AssessmentOutcome::InsufficientEvidence => Err((
            AssessmentOutcome::InsufficientEvidence,
            "known-reference engine qualification is insufficient".to_owned(),
        )),
        AssessmentOutcome::Fail => Err((
            AssessmentOutcome::Fail,
            "known-reference engine qualification did not pass".to_owned(),
        )),
        AssessmentOutcome::ExecutionFailure => Err((
            AssessmentOutcome::ExecutionFailure,
            "known-reference engine qualification failed to execute".to_owned(),
        )),
    }
}
