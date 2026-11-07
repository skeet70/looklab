// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writes the synthetic match case into the static site tree.

use std::path::{Path, PathBuf};

use looklab_color::Mat3;
use looklab_match::ControlCovariance;
use looklab_match::synth;
use looklab_model::LookDocument;
use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy, identity};
use looklab_model::realization::encode_crm;
use looklab_publish::{
    ArtifactHash, ArtifactName, LookStatus, LookSummary, LookVisibility, MatchBundle, MatchSim,
    SiteIndex, SitePath, VersionSummary, body_artifact, bundle_artifact, transform_artifact,
};
use looklab_schema::SimKey;
use serde::Serialize;

const SLUG: &str = "synthetic-match";
const VERSION: &str = "1.0.0";
const BODY: &str = "fujifilm/x-t5";

#[expect(
    clippy::disallowed_methods,
    reason = "native fixture publication reads and writes site artifacts"
)]
fn main() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "workspace root".to_owned())?;
    let site = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("site"));
    let version_dir = site.join("data/looks").join(SLUG).join(VERSION);
    if version_dir.exists() {
        std::fs::remove_dir_all(&version_dir).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&version_dir).map_err(|error| error.to_string())?;

    let classic_sim = SimKey::new(synth::SYNTH_SIM).map_err(|error| error.to_string())?;
    let provia_sim = SimKey::new("provia").map_err(|error| error.to_string())?;
    let classic = synth::body(9).map_err(|error| error.to_string())?;
    let provia = synth::body_for_sim(10, &provia_sim).map_err(|error| error.to_string())?;
    let document = LookDocument::from_look(
        identity(SLUG, VERSION).map_err(|error| error.to_string())?,
        classic.model.base_look.clone(),
    )
    .map_err(|error| error.to_string())?;
    let look_bytes = document.encode().map_err(|error| error.to_string())?;
    let lut = looklab_publish::bake_preview_lut(&document);
    let body_yaml = std::fs::read_to_string(root.join("data/bodies/fujifilm-x-t5.yaml"))
        .map_err(|error| error.to_string())?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    let body_id = schema.body.clone();
    let transform = transform_yaml();

    let classic_names =
        looklab_publish::sim_artifacts(&body_id, 1, &classic_sim).map_err(|e| e.to_string())?;
    let provia_names =
        looklab_publish::sim_artifacts(&body_id, 1, &provia_sim).map_err(|e| e.to_string())?;
    let classic_base = look_bytes.clone();
    let classic_crm = encode_crm(&classic.model.crm).map_err(|error| error.to_string())?;
    let classic_covariance = diagonal_covariance(&classic.model.crm)
        .to_json()
        .map_err(|error| error.to_string())?;
    let provia_base = LookDocument::from_look(
        identity(&format!("{SLUG}-provia"), VERSION).map_err(|error| error.to_string())?,
        provia.model.base_look.clone(),
    )
    .map_err(|error| error.to_string())?
    .encode()
    .map_err(|error| error.to_string())?;
    let provia_crm = encode_crm(&provia.model.crm).map_err(|error| error.to_string())?;
    let provia_covariance = diagonal_covariance(&provia.model.crm)
        .to_json()
        .map_err(|error| error.to_string())?;

    let bundle = MatchBundle::new(
        body_id.clone(),
        1,
        vec![
            MatchSim {
                sim: classic_sim,
                base: classic_names.base.clone(),
                crm: classic_names.crm.clone(),
                covariance: Some(classic_names.covariance.clone()),
            },
            MatchSim {
                sim: provia_sim,
                base: provia_names.base.clone(),
                crm: provia_names.crm.clone(),
                covariance: Some(provia_names.covariance.clone()),
            },
        ],
        Vec::new(),
    )
    .map_err(|error| error.to_string())?;
    let bundle_bytes = bundle.to_json().map_err(|error| error.to_string())?;
    let bundle_name = bundle_artifact(&body_id, 1).map_err(|error| error.to_string())?;
    let body_name = body_artifact(&body_id, 1).map_err(|error| error.to_string())?;
    let transform_name = transform_artifact(&body_id, 1).map_err(|error| error.to_string())?;

    let artifacts: [(&str, &[u8]); 10] = [
        ("look.looklab", look_bytes.as_slice()),
        ("preview.lut33", lut.as_slice()),
        (body_name.as_str(), body_yaml.as_bytes()),
        (transform_name.as_str(), transform.as_bytes()),
        (classic_names.base.as_str(), classic_base.as_slice()),
        (classic_names.crm.as_str(), classic_crm.as_slice()),
        (
            classic_names.covariance.as_str(),
            classic_covariance.as_slice(),
        ),
        (provia_names.base.as_str(), provia_base.as_slice()),
        (provia_names.crm.as_str(), provia_crm.as_slice()),
        (
            provia_names.covariance.as_str(),
            provia_covariance.as_slice(),
        ),
    ];
    for (name, bytes) in artifacts {
        std::fs::write(version_dir.join(name), bytes).map_err(|error| error.to_string())?;
    }
    std::fs::write(version_dir.join(bundle_name.as_str()), &bundle_bytes)
        .map_err(|error| error.to_string())?;
    let mut entries = artifact_entries(&artifacts);
    entries.push(serde_json::json!({
        "name": bundle_name.as_str(),
        "sha256": ArtifactHash::of(&bundle_bytes).as_str(),
        "bytes": bundle_bytes.len(),
    }));
    let manifest = manifest_value(&entries);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    std::fs::write(version_dir.join("manifest.json"), &manifest_bytes)
        .map_err(|error| error.to_string())?;

    std::fs::write(
        site.join("data/looks").join(format!("{SLUG}.json")),
        serde_json::to_vec_pretty(&look_page_value(&manifest))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let index_path = site.join("data/index.json");
    let index_bytes = std::fs::read(&index_path).map_err(|error| error.to_string())?;
    let mut index: SiteIndex =
        serde_json::from_slice(&index_bytes).map_err(|error| error.to_string())?;
    upsert_index(&mut index, &manifest_bytes, &look_bytes)?;
    let new_index = serde_json::to_vec_pretty(&index).map_err(|error| error.to_string())?;
    std::fs::write(&index_path, &new_index).map_err(|error| error.to_string())?;
    std::fs::write(
        site.join("site-root.txt"),
        ArtifactHash::of(&new_index).as_str(),
    )
    .map_err(|error| error.to_string())?;

    println!("wrote {}", version_dir.display());
    Ok(())
}

