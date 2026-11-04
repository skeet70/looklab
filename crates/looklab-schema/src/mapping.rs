// SPDX-License-Identifier: AGPL-3.0-or-later
//! Makernote tag/value inference from files labeled through the camera's menu
//! ([docs/08 §4](../../../docs/08-ui.md)).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::ids::{MakernoteTag, SimKey};
use crate::schema::MakernoteValue;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledFile {
    pub label: SimKey,
    pub tags: BTreeMap<MakernoteTag, MakernoteValue>,
}

pub const MENU_QUESTION: &str = "Which film simulation was this shot with?";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    #[error("label at least two files")]
    TooFewFiles,
    #[error("label the files with at least two different menu answers")]
    TooFewLabels,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MappingCandidate {
    pub tag: MakernoteTag,
    /// One value per menu answer, sorted by answer.
    pub pairs: Vec<(SimKey, MakernoteValue)>,
    pub confidence: f64,
    pub consistent: bool,
}

impl MappingCandidate {
    pub fn value_for(&self, label: &SimKey) -> Option<MakernoteValue> {
        self.pairs
            .iter()
            .find(|(candidate, _)| candidate == label)
            .map(|(_, value)| *value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MappingInference {
    /// Consistent tags first, then by confidence. Every label is covered.
    pub candidates: Vec<MappingCandidate>,
    pub labels: Vec<SimKey>,
}

impl MappingInference {
    pub fn best(&self) -> Option<&MappingCandidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.consistent)
    }
}

/// # Errors
/// Fewer than two files, or fewer than two distinct menu answers.
pub fn infer_mapping(files: &[LabeledFile]) -> Result<MappingInference, MappingError> {
    if files.len() < 2 {
        return Err(MappingError::TooFewFiles);
    }
    let mut labels = BTreeSet::new();
    for file in files {
        labels.insert(file.label.clone());
    }
    if labels.len() < 2 {
        return Err(MappingError::TooFewLabels);
    }
    let labels: Vec<SimKey> = labels.into_iter().collect();
    let tags = common_tags(files);

    let mut candidates: Vec<MappingCandidate> = tags
        .into_iter()
        .map(|tag| candidate(tag, files, &labels))
        .collect();
    candidates.sort_by(|a, b| {
        b.consistent
            .cmp(&a.consistent)
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| a.tag.cmp(&b.tag))
    });
    Ok(MappingInference { candidates, labels })
}

fn common_tags(files: &[LabeledFile]) -> Vec<MakernoteTag> {
    let mut common: Vec<MakernoteTag> = files
        .first()
        .map(|file| file.tags.keys().cloned().collect())
        .unwrap_or_default();
    for file in &files[1..] {
        common.retain(|tag| file.tags.contains_key(tag));
    }
    common
}

fn candidate(tag: MakernoteTag, files: &[LabeledFile], labels: &[SimKey]) -> MappingCandidate {
    let mut pairs = Vec::with_capacity(labels.len());
    let mut consistent = true;
    let mut values = BTreeSet::new();
    for label in labels {
        let observed: BTreeSet<MakernoteValue> = files
            .iter()
            .filter(|file| &file.label == label)
            .filter_map(|file| file.tags.get(&tag).copied())
            .collect();
        match observed.len() {
            1 => {
                let Some(value) = observed.into_iter().next() else {
                    consistent = false;
                    continue;
                };
                values.insert(value);
                pairs.push((label.clone(), value));
            }
            _ => consistent = false,
        }
    }
    if pairs.len() != labels.len() {
        consistent = false;
    }
    if consistent && values.len() != labels.len() {
        consistent = false;
    }
    let confidence = if consistent {
        1.0
    } else if pairs.len() == labels.len() {
        values.len() as f64 / labels.len() as f64
    } else {
        0.0
    };
    MappingCandidate {
        tag,
        pairs,
        confidence,
        consistent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str) -> MakernoteTag {
        MakernoteTag::new(name).expect("tag")
    }

    fn label(name: &str) -> SimKey {
        SimKey::new(name).expect("label")
    }

    fn file(label_name: &str, tags: &[(&str, u64)]) -> LabeledFile {
        LabeledFile {
            label: label(label_name),
            tags: tags
                .iter()
                .map(|(name, value)| (tag(name), MakernoteValue(*value)))
                .collect(),
        }
    }

    #[test]
    fn a_consistent_tag_separates_the_labels() {
        let files = vec![
            file(
                "classic_chrome",
                &[("FilmMode", 0x0700), ("GrainSize", 0x2)],
            ),
            file("provia", &[("FilmMode", 0x0000), ("GrainSize", 0x2)]),
            file("acros", &[("FilmMode", 0x0500), ("GrainSize", 0x2)]),
        ];
        let inference = infer_mapping(&files).expect("inference");
        let best = inference.best().expect("a consistent tag");
        assert_eq!(best.tag.as_str(), "FilmMode");
        assert_eq!(best.confidence, 1.0);
        assert_eq!(
            best.value_for(&label("acros")),
            Some(MakernoteValue(0x0500))
        );
        let grain = inference
            .candidates
            .iter()
            .find(|candidate| candidate.tag.as_str() == "GrainSize")
            .expect("shared value candidate");
        assert!(!grain.consistent);
        assert!(grain.confidence < 1.0);
    }

    #[test]
    fn a_shared_value_lowers_confidence_and_is_not_chosen() {
        let files = vec![
            file("classic_chrome", &[("FilmMode", 0x0700)]),
            file("provia", &[("FilmMode", 0x0700)]),
        ];
        let inference = infer_mapping(&files).expect("inference");
        assert!(inference.best().is_none());
        assert_eq!(inference.candidates[0].confidence, 0.5);
    }

    #[test]
    fn a_label_with_two_values_is_inconsistent() {
        let files = vec![
            file("classic_chrome", &[("FilmMode", 0x0700)]),
            file("classic_chrome", &[("FilmMode", 0x0300)]),
            file("provia", &[("FilmMode", 0x0000)]),
        ];
        let inference = infer_mapping(&files).expect("inference");
        assert!(inference.best().is_none());
        assert_eq!(inference.candidates[0].confidence, 0.0);
    }

    #[test]
    fn a_tag_missing_from_one_file_is_not_a_candidate() {
        let files = vec![
            file("classic_chrome", &[("FilmMode", 0x0700), ("Hidden", 1)]),
            file("provia", &[("FilmMode", 0x0000)]),
        ];
        let inference = infer_mapping(&files).expect("inference");
        assert!(
            inference
                .candidates
                .iter()
                .all(|candidate| candidate.tag.as_str() != "Hidden")
        );
    }

    #[test]
    fn too_few_files_or_labels_are_errors() {
        assert_eq!(
            infer_mapping(&[file("classic_chrome", &[("FilmMode", 0x0700)])]),
            Err(MappingError::TooFewFiles)
        );
        assert_eq!(
            infer_mapping(&[
                file("classic_chrome", &[("FilmMode", 0x0700)]),
                file("classic_chrome", &[("FilmMode", 0x0700)]),
            ]),
            Err(MappingError::TooFewLabels)
        );
    }
}
