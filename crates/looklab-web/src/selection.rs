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
            vec!["No current target is selected for this look.".to_owned()],
        )
    }
}

pub fn catalog_scope(look: &LookSummary) -> (SelectionScope, Vec<String>) {
    let Some(selected) = look.selected.as_ref() else {
        return (
            SelectionScope::Unassessed,
            vec!["No current target is selected for this look.".to_owned()],
        );
    };
    let mut blockers = Vec::new();
    if !selected.applicability.is_current() {
        blockers.push(format!(
            "The selected target is {}.",
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
        blockers.push("The selected target has no applicable assessment.".to_owned());
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
            vec!["Pinned historical target; no current qualification is claimed.".to_owned()],
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
            "Source-LUT preview without fitted layers; it cannot be matched, exported or downloaded as a card."
                .to_owned(),
        );
    }
    let selected = page
        .selected
        .as_ref()
        .is_some_and(|selected| selected.version == *version.version());
    if !selected {
        blockers.push("This version is not the selected target for its look.".to_owned());
    }
    let evidence = if page.head.as_ref().is_some_and(|head| {
        page.kind == LookKind::NativeFamily && head.version == *version.version()
    }) {
        page.head.as_ref().is_some_and(|head| head.report.is_some())
    } else if let Some(id) = &version.eligibility {
        match index.eligibility.iter().find(|entry| entry.id == *id) {
            Some(entry) if entry.status == TargetEligibilityStatus::Eligible => true,
            Some(_) => {
                blockers.push("The target eligibility snapshot is not eligible.".to_owned());
                false
            }
            None => {
                blockers.push("The target eligibility snapshot is not published.".to_owned());
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