fn diagonal_covariance(
    crm: &looklab_model::realization::ControlResponseModel,
) -> ControlCovariance {
    let ids = crm.parameter_ids();
    let n = ids.len();
    let mut covariance = vec![0.0_f64; n * n];
    for index in 0..n {
        covariance[index * n + index] = 1.0e-4;
    }
    ControlCovariance {
        parameter_ids: ids,
        covariance,
    }
}

fn artifact_entries(artifacts: &[(&str, &[u8])]) -> Vec<serde_json::Value> {
    artifacts
        .iter()
        .map(|(name, bytes)| {
            serde_json::json!({
                "name": name,
                "sha256": ArtifactHash::of(bytes).as_str(),
                "bytes": bytes.len(),
            })
        })
        .collect()
}

fn manifest_value(entries: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "look": SLUG,
        "version": VERSION,
        "artifacts": entries,
        "status": "published",
        "gate_summary": {
            "checks": [
                {"name": "synthetic case", "passed": true, "detail": "generated by publish_match_fixture"},
                {"name": "artifact manifest", "passed": true, "detail": "every artifact listed by SHA-256"}
            ]
        },
        "provenance": {
            "bodies": [{
                "body": BODY,
                "epochs": [1],
                "images": 0,
                "credentials": 0
            }],
            "root_admissions": [],
            "anchor_version": VERSION,
            "community_snapshot_version": 1,
            "prep_versions": [1],
            "created": "2026-11-04T00:00:00Z",
            "software": "looklab-wasm publish_match_fixture"
        }
    })
}

fn look_page_value(manifest: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "slug": SLUG,
        "name": "Synthetic Match Case",
        "kind": "native_family",
        "wb_policy": "adaptive",
        "description": "A reproducible synthetic body and Look for the browser match and verification flows.",
        "aliases": [],
        "license": {
            "declaration": "Project-owned synthetic data",
            "attribution": "looklab"
        },
        "versions": [{
            "manifest": manifest,
            "notes": {
                "changelog": ["generated by looklab-wasm publish_match_fixture"],
                "weak_spots": []
            }
        }]
    })
}

fn upsert_index(
    index: &mut SiteIndex,
    manifest_bytes: &[u8],
    look_bytes: &[u8],
) -> Result<(), String> {
    index.looks.retain(|look| look.slug.as_str() != SLUG);
    index.looks.push(LookSummary {
        slug: Slug::new(SLUG).map_err(|error| error.to_string())?,
        name: "Synthetic Match Case".to_owned(),
        kind: LookKind::NativeFamily,
        visibility: LookVisibility::TestFixture,
        wb_policy: WbPolicy::Adaptive,
        versions: vec![VersionSummary {
            version: LookVersion::new(VERSION).map_err(|error| error.to_string())?,
            status: LookStatus::Published,
            matchable: true,
            manifest: SitePath::new(format!("data/looks/{SLUG}/{VERSION}/manifest.json"))
                .map_err(|error| error.to_string())?,
            manifest_sha256: ArtifactHash::of(manifest_bytes),
            manifest_bytes: manifest_bytes.len() as u64,
            artifact: ArtifactName::new("look.looklab").map_err(|error| error.to_string())?,
            sha256: ArtifactHash::of(look_bytes),
            bytes: look_bytes.len() as u64,
        }],
    });
    Ok(())
}

#[derive(Serialize)]
struct TransformOut {
    format: String,
    version: u32,
    camera_to_xyz: Mat3,
}

fn transform_yaml() -> String {
    serde_norway::to_string(&TransformOut {
        format: "looklab-transform/1".to_owned(),
        version: 1,
        camera_to_xyz: synth::transform(),
    })
    .expect("transform serializes")
}
