// SPDX-License-Identifier: AGPL-3.0-or-later
//! The documentation-derived camera catalog loaded at startup.

use std::path::Path;

use looklab_schema::{
    BodyId, ManualCamera, ManualCatalog, Sha256Hex, manual_catalog_digest,
    manual_catalog_files_digest, merge_manual_catalogs, validate_manual_catalog,
};

#[derive(Debug)]
pub struct Manuals {
    catalog: ManualCatalog,
    digest: Sha256Hex,
}

#[derive(Debug, thiserror::Error)]
pub enum ManualsError {
    #[error("manuals: {0}")]
    Io(#[from] std::io::Error),
    #[error("manuals are invalid: {0}")]
    Invalid(String),
}

impl Manuals {
    /// # Errors
    /// An unreadable path or a catalog that fails validation.
    pub fn load(path: &Path) -> Result<Self, ManualsError> {
        if path.is_dir() {
            let mut files = Vec::new();
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let file = entry.path();
                if file.extension().and_then(|value| value.to_str()) != Some("yaml") {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                files.push((name, std::fs::read_to_string(&file)?));
            }
            files.sort_by(|left, right| left.0.cmp(&right.0));
            let mut catalogs = Vec::with_capacity(files.len());
            for (name, source) in &files {
                let catalog = validate_manual_catalog(source)
                    .map_err(|errors| ManualsError::Invalid(format!("{name}: {errors:?}")))?;
                catalogs.push(catalog);
            }
            let catalog = merge_manual_catalogs(catalogs)
                .map_err(|errors| ManualsError::Invalid(format!("{path:?}: {errors:?}")))?;
            return Ok(Self {
                digest: manual_catalog_files_digest(&files),
                catalog,
            });
        }
        if !path.is_file() {
            return Ok(Self {
                catalog: ManualCatalog {
                    catalog_version: looklab_schema::MANUAL_CATALOG_VERSION,
                    cameras: Vec::new(),
                },
                digest: manual_catalog_digest(""),
            });
        }
        let source = std::fs::read_to_string(path)?;
        let catalog = validate_manual_catalog(&source)
            .map_err(|errors| ManualsError::Invalid(format!("{path:?}: {errors:?}")))?;
        Ok(Self {
            digest: manual_catalog_digest(&source),
            catalog,
        })
    }

    pub fn catalog(&self) -> &ManualCatalog {
        &self.catalog
    }

    pub fn digest(&self) -> Sha256Hex {
        self.digest
    }

    pub fn camera(&self, id: &BodyId) -> Option<&ManualCamera> {
        self.catalog.camera(id)
    }

    pub fn camera_by_make_model(&self, make: &str, model: &str) -> Option<&ManualCamera> {
        self.catalog.camera_by_make_model(make, model)
    }
}
