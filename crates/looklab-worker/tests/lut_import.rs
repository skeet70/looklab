// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_export::{CubeFile, Lut3d, write_cube};
use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, BodyProvenance, CameraRendering, DeclaredEncoding,
    LookVersion, OutputEncoding, Provenance, RootProvenance,
};
use looklab_model::{Look, LookDocument, LookIdentity, LookKind, WbPolicy};
use looklab_publish::{
    ArtifactEntry, ArtifactHash, ArtifactName, LUT_BYTES, PREVIEW_LUT_ARTIFACT, PreviewBaseline,
    SOURCE_CUBE_ARTIFACT, SOURCE_LUT_ARTIFACT, SourceLutFormat, SourceLutPreview,
    SourceLutPreviewContext, bake_lut_preview,
};
use looklab_schema::{
    ArtifactBinding, ArtifactRole, EligibilitySnapshotId, LicenseDeclaration, LookId, LutBase,
    LutDynamicRange, LutEncoding, LutFile, LutLookProposal, LutLookVersion, LutRenderer,
    PublishedTargetId, Sha256Hex, TargetBasis, TargetEligibility, TargetEligibilityStatus,
    TargetVersion,
};
use looklab_server::{FsBlobStore, LutAdmission, ProposalId, admin, admit_lut, model_store};
use looklab_trust::VersionId;
use looklab_upload::LookSlug;
use looklab_worker::jobs::{self, JobPayload};
use looklab_worker::{Worker, WorkerConfig};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn identity_cube() -> CubeFile {
    let size = 3;
    let mut data = Vec::new();
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                let value = |n: u32| n as f32 / (size - 1) as f32;
                data.push([value(r), value(g), value(b)]);
            }
        }
    }
    CubeFile {
        lut: Lut3d {
            size,
            data,
            shaper: None,
        },
        domain: [0.0, 1.0],
    }
}

struct Fixture {
    _dir: TempDir,
    worker: Worker,
}

async fn fixture() -> Fixture {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let dir = TempDir::new().expect("temp dir");
    let mut config =
        WorkerConfig::new(dir.path().join("looklab.sqlite3"), dir.path().join("blobs"));
    config.data = data;
    config.snapshot_dir = config.data.join("community");
    config.site = dir.path().join("site");
    config.transforms = dir.path().join("transforms");
    std::fs::create_dir_all(&config.transforms).expect("transform dir");
    let worker = Worker::open(config).await.expect("worker");
    Fixture { _dir: dir, worker }
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_normal_budget_import_publishes_one_version() {
    const LOOK: &str = "imported-lut";

    let fixture = fixture().await;
    let cube = identity_cube();
    let text = write_cube(&cube.lut, "Test", cube.domain);
    let bytes = text.as_bytes();
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let digest = hex(&digest);
    let pool = fixture.worker.pool();
    sqlx::query("DELETE FROM jobs")
        .execute(pool)
        .await
        .expect("clear jobs");
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES ('cred', x'01', '{}', 0)",
    )
    .execute(pool)
    .await
    .expect("credential");
    let proposal = ProposalId::new();
    let payload = format!(
        "look: {LOOK}\nname: Imported LUT\nlicense: {{ own_work: true }}\n\
         attribution: A contributor\nlut:\n  file: imported.cube\n  sha256: {digest}\n  \
         size: {}\nencoding: flog2_fgamut\n",
        bytes.len()
    );
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES (?, 'cred', 'lut_look', ?, 'x', '[]', 'approved', 0)",
    )
    .bind(proposal.as_str())
    .bind(payload)
    .execute(pool)
    .await
    .expect("proposal");
    let hash = hex_digest(&digest);
    sqlx::query(
        "INSERT INTO lut_uploads
         (proposal_id, sha256, credential_id, file, size, created_at)
         VALUES (?, ?, 'cred', 'imported.cube', ?, 0)",
    )
    .bind(proposal.as_str())
    .bind(hash.as_slice())
    .bind(i64::try_from(bytes.len()).expect("size"))
    .execute(pool)
    .await
    .expect("upload");
    std::fs::write(fixture.worker.config().blobs.join(&digest), bytes).expect("blob");
    jobs::enqueue(
        pool,
        &JobPayload::LutImport {
            proposal_id: proposal.clone(),
        },
        0,
    )
    .await
    .expect("enqueue");
    fixture.worker.run_once().await.expect("import");
    for _ in 0..8 {
        let imported: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports WHERE proposal_id = ?")
                .bind(proposal.as_str())
                .fetch_one(pool)
                .await
                .expect("imports");
        if imported > 0 {
            break;
        }
        if let Some(reason) = sqlx::query_scalar::<_, String>(
            "SELECT reason FROM lut_import_failures WHERE proposal_id = ?",
        )
        .bind(proposal.as_str())
        .fetch_optional(pool)
        .await
        .expect("import failure")
        {
            panic!("the LUT import failed: {reason}");
        }
        fixture.worker.run_once().await.expect("worker");
    }

    let status: String = sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ?")
        .bind(LOOK)
        .fetch_one(pool)
        .await
        .expect("version");
    assert_eq!(
        looklab_worker::db::LookVersionStatus::parse(&status).expect("status"),
        looklab_worker::db::LookVersionStatus::Published
    );
    let imports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports WHERE proposal_id = ?")
        .bind(proposal.as_str())
        .fetch_one(pool)
        .await
        .expect("imports");
    assert_eq!(imports, 1);
    let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads WHERE proposal_id = ?")
        .bind(proposal.as_str())
        .fetch_one(pool)
        .await
        .expect("uploads");
    assert_eq!(uploads, 0);
    let failures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures WHERE proposal_id = ?")
            .bind(proposal.as_str())
            .fetch_one(pool)
            .await
            .expect("failures");
    assert_eq!(failures, 0);
    let eligible: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_target_eligibility_snapshots
         WHERE target = ? AND basis = 'lut_composition' AND status = 'eligible'",
    )
    .bind(format!("{LOOK}@1"))
    .fetch_one(pool)
    .await
    .expect("eligibility");
    assert_eq!(eligible, 1);
    let current: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_target_eligibility_current WHERE target = ?",
    )
    .bind(format!("{LOOK}@1"))
    .fetch_one(pool)
    .await
    .expect("current eligibility");
    assert_eq!(current, 1);
}

