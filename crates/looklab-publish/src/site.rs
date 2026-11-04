// SPDX-License-Identifier: AGPL-3.0-or-later
//! Site data layout and the sink-based publisher (docs/03 §5).

use std::io;

use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
use looklab_schema::BodyId;
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::audit::AuditLog;
use crate::body::{BodyKind, BodyPage, BodyStatus, MappingStatus};
use crate::hash::{ArtifactHash, ArtifactName, SitePath};
use crate::manifest::{
    ArtifactBlob, ArtifactEntry, GateSummary, LookStatus, ProvenanceSummary, VersionManifest,
    verify_artifact,
};
use crate::scene::{SCENE_FORMAT, Scene, SceneIndex, SceneName, SceneSummary};

pub const SITE_FORMAT: &str = "looklab-site/1";
pub const INDEX_PATH: &str = "data/index.json";
pub const AUDIT_LOG_PATH: &str = "data/audit-log.json";
pub const SCENE_INDEX_PATH: &str = "data/scenes/index.json";
pub const MANIFEST_NAME: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicensePage {
    pub declaration: String,
    pub attribution: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeakSpot {
    pub region: String,
    pub note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounty: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookVersionPage {
    pub manifest: VersionManifest,
    #[serde(skip)]
    pub blobs: Vec<ArtifactBlob>,
    pub notes: VersionNotes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionNotes {
    pub changelog: Vec<String>,
    pub weak_spots: Vec<WeakSpot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

impl LookVersionPage {
    /// # Errors
    /// Rejects an empty artifact list or duplicate artifact names.
    pub fn new(
        look: Slug,
        version: LookVersion,
        status: LookStatus,
        gate_summary: GateSummary,
        provenance: ProvenanceSummary,
        blobs: Vec<ArtifactBlob>,
        notes: VersionNotes,
    ) -> Result<Self, PublishError> {
        let manifest =
            VersionManifest::from_files(look, version, status, gate_summary, provenance, &blobs)?;
        Ok(Self {
            manifest,
            blobs,
            notes,
        })
    }

    pub fn version(&self) -> &LookVersion {
        &self.manifest.version
    }

    pub fn artifacts(&self) -> &[ArtifactEntry] {
        &self.manifest.artifacts
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookPage {
    pub slug: Slug,
    pub name: String,
    pub kind: LookKind,
    pub wb_policy: WbPolicy,
    pub description: String,
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<LicensePage>,
    pub versions: Vec<LookVersionPage>,
}

impl LookPage {
    pub fn latest(&self) -> Option<&LookVersionPage> {
        self.versions
            .iter()
            .max_by(|left, right| left.version().cmp(right.version()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub version: LookVersion,
    pub status: LookStatus,
    pub manifest: SitePath,
    pub artifact: ArtifactName,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookSummary {
    pub slug: Slug,
    pub name: String,
    pub kind: LookKind,
    pub wb_policy: WbPolicy,
    pub versions: Vec<VersionSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodySummary {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    pub kind: BodyKind,
    pub status: BodyStatus,
    pub mapping_status: MappingStatus,
    pub epochs: usize,
    pub looks: Vec<Slug>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteIndex {
    pub format: String,
    pub bodies: Vec<BodySummary>,
    pub looks: Vec<LookSummary>,
    pub scenes: Vec<SceneSummary>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteData {
    pub bodies: Vec<BodyPage>,
    pub looks: Vec<LookPage>,
    pub audit_log: AuditLog,
    pub scenes: Vec<Scene>,
}

/// A consumer of published files, such as a directory writer or an in-memory test sink.
pub trait SiteSink {
    fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()>;
}

impl<F> SiteSink for F
where
    F: FnMut(&SitePath, &[u8]) -> io::Result<()>,
{
    fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()> {
        self(path, bytes)
    }
}

pub fn index_path() -> SitePath {
    SitePath::new(INDEX_PATH).expect("static site path")
}

pub fn audit_log_path() -> SitePath {
    SitePath::new(AUDIT_LOG_PATH).expect("static site path")
}

pub fn scene_index_path() -> SitePath {
    SitePath::new(SCENE_INDEX_PATH).expect("static site path")
}

/// # Errors
/// Rejects a body id whose segments are not safe path components.
pub fn body_path(id: &BodyId) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/bodies/{}/{}.json", id.make(), id.model()))
}

/// # Errors
/// Rejects a look slug that is not a safe path component.
pub fn look_path(slug: &Slug) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/looks/{}.json", slug.as_str()))
}

/// # Errors
/// Rejects a look slug or version that is not a safe path component.
pub fn version_dir(slug: &Slug, version: &LookVersion) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/looks/{}/{}", slug.as_str(), version.as_str()))
}

/// # Errors
/// Rejects a scene name that is not a safe path component.
pub fn scene_path(name: &SceneName) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/scenes/{}.f16", name.as_str()))
}

/// # Errors
/// Rejects a look slug or version that is not a safe path component.
pub fn manifest_path(slug: &Slug, version: &LookVersion) -> Result<SitePath, PublishError> {
    SitePath::new(format!(
        "data/looks/{}/{}/{}",
        slug.as_str(),
        version.as_str(),
        MANIFEST_NAME
    ))
}

/// Writes every published file through `sink`, including artifacts and manifests.
///
/// # Errors
/// Rejects duplicate ids and unsafe paths, and propagates sink failures.
pub fn write_site<S: SiteSink + ?Sized>(sink: &mut S, site: &SiteData) -> Result<(), PublishError> {
    validate(site)?;

    for body in &site.bodies {
        let mut page = body.clone();
        for card in &mut page.native_looks {
            if let Some(look) = site.looks.iter().find(|look| look.slug == card.look)
                && let Some(latest) = look.latest()
            {
                card.version = Some(latest.manifest.version.clone());
                card.status = Some(latest.manifest.status);
            }
        }
        sink.put(&body_path(&page.id)?, &json(&page)?)?;
    }

    for look in &site.looks {
        let page_path = look_path(&look.slug)?;
        sink.put(&page_path, &json(look)?)?;
        for version in &look.versions {
            let dir = version_dir(&look.slug, &version.manifest.version)?;
            for blob in &version.blobs {
                verify_artifact(&version.manifest, &blob.name, &blob.bytes)?;
                sink.put(&dir.join(blob.name.as_str()), &blob.bytes)?;
            }
            sink.put(
                &manifest_path(&look.slug, &version.manifest.version)?,
                &json(&version.manifest)?,
            )?;
        }
    }

    for scene in &site.scenes {
        sink.put(&scene_path(&scene.name)?, &scene.to_f16_bytes())?;
    }
    sink.put(&scene_index_path(), &json(&scene_index(site)?)?)?;
    sink.put(&audit_log_path(), &json(&site.audit_log)?)?;
    sink.put(&index_path(), &json(&site_index(site)?)?)?;
    Ok(())
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>, PublishError> {
    Ok(serde_json::to_vec_pretty(value)?)
}

fn scene_index(site: &SiteData) -> Result<SceneIndex, PublishError> {
    let scenes = site
        .scenes
        .iter()
        .map(|scene| {
            Ok(SceneSummary {
                name: scene.name.clone(),
                width: scene.width,
                height: scene.height,
                path: scene_path(&scene.name)?,
            })
        })
        .collect::<Result<Vec<_>, PublishError>>()?;
    Ok(SceneIndex {
        format: SCENE_FORMAT.to_owned(),
        scenes,
    })
}

fn site_index(site: &SiteData) -> Result<SiteIndex, PublishError> {
    let bodies = site
        .bodies
        .iter()
        .map(|body| {
            let mut looks: Vec<Slug> = body
                .native_looks
                .iter()
                .map(|card| card.look.clone())
                .collect();
            looks.sort();
            looks.dedup();
            BodySummary {
                id: body.id.clone(),
                make: body.make.clone(),
                model: body.model.clone(),
                kind: body.kind,
                status: body.status,
                mapping_status: body.mapping_status,
                epochs: body.epochs.len(),
                looks,
            }
        })
        .collect();
    let looks = site
        .looks
        .iter()
        .map(|look| {
            let versions =
                look.versions
                    .iter()
                    .map(|version| {
                        let first = version.artifacts().first().ok_or_else(|| {
                            PublishError::NoArtifacts {
                                look: look.slug.as_str().to_owned(),
                                version: version.version().as_str().to_owned(),
                            }
                        })?;
                        Ok(VersionSummary {
                            version: version.version().clone(),
                            status: version.manifest.status,
                            manifest: manifest_path(&look.slug, version.version())?,
                            artifact: first.name.clone(),
                            sha256: first.sha256.clone(),
                            bytes: first.bytes,
                        })
                    })
                    .collect::<Result<Vec<_>, PublishError>>()?;
            Ok(LookSummary {
                slug: look.slug.clone(),
                name: look.name.clone(),
                kind: look.kind,
                wb_policy: look.wb_policy,
                versions,
            })
        })
        .collect::<Result<Vec<_>, PublishError>>()?;
    Ok(SiteIndex {
        format: SITE_FORMAT.to_owned(),
        bodies,
        looks,
        scenes: scene_index(site)?.scenes,
    })
}

fn validate(site: &SiteData) -> Result<(), PublishError> {
    let mut body_ids = Vec::with_capacity(site.bodies.len());
    for body in &site.bodies {
        if body_ids.contains(&body.id) {
            return Err(PublishError::DuplicateBody(body.id.to_string()));
        }
        body_ids.push(body.id.clone());
        body_path(&body.id)?;
    }

    let mut look_slugs = Vec::with_capacity(site.looks.len());
    for look in &site.looks {
        if look_slugs.contains(&look.slug) {
            return Err(PublishError::DuplicateLook(look.slug.to_string()));
        }
        look_slugs.push(look.slug.clone());
        look_path(&look.slug)?;
        let mut versions = Vec::with_capacity(look.versions.len());
        for version in &look.versions {
            if versions.contains(&version.manifest.version) {
                return Err(PublishError::DuplicateVersion {
                    look: look.slug.to_string(),
                    version: version.version().to_string(),
                });
            }
            if version.manifest.look != look.slug {
                return Err(PublishError::ManifestLook {
                    expected: look.slug.to_string(),
                    found: version.manifest.look.to_string(),
                });
            }
            versions.push(version.manifest.version.clone());
            version_dir(&look.slug, version.version())?;
            for blob in &version.blobs {
                verify_artifact(&version.manifest, &blob.name, &blob.bytes)?;
            }
            if version.blobs.is_empty() {
                return Err(PublishError::NoArtifacts {
                    look: look.slug.to_string(),
                    version: version.version().to_string(),
                });
            }
        }
    }

    let mut scene_names = Vec::with_capacity(site.scenes.len());
    for scene in &site.scenes {
        if scene_names.contains(&scene.name) {
            return Err(PublishError::DuplicateScene(scene.name.to_string()));
        }
        scene_names.push(scene.name.clone());
        scene_path(&scene.name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
    use looklab_schema::{validate_body, validate_control_schema};

    use super::*;
    use crate::audit::AuditLog;
    use crate::body::{BodyExtras, BodyPage};
    use crate::hash::ArtifactName;
    use crate::manifest::LookStatus;
    use crate::manifest::ProvenanceSummary;

    struct MemorySink(BTreeMap<String, Vec<u8>>);

    impl SiteSink for MemorySink {
        fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()> {
            self.0.insert(path.as_str().to_owned(), bytes.to_vec());
            Ok(())
        }
    }

    fn body() -> BodyPage {
        let body = validate_body(
            "id: fujifilm/x-t5\nmake: Fujifilm\nmodel: X-T5\nexif_aliases: [X-T5]\nkind: camera\ncapabilities:\n  in_camera_raw_conversion: true\n  jpeg_color_spaces: [srgb]\n  heif: false\nstatus: active\nmapping_status: provisional\n",
        )
        .expect("body");
        let schema =
            validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml"))
                .expect("schema");
        BodyPage::from_schema(&body, &schema, &BodyExtras::default()).expect("page")
    }

    fn look() -> LookPage {
        let blob = ArtifactBlob::new(
            ArtifactName::new("look.looklab").expect("name"),
            vec![1, 2, 3, 4],
        );
        let version = LookVersionPage::new(
            Slug::new("provia").expect("slug"),
            LookVersion::new("1.0.0").expect("version"),
            LookStatus::Published,
            GateSummary::default(),
            ProvenanceSummary::default(),
            vec![blob],
            VersionNotes {
                changelog: vec!["initial".to_owned()],
                weak_spots: Vec::new(),
                notice: None,
            },
        )
        .expect("version");
        LookPage {
            slug: Slug::new("provia").expect("slug"),
            name: "Provia".to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            description: "Standard".to_owned(),
            aliases: Vec::new(),
            license: None,
            versions: vec![version],
        }
    }

    fn scene() -> Scene {
        Scene::new(
            SceneName::new("gradient").expect("name"),
            1,
            1,
            vec![[0.18, 0.18, 0.18]],
        )
        .expect("scene")
    }

    #[test]
    fn writer_emits_every_indexed_file() {
        let site = SiteData {
            bodies: vec![body()],
            looks: vec![look()],
            audit_log: AuditLog::default(),
            scenes: vec![scene()],
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        for path in [
            "data/index.json",
            "data/audit-log.json",
            "data/scenes/index.json",
            "data/bodies/fujifilm/x-t5.json",
            "data/looks/provia.json",
            "data/looks/provia/1.0.0/look.looklab",
            "data/looks/provia/1.0.0/manifest.json",
            "data/scenes/gradient.f16",
        ] {
            assert!(sink.0.contains_key(path), "{path}");
        }
    }

    #[test]
    fn index_links_the_manifest_and_the_body_gets_the_latest_version() {
        let site = SiteData {
            bodies: vec![body()],
            looks: vec![look()],
            audit_log: AuditLog::default(),
            scenes: vec![scene()],
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex =
            serde_json::from_slice(&sink.0["data/index.json"]).expect("index json");
        assert_eq!(
            index.looks[0].versions[0].manifest.as_str(),
            "data/looks/provia/1.0.0/manifest.json"
        );
        let page: BodyPage =
            serde_json::from_slice(&sink.0["data/bodies/fujifilm/x-t5.json"]).expect("body json");
        assert_eq!(
            page.native_looks[1]
                .version
                .as_ref()
                .expect("version")
                .as_str(),
            "1.0.0"
        );
    }

    #[test]
    fn writer_rejects_duplicate_looks() {
        let site = SiteData {
            bodies: Vec::new(),
            looks: vec![look(), look()],
            audit_log: AuditLog::default(),
            scenes: Vec::new(),
        };
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(&mut sink, &site),
            Err(PublishError::DuplicateLook(_))
        ));
    }
}
