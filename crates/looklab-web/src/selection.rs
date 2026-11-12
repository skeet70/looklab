// SPDX-License-Identifier: AGPL-3.0-or-later
//! One catalog selection path for target versions and their qualification scope.

use looklab_model::file::{LookKind, LookVersion};
use looklab_publish::{
    LookPage, LookSummary, LookVersionPage, SelectionScope, SiteIndex, VersionLookShape,
    VersionSummary, look_shape,
};
use looklab_schema::{Applicability, TargetEligibilityStatus};

/// The selected target version; no fallback to a maximum or a legacy artifact.
pub fn catalog_version(look: &LookSummary) -> Option<&VersionSummary> {
    let selected = look.selected.as_ref()?;
    look.versions
        .iter()
        .find(|entry| entry.version == selected.version)
}

/// A version a catalog may display for browsing; never a qualified selection.
pub fn browsable_version(look: &LookSummary) -> Option<&VersionSummary> {
    catalog_version(look).or_else(|| {
        look.versions
            .iter()
            .max_by(|left, right| looklab_publish::compare_versions(&left.version, &right.version))
    })
}

pub fn browsable_scope(look: &LookSummary) -> (SelectionScope, Vec<String>) {
    if look.selected.is_some() {
        catalog_scope(look)
    } else {
        (
            SelectionScope::Unassessed,
            vec!["This look has no current version selected.".to_owned()],
        )
    }
}

pub fn catalog_scope(look: &LookSummary) -> (SelectionScope, Vec<String>) {
    let Some(selected) = look.selected.as_ref() else {
        return (
            SelectionScope::Unassessed,
            vec!["This look has no current version selected.".to_owned()],
        );
    };
    let mut blockers = Vec::new();
    if !selected.applicability.is_current() {
        blockers.push(format!(
            "The selected version is {}.",
            crate::data::applicability_label(&selected.applicability)
        ));
    }
    let evidence = match look.kind {
        LookKind::NativeFamily => look
            .head
            .as_ref()
            .is_some_and(|head| head.version == selected.version && head.report.is_some()),
        LookKind::LutDefined => selected.eligibility.is_some(),
    };
    if !evidence {
        blockers.push("The selected version has not been checked for these settings.".to_owned());
    }
    let current = selected.applicability.is_current();
    let selected_version = look
        .versions
        .iter()
        .find(|entry| entry.version == selected.version);
    let matchable = selected_version.is_some_and(|entry| entry.matchable);
    let approximated = selected_version.is_some_and(|entry| entry.approximated);
    let scope = if current && approximated && matchable {
        SelectionScope::Experimental
    } else if current && evidence {
        SelectionScope::Qualified
    } else if current && matchable {
        SelectionScope::Experimental
    } else {
        SelectionScope::Unassessed
    };
    (scope, blockers)
}

pub fn catalog_version_scope(
    look: &LookSummary,
    version: &LookVersion,
) -> (SelectionScope, Vec<String>) {
    if look
        .selected
        .as_ref()
        .is_some_and(|selected| selected.version == *version)
    {
        catalog_scope(look)
    } else {
        (
            SelectionScope::Unassessed,
            vec![
                "This is an older version. It has not been checked against the current data."
                    .to_owned(),
            ],
        )
    }
}

pub fn canonical_version(page: &LookPage) -> Option<&LookVersionPage> {
    let selected = page.selected.as_ref()?;
    page.versions
        .iter()
        .find(|entry| entry.version() == &selected.version)
}

pub fn browsable_page_version(page: &LookPage) -> Option<&LookVersionPage> {
    canonical_version(page).or_else(|| {
        page.versions.iter().max_by(|left, right| {
            looklab_publish::compare_versions(left.version(), right.version())
        })
    })
}