fn hex_digest(text: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex");
    }
    bytes
}

fn look_slug(text: &str) -> LookSlug {
    LookSlug::new(text).expect("slug")
}

fn base_reference(slug: &str, version: &str) -> BaseReference {
    BaseReference::Look {
        slug: looklab_model::file::Slug::new(slug).expect("slug"),
        version: LookVersion::new(version).expect("version"),
    }
}

fn write_blob(worker: &Worker, bytes: &[u8]) -> String {
    let digest = hex(&Sha256::digest(bytes));
    std::fs::write(worker.config().blobs.join(&digest), bytes).expect("blob");
    digest
}

async fn publish_source_preview_base(worker: &Worker, slug: &str, version: u32) {
    let text = write_cube(&identity_cube().lut, "Base", [0.0, 1.0]);
    let cube = looklab_export::read_cube(&text).expect("cube");
    let encoding = DeclaredEncoding::DisplayReferred {
        base: BaseReference::neutral_baseline(),
        status: BaseStatus::BaseIncomplete {
            missing: BaseField::FilmSimulation,
        },
    };
    let record = looklab_export::lut_data_from_cube(&cube)
        .expect("lut")
        .record();
    let preview = bake_lut_preview(
        &cube.lut,
        &encoding,
        OutputEncoding::Srgb,
        &record,
        &BaseReference::neutral_baseline(),
        None,
    )
    .expect("preview");
    let source = text.as_bytes().to_vec();
    let descriptor = SourceLutPreview {
        format: SourceLutFormat::V1,
        look: LookIdentity::new(
            slug,
            &format!("{version}.0.0"),
            LookKind::LutDefined,
            WbPolicy::Adaptive,
        )
        .expect("identity"),
        name: "Preview base".to_owned(),
        license: looklab_model::file::LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "a donor".to_owned(),
        },
        encoding,
        output: OutputEncoding::Srgb,
        emulates: None,
        source: ArtifactEntry {
            name: ArtifactName::new(SOURCE_CUBE_ARTIFACT).expect("name"),
            sha256: ArtifactHash::of(&source),
            bytes: source.len() as u64,
        },
        preview: SourceLutPreviewContext {
            baseline: PreviewBaseline::NeutralFallback,
            lut: ArtifactEntry {
                name: ArtifactName::new(PREVIEW_LUT_ARTIFACT).expect("name"),
                sha256: ArtifactHash::of(&preview),
                bytes: preview.len() as u64,
            },
        },
    };
    let artifacts = [
        (
            SOURCE_LUT_ARTIFACT,
            descriptor.to_json().expect("descriptor json"),
        ),
        (PREVIEW_LUT_ARTIFACT, preview),
        (SOURCE_CUBE_ARTIFACT, source),
    ];
    let mut manifest = Vec::new();
    for (name, bytes) in &artifacts {
        manifest.push(admin::ManifestArtifact {
            name: (*name).to_owned(),
            sha256: write_blob(worker, bytes),
        });
    }
    admin::write_version_manifest(
        worker.pool(),
        &look_slug(slug),
        VersionId::new(version),
        admin::VersionStatus::Published,
        &manifest,
        &[],
    )
    .await
    .expect("base preview manifest");
}

