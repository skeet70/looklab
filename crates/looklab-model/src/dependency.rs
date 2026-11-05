// SPDX-License-Identifier: AGPL-3.0-or-later
//! Records which LUT-defined Look versions pin which display base (docs/05 §8).

use serde::{Deserialize, Serialize};

use crate::file::{BaseReference, LookVersion, Slug};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseDependency {
    pub look: Slug,
    pub version: LookVersion,
    pub base: BaseReference,
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_rebase: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl BaseDependency {
    pub fn new(look: Slug, version: LookVersion, base: BaseReference) -> Self {
        Self {
            look,
            version,
            base,
            needs_rebase: false,
        }
    }
}

pub fn mark_dependents(dependencies: &mut [BaseDependency], changed: &BaseReference) -> usize {
    let mut marked = 0;
    for dependency in dependencies {
        if dependency.base == *changed && !dependency.needs_rebase {
            dependency.needs_rebase = true;
            marked += 1;
        }
    }
    marked
}

pub fn needs_rebase(dependencies: &[BaseDependency]) -> Vec<&BaseDependency> {
    dependencies
        .iter()
        .filter(|dependency| dependency.needs_rebase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency(slug: &str, version: &str) -> BaseDependency {
        BaseDependency::new(
            Slug::new(slug).expect("slug"),
            LookVersion::new(version).expect("version"),
            BaseReference::neutral_baseline(),
        )
    }

    #[test]
    fn a_reverted_base_marks_only_its_pins() {
        let base = BaseReference::neutral_baseline();
        let mut dependencies = vec![dependency("a", "1.0.0"), dependency("b", "1.0.0")];
        assert_eq!(mark_dependents(&mut dependencies, &base), 2);
        assert_eq!(mark_dependents(&mut dependencies, &base), 0);
        assert_eq!(needs_rebase(&dependencies).len(), 2);

        let other = BaseReference::Look {
            slug: Slug::new("some-base").expect("slug"),
            version: LookVersion::new("2.0.0").expect("version"),
        };
        assert_eq!(mark_dependents(&mut dependencies, &other), 0);
    }
}