/// The scope and blockers of one published version, including target-only card eligibility.
pub fn version_scope(
    page: &LookPage,
    index: &SiteIndex,
    version: &LookVersionPage,
) -> (SelectionScope, Vec<String>) {
    let mut blockers = Vec::new();
    if !version.applicability.is_current() {
        blockers.push(format!(
            "This version is {}.",
            crate::data::applicability_label(&version.applicability)
        ));
    }
    if matches!(
        look_shape(&version.manifest),
        Ok(VersionLookShape::SourcePreview)
    ) {
        blockers.push(
            "Preview only: no settings to match, no editing-app export and no card.".to_owned(),
        );
    }
    let selected = page
        .selected
        .as_ref()
        .is_some_and(|selected| selected.version == *version.version());
    if !selected {
        blockers.push("This is not the current version of this look.".to_owned());
    }
    let evidence = if page.head.as_ref().is_some_and(|head| {
        page.kind == LookKind::NativeFamily && head.version == *version.version()
    }) {
        page.head.as_ref().is_some_and(|head| head.report.is_some())
    } else if let Some(id) = &version.eligibility {
        match index.eligibility.iter().find(|entry| entry.id == *id) {
            Some(entry) if entry.status == TargetEligibilityStatus::Eligible => true,
            Some(_) => {
                blockers
                    .push("This version cannot be used for new matches or downloads.".to_owned());
                false
            }
            None => {
                blockers.push(
                    "This version cannot be used for new matches yet: its eligibility record is missing."
                        .to_owned(),
                );
                false
            }
        }
    } else {
        false
    };
    let current = matches!(&version.applicability, Applicability::Current);
    let scope = if !selected {
        SelectionScope::Unassessed
    } else if current && version.approximated && version.matchable {
        SelectionScope::Experimental
    } else if current && evidence {
        SelectionScope::Qualified
    } else if current
        && version.matchable
        && version
            .eligibility_status
            .is_none_or(|status| status == TargetEligibilityStatus::Eligible)
    {
        SelectionScope::Experimental
    } else {
        SelectionScope::Unassessed
    };
    (scope, blockers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
    use looklab_publish::{
        ArtifactHash, ArtifactName, FamilyHeadPage, LookStatus, LookSummary, LookVisibility,
        SitePath, VersionSummary,
    };
    use looklab_schema::{FamilyTargetId, LookId, SafetyCheck, TargetVersion};

    fn version_summary() -> VersionSummary {
        VersionSummary {
            support: looklab_publish::PublishedSupport::Unstated,
            version: LookVersion::new("1.0.0").expect("version"),
            status: LookStatus::Published,
            matchable: true,
            card_eligible: false,
            approximated: false,
            applicability: Applicability::Current,
            eligibility_status: None,
            card: None,
            manifest: SitePath::new("data/looks/provia/1.0.0/manifest.json").expect("path"),
            manifest_sha256: ArtifactHash::of(b""),
            manifest_bytes: 0,
            artifact: ArtifactName::new("look.looklab").expect("artifact"),
            sha256: ArtifactHash::of(b""),
            bytes: 0,
        }
    }

    fn summary(report: Option<looklab_schema::ModelReportId>) -> LookSummary {
        let target = FamilyTargetId {
            family: LookId::new("provia").expect("family"),
            version: TargetVersion::new(1).expect("target version"),
        };
        LookSummary {
            slug: Slug::new("provia").expect("slug"),
            name: "Provia".to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            visibility: LookVisibility::Public,
            head: Some(FamilyHeadPage {
                target,
                version: LookVersion::new("1.0.0").expect("version"),
                source_limited: report.is_none(),
                safety: looklab_schema::CandidateSafety {
                    numerical: SafetyCheck::Passed,
                    release_gates: SafetyCheck::Passed,
                },
                report,
                applicability: Applicability::Current,
            }),
            selected: Some(looklab_publish::SelectedTargetSummary {
                version: LookVersion::new("1.0.0").expect("version"),
                eligibility: None,
                applicability: Applicability::Current,
            }),
            versions: vec![version_summary()],
        }
    }

    #[test]
    fn a_source_limited_selected_target_is_experimental() {
        let look = summary(None);
        let (scope, blockers) = catalog_scope(&look);
        assert_eq!(scope, SelectionScope::Experimental);
        assert_eq!(
            blockers.len(),
            1,
            "the missing assessment is the only blocker"
        );
        assert_eq!(browsable_scope(&look).0, SelectionScope::Experimental);
        assert_eq!(
            crate::data::selection_scope_label(scope),
            crate::data::selection_scope_label(SelectionScope::Experimental)
        );
        assert_ne!(
            crate::data::selection_scope_label(scope),
            crate::data::selection_scope_label(SelectionScope::Qualified)
        );
    }

    #[test]
    fn a_selected_target_with_a_report_stays_qualified() {
        let look = summary(Some(looklab_schema::ModelReportId::from_digest([3; 32])));
        assert_eq!(catalog_scope(&look).0, SelectionScope::Qualified);
    }
}