async fn publish_layered_base(
    worker: &Worker,
    slug: &str,
    version: u32,
    eligible: bool,
    maker: &str,
    rendering: &str,
) {
    let identity = LookIdentity::new(
        slug,
        &format!("{version}.0.0"),
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let mut document = LookDocument::from_look(identity, Look::colorimetric()).expect("document");
    let rendering = CameraRendering {
        maker: maker.to_owned(),
        name: rendering.to_owned(),
    };
    document.file.rendering = Some(rendering.clone());
    document.file.provenance = Provenance {
        bodies: vec![BodyProvenance {
            body: "fujifilm/x-t5".to_owned(),
            epoch_map_version: 0,
            epochs: vec![1],
            input_transform_version: 1,
            realization_params_version: 0,
            control_response_version: 1,
            clip_k: document.look.clip.k,
            images: 1,
            credentials: 1,
        }],
        root: RootProvenance {
            admissions: Vec::new(),
            anchor_version: format!("{version}.0.0"),
        },
        base: None,
        needs_rebase: false,
        community_snapshot_version: 7,
        prep_versions: vec![1],
        created: "2026-01-01T00:00:00Z".to_owned(),
        software: "looklab-worker test".to_owned(),
    };
    let bytes = document.encode().expect("encode");
    let sha256 = write_blob(worker, &bytes);
    admin::write_version_manifest(
        worker.pool(),
        &look_slug(slug),
        VersionId::new(version),
        admin::VersionStatus::Published,
        &[admin::ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256: sha256.clone(),
        }],
        &[],
    )
    .await
    .expect("base layered manifest");
    admin::record_native_rendering(worker.pool(), &look_slug(slug), version, &rendering)
        .await
        .expect("native rendering");
    if eligible {
        let mut conn = worker.pool().acquire().await.expect("connection");
        model_store::record_target_eligibility(
            &mut conn,
            &TargetEligibility {
                id: EligibilitySnapshotId::from_digest([0; 32]),
                target: PublishedTargetId {
                    look: LookId::new(slug).expect("look"),
                    version: TargetVersion::new(version).expect("version"),
                },
                basis: TargetBasis::NativeFamily,
                artifacts: vec![ArtifactBinding {
                    sha256: Sha256Hex::parse(&sha256).expect("digest"),
                    role: ArtifactRole::Look,
                }],
                base_pin: None,
                support_uncertainty_source: None,
                status: TargetEligibilityStatus::Eligible,
                reasons: Vec::new(),
                dependencies: Vec::new(),
            },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("base eligibility");
    }
}

async fn run_import(worker: &Worker, proposal: &ProposalId, look: &str, cube: &CubeFile) {
    let text = write_cube(&cube.lut, "Imported", cube.domain);
    let bytes = text.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let pool = worker.pool();
    sqlx::query("DELETE FROM jobs")
        .execute(pool)
        .await
        .expect("clear jobs");
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES ('cred', x'01', '{}', 0)",
    )
    .execute(pool)
    .await
    .expect("credential");
    let payload = LutLookProposal {
        look: LookId::new(look).expect("look"),
        name: "Imported LUT".to_owned(),
        license: LicenseDeclaration {
            own_work: true,
            source_license: None,
            source_url: None,
        },
        attribution: "A contributor".to_owned(),
        lut: LutFile {
            file: "imported.cube".to_owned(),
            sha256: Sha256Hex::parse(&digest).expect("digest"),
            size: bytes.len() as u64,
        },
        encoding: LutEncoding::Display {
            base: LutBase::Look {
                look: LookId::new("base-look").expect("base look"),
                version: LutLookVersion::new("1.0.0").expect("version"),
            },
        },
        emulates: None,
    };
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES (?, 'cred', 'lut_look', ?, 'x', '[]', 'approved', 0)",
    )
    .bind(proposal.as_str())
    .bind(serde_norway::to_string(&payload).expect("proposal yaml"))
    .execute(pool)
    .await
    .expect("proposal");
    sqlx::query(
        "INSERT INTO lut_uploads
         (proposal_id, sha256, credential_id, file, size, created_at)
         VALUES (?, ?, 'cred', 'imported.cube', ?, 0)",
    )
    .bind(proposal.as_str())
    .bind(hex_digest(&digest).as_slice())
    .bind(i64::try_from(bytes.len()).expect("size"))
    .execute(pool)
    .await
    .expect("upload");
    std::fs::write(worker.config().blobs.join(&digest), bytes).expect("blob");
    jobs::enqueue(
        pool,
        &JobPayload::LutImport {
            proposal_id: proposal.clone(),
        },
        0,
    )
    .await
    .expect("enqueue");
    worker.run_once().await.expect("import");
}

