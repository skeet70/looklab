// SPDX-License-Identifier: AGPL-3.0-or-later
//! Build-time identities of the numerical implementation.

use std::sync::LazyLock;

use looklab_schema::{DependencyRef, Sha256Hex, SourceRef};
use looklab_server::model_store::SourceActivation;

include!(concat!(env!("OUT_DIR"), "/source_identity.rs"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericalSources {
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
}

impl NumericalSources {
    /// The dependency edges a report or realization revision records and rechecks.
    pub fn dependencies(&self) -> Vec<DependencyRef> {
        self.storage().dependencies()
    }

    pub fn storage(&self) -> looklab_server::model_store::NumericalSources {
        looklab_server::model_store::NumericalSources {
            fitter: self.fitter.clone(),
            evaluator: self.evaluator.clone(),
            model: self.model.clone(),
        }
    }
}

static SOURCES: LazyLock<NumericalSources> = LazyLock::new(|| NumericalSources {
    fitter: source(FITTER),
    evaluator: source(EVALUATOR),
    model: source(MODEL),
});

pub fn sources() -> &'static NumericalSources {
    &SOURCES
}

/// Digest that binds persisted resume state to the source identities above.
pub fn checkpoint_identity() -> Sha256Hex {
    Sha256Hex::parse(CHECKPOINT_IDENTITY)
        .expect("build-time checkpoint identity is a lowercase sha256 digest")
}

/// Registers `sources()` once for this database and invalidates the previous
/// implementation's dependency generations in the same transaction.
///
/// Call from `Worker::open`, never per job.
///
/// # Errors
/// The activation row or a dependency generation cannot be read or written.
pub async fn activate(pool: &sqlx::SqlitePool) -> crate::Result<SourceActivation> {
    let storage = sources().storage();
    Ok(
        looklab_server::model_store::sync_active_numerical_sources_fenced(
            pool,
            &storage,
            crate::clock::now_unix(),
        )
        .await?,
    )
}

fn source(value: &'static str) -> SourceRef {
    SourceRef::new(value).expect("build-time source identity is a valid SourceRef")
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_schema::DependencyKind;

    #[test]
    fn sources_are_distinct_and_dependency_ready() {
        let sources = sources();
        let dependencies = sources.dependencies();
        assert_eq!(dependencies.len(), 3);
        for dependency in &dependencies {
            assert_eq!(dependency.kind, DependencyKind::ModelSource);
            assert!(dependency.required);
        }
        assert_ne!(sources.fitter, sources.evaluator);
        assert_ne!(sources.evaluator, sources.model);
        assert_ne!(checkpoint_identity(), Sha256Hex::from_bytes([0; 32]));
    }

    #[test]
    fn dependency_targets_are_the_source_refs() {
        let sources = sources();
        let dependencies = sources.dependencies();
        let targets: Vec<&str> = dependencies
            .iter()
            .map(|dependency| dependency.target.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![
                sources.fitter.as_str(),
                sources.evaluator.as_str(),
                sources.model.as_str()
            ]
        );
    }
}
