// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use looklab_match::{
    AssessedDomain, MatchModel, QualificationRef, ReportPin, ReportSubject, SelectionContext,
    SelectionMode,
};
use looklab_model::file::{LookDocument, LookKind};
use looklab_publish::{ArtifactHash, SiteIndex, SiteReport, TargetEligibilitySnapshot};
use looklab_schema::{
    ModelSubject, ModelVersionId, QualificationCapability, RealizationRevisionId,
};

pub(super) struct Assessments {
    pub index: SiteIndex,
    pub reports: Vec<SiteReport>,
}

pub(super) fn controls_pin(path: &Path) -> Result<ReportPin> {
    let report: SiteReport = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("decoding {}", path.display()))?;
    let ModelVersionId::BodyRealization { realization } = &report.summary.candidate else {
        bail!("{} does not name a camera realization", path.display());
    };
    if report.summary.capability != QualificationCapability::ControlsMatching {
        bail!("{} is not a controls and matching report", path.display());
    }
    Ok(ReportPin {
        subject: ReportSubject::Realization(RealizationRevisionId::for_version(realization)),
        reference: QualificationRef::Report(report.summary.report),
    })
}

pub(super) fn load(
    root: Option<&Path>,
    selection: Option<&SelectionContext>,
) -> Result<Option<Assessments>> {
    let Some(selection) = selection else {
        return Ok(None);
    };
    let root = root.ok_or_else(|| {
        anyhow::anyhow!(
            "a selected result needs --publication to authenticate its pinned artifacts"
        )
    })?;
    let index: SiteIndex = serde_json::from_slice(
        &std::fs::read(root.join("data/index.json")).context("reading the publication index")?,
    )
    .context("decoding the publication index")?;
    let mut reports = Vec::new();
    for pin in &selection.reports {
        match &pin.reference {
            QualificationRef::Report(id) => {
                let entry = index
                    .reports
                    .iter()
                    .find(|entry| entry.report == *id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("report {id} is not listed in the publication")
                    })?;
                let path = root.join(entry.path.as_str());
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                if ArtifactHash::of(&bytes) != entry.sha256 || bytes.len() as u64 != entry.bytes {
                    bail!("{} does not match the publication index", path.display());
                }
                let report: SiteReport = serde_json::from_slice(&bytes)
                    .with_context(|| format!("decoding {}", path.display()))?;
                if report.summary.report != *id {
                    bail!("{} names another report", path.display());
                }
                let bound = match (
                    &pin.subject,
                    &report.summary.subject,
                    &report.summary.candidate,
                ) {
                    (
                        ReportSubject::Target(expected),
                        ModelSubject::SharedFamily { family },
                        ModelVersionId::SharedFamily { target },
                    ) => {
                        expected.look == target.family
                            && expected.version == target.version
                            && family == &target.family
                    }
                    (
                        ReportSubject::Realization(expected),
                        ModelSubject::BodyRealization {
                            realization: subject,
                        },
                        ModelVersionId::BodyRealization {
                            realization: candidate,
                        },
                    ) => {
                        *expected == RealizationRevisionId::for_version(candidate)
                            && subject == &candidate.realization
                    }
                    _ => false,
                };
                if !bound {
                    bail!("{} does not belong to the pinned subject", path.display());
                }
                reports.push(report);
            }
            QualificationRef::Eligibility(id) => {
                let entry = index
                    .eligibility
                    .iter()
                    .find(|entry| entry.id == *id)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "eligibility snapshot {id} is not listed in the publication"
                        )
                    })?;
                let path = root.join(entry.path.as_str());
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                if ArtifactHash::of(&bytes) != entry.sha256 || bytes.len() as u64 != entry.bytes {
                    bail!("{} does not match the publication index", path.display());
                }
                let snapshot: TargetEligibilitySnapshot = serde_json::from_slice(&bytes)
                    .with_context(|| format!("decoding {}", path.display()))?;
                if snapshot.id != *id
                    || !matches!(&pin.subject, ReportSubject::Target(target) if target == &snapshot.target)
                {
                    bail!("{} does not belong to the pinned target", path.display());
                }
            }
        }
    }
    Ok(Some(Assessments { index, reports }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetUse {
    Match,
    Replay,
}

pub(super) fn validate_target(
    assessments: &Assessments,
    selection: &SelectionContext,
    document: &LookDocument,
    path: &Path,
    usage: TargetUse,
) -> Result<()> {
    let look = assessments
        .index
        .looks
        .iter()
        .find(|look| look.slug == document.file.look.slug)
        .ok_or_else(|| anyhow::anyhow!("the target is not listed in the publication"))?;
    let version = look
        .versions
        .iter()
        .find(|version| version.version == document.file.look.version)
        .ok_or_else(|| anyhow::anyhow!("the target version is not listed in the publication"))?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if ArtifactHash::of(&bytes) != version.sha256 || bytes.len() as u64 != version.bytes {
        bail!(
            "{} does not match the published target artifact",
            path.display()
        );
    }
    if usage == TargetUse::Replay {
        return Ok(());
    }
    if !version.matchable || !version.permits_new_outputs() {
        bail!("the target version is not eligible for a new match");
    }
    if selection.mode != SelectionMode::Qualified {
        return Ok(());
    }
    let selected = look
        .selected
        .as_ref()
        .filter(|selected| {
            selected.version == version.version && selected.applicability.is_current()
        })
        .ok_or_else(|| anyhow::anyhow!("a qualified match needs the current selected target"))?;
    let proof = match look.kind {
        LookKind::NativeFamily => look.head.as_ref().is_some_and(|head| {
            head.version == version.version
                && head.applicability.is_current()
                && head.report.is_some_and(|report| {
                    selection.reports.iter().any(|pin| {
                        matches!(&pin.subject, ReportSubject::Target(target) if target.look == head.target.family && target.version == head.target.version)
                            && pin.reference == QualificationRef::Report(report)
                    })
                })
        }),
        LookKind::LutDefined => selected.eligibility.as_ref().is_some_and(|id| {
            assessments.index.eligibility.iter().any(|entry| {
                entry.id == *id && entry.status == looklab_schema::TargetEligibilityStatus::Eligible
                    && selection.reports.iter().any(|pin| {
                        pin.subject == ReportSubject::Target(entry.target.clone())
                            && pin.reference == QualificationRef::Eligibility(*id)
                    })
            })
        }),
    };
    if !proof {
        bail!("the selected target's current qualification proof is not pinned");
    }
    if selection.reports.iter().any(|pin| match &pin.reference {
        QualificationRef::Report(id) => assessments
            .index
            .reports
            .iter()
            .find(|entry| entry.report == *id)
            .is_none_or(|entry| !entry.applicability.is_current()),
        QualificationRef::Eligibility(_) => false,
    }) {
        bail!("a pinned assessment is no longer current");
    }
    Ok(())
}

pub(super) fn domains(
    plan: &looklab_match::Plan,
    models: &[MatchModel],
    selection: Option<&SelectionContext>,
    assessments: Option<&Assessments>,
    output: Option<looklab_schema::OutputEncoding>,
) -> Result<BTreeMap<looklab_schema::SimKey, AssessedDomain>> {
    let Some(selection) = selection.filter(|selection| selection.mode == SelectionMode::Qualified)
    else {
        return Ok(BTreeMap::new());
    };
    let assessments = assessments.ok_or_else(|| anyhow::anyhow!("missing pinned assessments"))?;
    let mut domains = BTreeMap::new();
    for report in &assessments.reports {
        let report = &report.summary;
        if report.capability != QualificationCapability::ControlsMatching {
            continue;
        }
        let ModelVersionId::BodyRealization {
            realization: candidate,
        } = &report.candidate
        else {
            bail!("a controls report does not name a camera realization");
        };
        let Some(model) = models
            .iter()
            .find(|model| model.sim() == &candidate.realization.native_sim)
        else {
            continue;
        };
        let realization = model.realization.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no pinned realization parameters for {}", model.sim())
        })?;
        if candidate != &realization.realization {
            bail!(
                "the controls report for {} names another realization version or target",
                model.sim()
            );
        }
        if let Some(expected) = output
            && report.scope.output_encoding != expected
        {
            bail!(
                "the controls report for {} assessed {}, not the selected camera output {}",
                model.sim(),
                report.scope.output_encoding.as_str(),
                expected.as_str()
            );
        }
        let params = model
            .params
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no pinned body parameters for {}", model.sim()))?;
        let firmware: Vec<_> = realization
            .firmware
            .iter()
            .map(|entry| entry.firmware.clone())
            .collect();
        let wb: Vec<_> = params.wb.iter().map(|entry| entry.mode.clone()).collect();
        let domain = AssessedDomain::from_parts(
            plan,
            report.capability,
            report.outcome,
            &report.scope,
            &report.supported_mask,
            selection.firmware.as_ref(),
            selection.wb_mode.as_ref(),
            &firmware,
            &wb,
        )?;
        if let Some(previous) = domains.get(model.sim()) {
            if previous != &domain {
                bail!("conflicting controls reports for {}", model.sim());
            }
        } else {
            domains.insert(model.sim().clone(), domain);
        }
    }
    if domains.is_empty() {
        bail!("no ranked sim has a passing pinned controls report");
    }
    Ok(domains)
}

pub(super) fn probes(
    domains: &BTreeMap<looklab_schema::SimKey, AssessedDomain>,
) -> Result<(looklab_model::ProbeSet, Vec<usize>)> {
    let mut probes = looklab_model::probes::v1();
    if domains.is_empty() {
        let indices = (0..probes.probes.len()).collect();
        return Ok((probes, indices));
    }
    let mut indices = Vec::new();
    let mut kept = Vec::new();
    for (index, probe) in probes.probes.iter().enumerate() {
        if AssessedDomain::cell_of(probe)
            .is_some_and(|cell| domains.values().all(|domain| domain.supports(&cell)))
        {
            kept.push(probe.clone());
            indices.push(index);
        }
    }
    if kept.is_empty() {
        bail!("the pinned controls reports have no common supported probes");
    }
    probes.probes = kept;
    Ok((probes, indices))
}