#[tokio::test]
async fn an_approximated_import_falls_back_to_the_recorded_preview() {
    let fixture = fixture().await;
    publish_source_preview_base(&fixture.worker, "base-look", 1).await;
    publish_layered_base(
        &fixture.worker,
        "base-look",
        2,
        false,
        "Fujifilm",
        "Classic Chrome",
    )
    .await;
    let proposal = ProposalId::new();
    run_import(
        &fixture.worker,
        &proposal,
        "approximated-lut",
        &identity_cube(),
    )
    .await;

    let pool = fixture.worker.pool();
    let failures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures WHERE proposal_id = ?")
            .bind(proposal.as_str())
            .fetch_one(pool)
            .await
            .expect("failures");
    assert_eq!(failures, 0);
    let artifacts = looklab_worker::db::manifest_artifacts(pool, "approximated-lut", 1)
        .await
        .expect("manifest")
        .expect("version");
    assert!(
        !artifacts
            .iter()
            .any(|artifact| artifact.name == "look.looklab")
    );
    let descriptor = artifacts
        .iter()
        .find(|artifact| artifact.name == SOURCE_LUT_ARTIFACT)
        .map(|artifact| {
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("descriptor bytes")
        })
        .expect("descriptor");
    let descriptor = SourceLutPreview::decode(&descriptor).expect("decode");
    assert_eq!(
        descriptor.encoding.base(),
        Some(&base_reference("base-look", "1.0.0"))
    );
    assert_eq!(
        descriptor.preview.baseline,
        PreviewBaseline::Approximated {
            base: base_reference("base-look", "2.0.0"),
        }
    );
}

#[tokio::test]
async fn a_neutral_source_preview_upgrades_when_its_declared_base_publishes() {
    let fixture = fixture().await;
    publish_source_preview_base(&fixture.worker, "base-look", 1).await;
    let proposal = ProposalId::new();
    run_import(&fixture.worker, &proposal, "upgraded-lut", &identity_cube()).await;

    let pool = fixture.worker.pool();
    let before = looklab_worker::db::manifest_artifacts(pool, "upgraded-lut", 1)
        .await
        .expect("manifest")
        .expect("version 1");
    let descriptor = before
        .iter()
        .find(|artifact| artifact.name == SOURCE_LUT_ARTIFACT)
        .map(|artifact| {
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("descriptor bytes")
        })
        .expect("descriptor");
    let descriptor = SourceLutPreview::decode(&descriptor).expect("decode");
    assert_eq!(
        descriptor.preview.baseline,
        PreviewBaseline::NeutralFallback
    );
    let before_bytes: Vec<(String, Vec<u8>)> = before
        .iter()
        .map(|artifact| {
            (
                artifact.name.clone(),
                std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                    .expect("old artifact bytes"),
            )
        })
        .collect();

    publish_layered_base(
        &fixture.worker,
        "base-look",
        2,
        false,
        "Fujifilm",
        "Classic Chrome",
    )
    .await;
    sqlx::query("DELETE FROM jobs")
        .execute(pool)
        .await
        .expect("clear jobs");
    jobs::enqueue(pool, &JobPayload::LutRebase, 0)
        .await
        .expect("enqueue");
    fixture.worker.run_once().await.expect("rebase");

    let after = looklab_worker::db::manifest_artifacts(pool, "upgraded-lut", 2)
        .await
        .expect("manifest")
        .expect("version 2");
    let descriptor = after
        .iter()
        .find(|artifact| artifact.name == SOURCE_LUT_ARTIFACT)
        .map(|artifact| {
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("descriptor bytes")
        })
        .expect("descriptor");
    let descriptor = SourceLutPreview::decode(&descriptor).expect("decode");
    assert_eq!(
        descriptor.preview.baseline,
        PreviewBaseline::Approximated {
            base: base_reference("base-look", "2.0.0"),
        }
    );

    let unchanged = looklab_worker::db::manifest_artifacts(pool, "upgraded-lut", 1)
        .await
        .expect("manifest")
        .expect("version 1");
    for (name, bytes) in before_bytes {
        let artifact = unchanged
            .iter()
            .find(|artifact| artifact.name == name)
            .expect("old artifact listed");
        assert_eq!(artifact.sha256, hex(&Sha256::digest(&bytes)));
        assert_eq!(
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("old artifact bytes"),
            bytes
        );
    }
}

#[tokio::test]
async fn a_preview_on_the_latest_base_is_not_re_marked() {
    let fixture = fixture().await;
    publish_layered_base(
        &fixture.worker,
        "base-look",
        2,
        false,
        "Fujifilm",
        "Classic Chrome",
    )
    .await;
    let pool = fixture.worker.pool();
    let look = look_slug("preview-lut");
    let declared = base_reference("base-look", "1.0.0");
    let composition = base_reference("base-look", "2.0.0");
    admin::write_version_manifest(
        pool,
        &look,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &[admin::ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256: hex(&Sha256::digest(b"placeholder")),
        }],
        &[],
    )
    .await
    .expect("preview manifest");
    admin::record_lut_base(
        pool,
        &look,
        1,
        &declared,
        &BaseStatus::BaseIdentifiedUnmeasured,
    )
    .await
    .expect("base pin");
    let mut conn = pool.acquire().await.expect("connection");
    admin::record_lut_preview_base_conn(&mut conn, &look, 1, &composition)
        .await
        .expect("composition pin");
    drop(conn);

    let marked = admin::mark_lut_dependents(pool, 1).await.expect("mark");
    assert!(marked.is_empty());
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_rebase_pending")
        .fetch_one(pool)
        .await
        .expect("pending");
    assert_eq!(pending, 0);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn an_approximated_fit_publishes_one_marked_version() {
    let fixture = fixture().await;
    publish_source_preview_base(&fixture.worker, "base-look", 1).await;
    publish_layered_base(
        &fixture.worker,
        "base-look",
        2,
        true,
        "Fujifilm",
        "Classic Chrome",
    )
    .await;
    let proposal = ProposalId::new();
    run_import(&fixture.worker, &proposal, "fitted-lut", &identity_cube()).await;

    let pool = fixture.worker.pool();
    let artifacts = looklab_worker::db::manifest_artifacts(pool, "fitted-lut", 1)
        .await
        .expect("manifest")
        .expect("version");
    let look = artifacts
        .iter()
        .find(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        .expect("fitted artifact");
    let bytes = std::fs::read(fixture.worker.config().blobs.join(&look.sha256)).expect("blob");
    let document = LookDocument::decode(&bytes).expect("document");
    let definition = document.file.definition.expect("definition");
    assert_eq!(
        definition.declared_base,
        Some(base_reference("base-look", "1.0.0"))
    );
    assert_eq!(
        definition.encoding.base(),
        Some(&base_reference("base-look", "2.0.0"))
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM look_versions WHERE look = 'fitted-lut' AND version = 1",
    )
    .fetch_one(pool)
    .await
    .expect("status");
    assert_eq!(
        looklab_worker::db::LookVersionStatus::parse(&status).expect("status"),
        looklab_worker::db::LookVersionStatus::Published
    );

    let marked = admin::mark_lut_dependents(pool, 1).await.expect("mark");
    assert!(marked.is_empty());
    let versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = 'fitted-lut'")
            .fetch_one(pool)
            .await
            .expect("versions");
    assert_eq!(versions, 1);
}

#[tokio::test]
async fn a_base_error_does_not_fail_the_preview_bake() {
    let cube = identity_cube();
    let record = looklab_export::lut_data_from_cube(&cube)
        .expect("lut")
        .record();
    let encoding = DeclaredEncoding::DisplayReferred {
        base: base_reference("missing-base", "1.0.0"),
        status: BaseStatus::BaseIdentifiedUnmeasured,
    };
    let preview = bake_lut_preview(
        &cube.lut,
        &encoding,
        OutputEncoding::Srgb,
        &record,
        &base_reference("missing-base", "1.0.0"),
        None,
    )
    .expect("preview");
    assert_eq!(preview.len(), LUT_BYTES);
}

#[tokio::test]
async fn an_operator_admission_publishes_a_neutral_preview() {
    let fixture = fixture().await;
    let cube = identity_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let proposal = LutLookProposal {
        look: LookId::new("operator-preview").expect("look"),
        name: "Operator Preview".to_owned(),
        license: LicenseDeclaration {
            own_work: false,
            source_license: Some("CC0-1.0".to_owned()),
            source_url: None,
        },
        attribution: "A donor".to_owned(),
        lut: LutFile {
            file: "donated.cube".to_owned(),
            sha256: Sha256Hex::parse(&digest).expect("digest"),
            size: bytes.len() as u64,
        },
        encoding: LutEncoding::Display {
            base: LutBase::BodyRealization {
                body: looklab_schema::BodyId::new("fujifilm/x-e3").expect("body"),
                renderer: LutRenderer::Unspecified,
                film_simulation: None,
                dynamic_range: None,
                notes: "Fujifilm X-E3 RAW through an unnamed RAW converter.".to_owned(),
            },
        },
        emulates: None,
    };
    let store = FsBlobStore::new(fixture.worker.config().blobs.clone()).expect("store");
    let admitted = admit_lut(
        fixture.worker.pool(),
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "operator test admission".to_owned(),
        },
    )
    .await
    .expect("admit");
    let pool = fixture.worker.pool();
    let audit: String =
        sqlx::query_scalar("SELECT payload FROM audit_log WHERE type = 'moderation_decision'")
            .fetch_one(pool)
            .await
            .expect("audit");
    let audit: serde_json::Value = serde_json::from_str(&audit).expect("audit payload");
    assert_eq!(audit["actor"], "cli");
    assert_eq!(audit["proposal"], admitted.proposal.as_str());
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'lut_import'")
        .fetch_one(pool)
        .await
        .expect("job");
    assert_eq!(queued, 1);
    for _ in 0..8 {
        fixture.worker.run_once().await.expect("worker");
        let imported: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports WHERE proposal_id = ?")
                .bind(admitted.proposal.as_str())
                .fetch_one(pool)
                .await
                .expect("imports");
        if imported > 0 {
            break;
        }
        if let Some(reason) = sqlx::query_scalar::<_, String>(
            "SELECT reason FROM lut_import_failures WHERE proposal_id = ?",
        )
        .bind(admitted.proposal.as_str())
        .fetch_optional(pool)
        .await
        .expect("import failure")
        {
            panic!("the LUT import failed: {reason}");
        }
    }
    let failures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures WHERE proposal_id = ?")
            .bind(admitted.proposal.as_str())
            .fetch_one(pool)
            .await
            .expect("failures");
    assert_eq!(failures, 0);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM look_versions WHERE look = 'operator-preview' AND version = 1",
    )
    .fetch_one(pool)
    .await
    .expect("status");
    assert_eq!(
        looklab_worker::db::LookVersionStatus::parse(&status).expect("status"),
        looklab_worker::db::LookVersionStatus::Published
    );
    let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads WHERE proposal_id = ?")
        .bind(admitted.proposal.as_str())
        .fetch_one(pool)
        .await
        .expect("uploads");
    assert_eq!(uploads, 0);
    let artifacts = looklab_worker::db::manifest_artifacts(pool, "operator-preview", 1)
        .await
        .expect("manifest")
        .expect("version");
    assert!(
        !artifacts
            .iter()
            .any(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
    );
    let descriptor = artifacts
        .iter()
        .find(|artifact| artifact.name == SOURCE_LUT_ARTIFACT)
        .map(|artifact| {
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("descriptor bytes")
        })
        .expect("descriptor");
    let descriptor = SourceLutPreview::decode(&descriptor).expect("decode");
    assert_eq!(
        descriptor.preview.baseline,
        PreviewBaseline::NeutralFallback
    );
    assert!(matches!(
        descriptor.encoding,
        DeclaredEncoding::DisplayReferred {
            status: BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation
            },
            ..
        }
    ));
}

#[tokio::test]
async fn an_operator_family_base_publishes_a_neutral_preview() {
    let fixture = fixture().await;
    publish_layered_base(
        &fixture.worker,
        "eterna-base",
        1,
        false,
        "Fujifilm",
        "Eterna",
    )
    .await;
    let cube = identity_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let proposal = LutLookProposal {
        look: LookId::new("operator-family-preview").expect("look"),
        name: "Operator Family Preview".to_owned(),
        license: LicenseDeclaration {
            own_work: false,
            source_license: Some("CC0-1.0".to_owned()),
            source_url: None,
        },
        attribution: "A donor".to_owned(),
        lut: LutFile {
            file: "donated.cube".to_owned(),
            sha256: Sha256Hex::parse(&digest).expect("digest"),
            size: bytes.len() as u64,
        },
        encoding: LutEncoding::Display {
            base: LutBase::FamilyRendering {
                maker: "fujifilm".to_owned().try_into().expect("maker"),
                rendering: "Eterna".to_owned().try_into().expect("rendering"),
                dynamic_range: Some(LutDynamicRange::Dr200),
                notes: String::new(),
            },
        },
        emulates: None,
    };
    let store = FsBlobStore::new(fixture.worker.config().blobs.clone()).expect("store");
    let admitted = admit_lut(
        fixture.worker.pool(),
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "operator test admission".to_owned(),
        },
    )
    .await
    .expect("admit");
    let pool = fixture.worker.pool();
    for _ in 0..8 {
        fixture.worker.run_once().await.expect("worker");
        let imported: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports WHERE proposal_id = ?")
                .bind(admitted.proposal.as_str())
                .fetch_one(pool)
                .await
                .expect("imports");
        if imported > 0 {
            break;
        }
        if let Some(reason) = sqlx::query_scalar::<_, String>(
            "SELECT reason FROM lut_import_failures WHERE proposal_id = ?",
        )
        .bind(admitted.proposal.as_str())
        .fetch_optional(pool)
        .await
        .expect("import failure")
        {
            panic!("the LUT import failed: {reason}");
        }
    }
    let artifacts = looklab_worker::db::manifest_artifacts(pool, "operator-family-preview", 1)
        .await
        .expect("manifest")
        .expect("version");
    let descriptor = artifacts
        .iter()
        .find(|artifact| artifact.name == SOURCE_LUT_ARTIFACT)
        .map(|artifact| {
            std::fs::read(fixture.worker.config().blobs.join(&artifact.sha256))
                .expect("descriptor bytes")
        })
        .expect("descriptor");
    let descriptor = SourceLutPreview::decode(&descriptor).expect("decode");
    assert_eq!(
        descriptor.preview.baseline,
        PreviewBaseline::NeutralFallback
    );
    assert!(matches!(
        descriptor.encoding,
        DeclaredEncoding::DisplayReferred {
            status: BaseStatus::BaseIdentifiedUnmeasured,
            ..
        }
    ));
}
