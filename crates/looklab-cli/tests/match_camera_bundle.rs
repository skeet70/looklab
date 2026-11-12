// SPDX-License-Identifier: AGPL-3.0-or-later
//! Camera bundle replay through `match` and `verify` (docs/04 §2-§5).

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use looklab_match::synth::{self, SyntheticBody};
use looklab_match::{AssessedDomain, Permalink, SelectionMode};
use looklab_model::LookDocument;
use looklab_model::file::{
    Density, LookKind, LookVersion, ProbeCovariance, ProbeScalars, Slug, UncertaintyData, WbPolicy,
    identity,
};
use looklab_model::realization::{ControlValue, Settings, encode_crm};
use looklab_publish::{
    ArtifactEntry, ArtifactHash, ArtifactName, BundleEntry, FamilyHeadPage, LookStatus,
    LookSummary, MANIFEST_NAME, PublishedReportSummary, ReportFormat, ReportSummaryEntry,
    SelectedTargetSummary, SiteIndex, SitePath, SiteReport, VersionSummary, bundle_artifacts_path,
    bundle_dir, bundle_manifest_path, report_path,
};
use looklab_schema::{
    Applicability, AssessedPoint, AssessmentOutcome, BodyId, BodyRealizationId,
    BodyRealizationVersionId, BodyStateVersion, BundleSimEntry, CandidateSafety, CombinationBasis,
    ControlEpochId, DomainId, FamilyTargetId, FirmwareId, FirmwareOffset, LookId,
    MatchBundleRevision, MatchParams, MatchParamsFormat, MatchRealizationParams, MatchStage,
    MatchTransform, ModelReportId, ModelSubject, ModelVersionId, OutputEncoding,
    QualificationCapability, RealizationRevisionId, RealizationVersion, ReportMetrics, ReportScope,
    RuleRevision, SafetyCheck, SettingCombination, SettingValue, Sha256Hex, SimKey, SourceRef,
    SupportedMask, TargetVersion, WbModeKey, WbModeOffset,
};
use sha2::{Digest, Sha256};

const BODY: &str = "fujifilm/x-t5";
const EPOCH: u32 = 1;
const ASSESSED_PARAMS_VERSION: u32 = 2;
const OTHER_PARAMS_VERSION: u32 = 3;

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn ok(mut command: Command) -> String {
    ok_output(command.output().expect("run"))
}

fn ok_output(output: Output) -> String {
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

fn rejected(output: Output) -> String {
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        !output.status.success(),
        "command succeeded\nstdout: {stdout}\nstderr: {stderr}"
    );
    stderr
}

fn digest(bytes: &[u8]) -> Sha256Hex {
    Sha256Hex::parse(ArtifactHash::of(bytes).as_str()).expect("digest")
}

fn report_id(label: &str) -> ModelReportId {
    ModelReportId::from_digest(Sha256::digest(label.as_bytes()).into())
}

struct SimRecord {
    sim: SimKey,
    base_slug: String,
    truth: SyntheticBody,
    degraded: SyntheticBody,
    params_version: u32,
}

struct CameraFixture {
    temp: tempfile::TempDir,
    site: PathBuf,
    bundle: PathBuf,
    bundle_id: looklab_schema::MatchBundleRevisionId,
    target: PathBuf,
    target_report: ModelReportId,
    controls_report: PathBuf,
    assessed: SimKey,
    assessed_settings: Settings,
    assessed_base_slug: String,
    sims: Vec<SimRecord>,
}

impl CameraFixture {
    fn observed_at(&self, name: &str, settings: &Settings, sim: &SimKey) -> PathBuf {
        let dir = self.temp.path().join(name);
        std::fs::create_dir_all(&dir).expect("observed dir");
        let record = self
            .sims
            .iter()
            .find(|record| &record.sim == sim)
            .expect("sim record");
        let model = if record.sim == self.assessed {
            &record.degraded.model
        } else {
            &record.truth.model
        };
        let samples = synth::observed(model, settings, &synth::probes(), 200).expect("observed");
        std::fs::write(
            dir.join("observed.llsamp"),
            looklab_samples::encode(&samples).expect("encode samples"),
        )
        .expect("write samples");
        dir
    }
}

fn setting_value(value: &ControlValue) -> SettingValue {
    match value {
        ControlValue::Step(step) => SettingValue::Step(*step),
        ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
        ControlValue::Bool(value) => SettingValue::Bool(*value),
    }
}

/// A non-default synthetic control with a fitted response, and its settings point.
fn assessed_point(record: &SimRecord) -> (SettingCombination, Settings) {
    let plan = synth::plan();
    for (key, value) in record.truth.settings.iter() {
        let Some(index) = plan.by_key.get(key) else {
            continue;
        };
        if !plan.plan_controls.contains(index) || plan.domains[*index].default_value() == value {
            continue;
        }
        let layer_response = record
            .truth
            .model
            .crm
            .controls
            .iter()
            .find(|control| &control.key == key)
            .and_then(|control| control.values.iter().find(|entry| &entry.value == value))
            .is_some_and(|entry| {
                entry.delta.layers.iter().any(|layer| {
                    layer
                        .entries
                        .iter()
                        .any(|(_, coefficient)| *coefficient != 0.0)
                })
            });
        let wb_response = record
            .truth
            .model
            .crm
            .wb
            .controls
            .iter()
            .find(|control| &control.key == key)
            .and_then(|control| control.values.iter().find(|entry| &entry.value == value))
            .is_some_and(|entry| entry.log_gain != [0.0, 0.0]);
        if !layer_response && !wb_response {
            continue;
        }
        let mut combination = SettingCombination::new();
        combination.insert(key.clone(), setting_value(value));
        let mut settings = plan.default.clone();
        settings.set(key.clone(), value.clone());
        return (combination, settings);
    }
    panic!("no synthetic control has a non-default fitted response");
}

fn scope(body: &BodyId, combination: &SettingCombination) -> ReportScope {
    ReportScope {
        points: vec![AssessedPoint {
            settings: combination.clone(),
            firmware: None,
            wb_mode: None,
        }],
        combination_basis: CombinationBasis::Enumerated,
        defaults_assessed: true,
        output_encoding: OutputEncoding::Srgb,
        colour_cells: Vec::new(),
        exposure_cells: Vec::new(),
        assessed_bodies: vec![body.clone()],
        assessed_domain: DomainId::new("supported-probes").expect("domain"),
    }
}

fn report(
    id: ModelReportId,
    subject: ModelSubject,
    candidate: ModelVersionId,
    capability: QualificationCapability,
    scope: ReportScope,
    mask: SupportedMask,
) -> SiteReport {
    SiteReport {
        summary: PublishedReportSummary {
            format: ReportFormat::V1,
            report: id,
            subject,
            candidate,
            capability,
            scope,
            rule_revision: RuleRevision::new("looklab-tests/1").expect("rule revision"),
            outcome: AssessmentOutcome::Pass,
            metrics: ReportMetrics::new(),
            supported_mask: mask,
            limitations: Vec::new(),
            evidence_requests: Vec::new(),
            artifacts: Vec::new(),
        },
        applicability: Applicability::Current,
    }
}

type PermalinkMutation<'a> = (&'a str, Box<dyn Fn(&mut Permalink)>, &'a str);

fn target_document(record: &SimRecord) -> Vec<u8> {
    let probes = synth::probes();
    let count = probes.probes.len();
    let mut covariance = vec![0.0_f64; count * 9];
    for chunk in covariance.as_chunks_mut::<9>().0 {
        chunk[0] = 0.01;
        chunk[4] = 0.01;
        chunk[8] = 0.01;
    }
    let uncertainty = UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(count, covariance).expect("probe covariance")),
        support: Some(ProbeScalars::new(vec![1.0; count]).expect("support")),
        density: Some(Density::new(vec![1.0; count]).expect("density")),
        ..UncertaintyData::default()
    };
    LookDocument::from_look(
        identity("target", "1.0.0").expect("identity"),
        record.truth.model.base_look.clone(),
    )
    .expect("document")
    .with_uncertainty(uncertainty)
    .expect("uncertainty")
    .encode()
    .expect("encode")
}

fn camera_fixture() -> CameraFixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let site = temp.path().join("site");
    std::fs::create_dir_all(site.join("data")).expect("site data");
    let body = BodyId::new(BODY).expect("body");
    let epoch = ControlEpochId::new(EPOCH);

    let classic = synth::body(9).expect("synthetic body");
    let provia_sim = SimKey::new("provia").expect("sim");
    let provia = synth::body_for_sim(10, &provia_sim).expect("synthetic body");
    let classic_sim = classic.model.crm.base_sim.clone();
    let records = vec![
        SimRecord {
            sim: classic_sim.clone(),
            base_slug: "base-classic-chrome".to_owned(),
            degraded: classic.degraded(2.0),
            truth: classic,
            params_version: ASSESSED_PARAMS_VERSION,
        },
        SimRecord {
            sim: provia_sim.clone(),
            base_slug: "base-provia".to_owned(),
            degraded: provia.degraded(2.0),
            truth: provia,
            params_version: OTHER_PARAMS_VERSION,
        },
    ];
    let assessed_record = &records[0];
    let (combination, assessed_settings) = assessed_point(assessed_record);
    let assessed = assessed_record.sim.clone();
    let assessed_base_slug = assessed_record.base_slug.clone();

    let target = temp.path().join("target.looklab");
    std::fs::write(&target, target_document(&records[1])).expect("target look");

    let transform = looklab_cli::TransformFile {
        format: "looklab-transform/1".to_owned(),
        version: 1,
        body: None,
        cct: None,
        camera_to_xyz: Some(synth::transform()),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image: Vec::new(),
    };
    let transform_bytes = transform.to_yaml().expect("transform yaml").into_bytes();
    let body_bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"),
    )
    .expect("body schema");
    let params = MatchParams {
        format: MatchParamsFormat::V1,
        stage: MatchStage::Native,
        body: body.clone(),
        transform: MatchTransform {
            version: 1,
            camera_to_xyz: synth::transform().0,
            delta: [0.0; 6],
        },
        exposure: Vec::new(),
        wb: vec![WbModeOffset {
            mode: WbModeKey::new("daylight").expect("wb mode"),
            r: 0.0,
            b: 0.0,
        }],
    };
    let params_bytes = serde_json::to_vec(&params).expect("params json");

    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("body.yaml".to_owned(), body_bytes),
        ("transform.yaml".to_owned(), transform_bytes),
        ("body_params.json".to_owned(), params_bytes.clone()),
    ];
    let mut sims = Vec::with_capacity(records.len());
    for record in &records {
        let family = FamilyTargetId {
            family: LookId::new(record.base_slug.as_str()).expect("family"),
            version: TargetVersion::new(1).expect("target version"),
        };
        let realization = BodyRealizationVersionId {
            realization: BodyRealizationId {
                body: body.clone(),
                epoch,
                native_sim: record.sim.clone(),
            },
            version: RealizationVersion::new(record.params_version).expect("realization version"),
            target: family.clone(),
        };
        let realization_params = MatchRealizationParams {
            format: MatchParamsFormat::V1,
            realization: realization.clone(),
            family: family.clone(),
            version: record.params_version,
            control_response_version: record.truth.model.crm.version,
            clip_k: record.truth.model.base_look.clip.k,
            firmware: vec![FirmwareOffset {
                firmware: FirmwareId::parse("fujifilm/x-t5@1:1.00").expect("firmware"),
                t_shift: 0.0,
                log_g_shift: 0.0,
            }],
        };
        let realization_bytes =
            serde_json::to_vec(&realization_params).expect("realization params json");
        let base_bytes = LookDocument::from_look(
            identity(&record.base_slug, "1.0.0").expect("base identity"),
            record.truth.model.base_look.clone(),
        )
        .expect("base document")
        .encode()
        .expect("base bytes");
        let crm_bytes = encode_crm(&record.truth.model.crm).expect("crm bytes");
        let base_name = ArtifactHash::of(&base_bytes).as_str().to_owned();
        let crm_name = ArtifactHash::of(&crm_bytes).as_str().to_owned();
        let realization_name = ArtifactHash::of(&realization_bytes).as_str().to_owned();
        files.push((base_name, base_bytes.clone()));
        files.push((crm_name, crm_bytes.clone()));
        files.push((realization_name, realization_bytes.clone()));
        sims.push(BundleSimEntry {
            native_sim: record.sim.clone(),
            realization: RealizationRevisionId::for_version(&realization),
            family_version: family,
            base_artifact: digest(&base_bytes),
            control_response: digest(&crm_bytes),
            covariance: None,
            control_response_version: record.truth.model.crm.version,
            clip_k: record.truth.model.base_look.clip.k,
            realization_params_version: record.params_version,
            body_params: Some(digest(&params_bytes)),
            realization_params: Some(digest(&realization_bytes)),
        });
    }

    let manifest = MatchBundleRevision {
        id: looklab_schema::MatchBundleRevisionId::from_digest([0; 32]),
        manifest_hash: looklab_schema::MatchBundleRevisionId::from_digest([0; 32]),
        body: body.clone(),
        epoch,
        schema_snapshot_version: 1,
        input_transform_version: 1,
        body_state_version: BodyStateVersion::new(1).expect("body state"),
        body_realization_params_version: 1,
        sims,
        excluded: Vec::new(),
        producer: SourceRef::new("looklab-cli-tests/1").expect("producer"),
        created_at: 0,
    }
    .sealed();
    let manifest_text = manifest.manifest();
    let bundle_id = manifest.id;
    let bundle = site.join(
        bundle_dir(&body, EPOCH, &bundle_id)
            .expect("bundle dir")
            .as_str(),
    );
    std::fs::create_dir_all(&bundle).expect("bundle directory");
    let mut inventory = Vec::with_capacity(files.len());
    for (name, bytes) in &files {
        std::fs::write(bundle.join(name), bytes).expect("bundle artifact");
        inventory.push(ArtifactEntry {
            name: ArtifactName::new(name.clone()).expect("artifact name"),
            sha256: ArtifactHash::of(bytes),
            bytes: bytes.len() as u64,
        });
    }
    let inventory_bytes = serde_json::to_vec(&inventory).expect("inventory json");
    std::fs::write(bundle.join("artifacts.json"), &inventory_bytes).expect("inventory");
    std::fs::write(bundle.join(MANIFEST_NAME), &manifest_text).expect("manifest");

    let target_bytes = std::fs::read(&target).expect("target bytes");
    let target_family = FamilyTargetId {
        family: LookId::new("target").expect("target family"),
        version: TargetVersion::new(1).expect("target version"),
    };
    let target_report_id = report_id("target-native-prediction");
    let controls_report_id = report_id("classic-controls-matching");

    let mut mask = SupportedMask::new();
    let probes = synth::probes();
    let first_cell = AssessedDomain::cell_of(&probes.probes[0]).expect("probe cell");
    let retained = probes
        .probes
        .iter()
        .filter(|probe| AssessedDomain::cell_of(probe).as_ref() == Some(&first_cell))
        .count();
    assert!(
        retained > 0 && retained < probes.probes.len(),
        "the assessed cell must cover a strict subset of the probes"
    );
    mask.insert(first_cell);

    let target_report = report(
        target_report_id,
        ModelSubject::SharedFamily {
            family: LookId::new("target").expect("family"),
        },
        ModelVersionId::SharedFamily {
            target: target_family.clone(),
        },
        QualificationCapability::NativePrediction,
        scope(&body, &SettingCombination::new()),
        SupportedMask::new(),
    );
    let controls_report = report(
        controls_report_id,
        ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                body: body.clone(),
                epoch,
                native_sim: assessed.clone(),
            },
        },
        ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization: BodyRealizationId {
                    body: body.clone(),
                    epoch,
                    native_sim: assessed.clone(),
                },
                version: RealizationVersion::new(ASSESSED_PARAMS_VERSION)
                    .expect("realization version"),
                target: FamilyTargetId {
                    family: LookId::new(assessed_base_slug.as_str()).expect("family"),
                    version: TargetVersion::new(1).expect("target version"),
                },
            },
        },
        QualificationCapability::ControlsMatching,
        scope(&body, &combination),
        mask,
    );

    let mut report_entries = Vec::new();
    let mut controls_report_path = PathBuf::new();
    for (id, report) in [
        (target_report_id, target_report),
        (controls_report_id, controls_report),
    ] {
        let path = report_path(&id).expect("report path");
        let bytes = serde_json::to_vec(&report).expect("report json");
        let file = site.join(path.as_str());
        std::fs::create_dir_all(file.parent().expect("report parent")).expect("report dir");
        std::fs::write(&file, &bytes).expect("report");
        if id == controls_report_id {
            controls_report_path = file;
        }
        report_entries.push(ReportSummaryEntry {
            report: id,
            subject: report.summary.subject,
            candidate: report.summary.candidate,
            capability: report.summary.capability,
            outcome: report.summary.outcome,
            applicability: report.applicability,
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
    }

    let index = SiteIndex {
        format: "looklab-site/1".to_owned(),
        community_snapshot_version: 0,
        bodies: Vec::new(),
        manuals: Vec::new(),
        interfaces: Vec::new(),
        input_transforms: Vec::new(),
        looks: vec![LookSummary {
            slug: Slug::new("target").expect("slug"),
            name: "Target".to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            visibility: looklab_publish::LookVisibility::TestFixture,
            head: Some(FamilyHeadPage {
                target: target_family,
                version: LookVersion::new("1.0.0").expect("version"),
                source_limited: false,
                safety: CandidateSafety {
                    numerical: SafetyCheck::Passed,
                    release_gates: SafetyCheck::Passed,
                },
                report: Some(target_report_id),
                applicability: Applicability::Current,
            }),
            selected: Some(SelectedTargetSummary {
                version: LookVersion::new("1.0.0").expect("version"),
                eligibility: None,
                applicability: Applicability::Current,
            }),
            versions: vec![VersionSummary {
                support: looklab_publish::PublishedSupport::Unstated,
                version: LookVersion::new("1.0.0").expect("version"),
                status: LookStatus::Published,
                matchable: true,
                card_eligible: false,
                approximated: false,
                applicability: Applicability::Current,
                eligibility_status: None,
                card: None,
                manifest: SitePath::new("data/looks/target/1.0.0/manifest.json")
                    .expect("manifest path"),
                manifest_sha256: ArtifactHash::of(b""),
                manifest_bytes: 0,
                artifact: ArtifactName::new("target.looklab").expect("artifact name"),
                sha256: ArtifactHash::of(&target_bytes),
                bytes: target_bytes.len() as u64,
            }],
        }],
        scenes: Vec::new(),
        reports: report_entries,
        eligibility: Vec::new(),
        current: Vec::new(),
        bundles: vec![BundleEntry {
            bundle: bundle_id,
            body: body.clone(),
            epoch: EPOCH,
            path: bundle_manifest_path(&body, EPOCH, &bundle_id).expect("manifest path"),
            sha256: ArtifactHash::of(manifest_text.as_bytes()),
            bytes: manifest_text.len() as u64,
            artifacts_path: bundle_artifacts_path(&body, EPOCH, &bundle_id)
                .expect("inventory path"),
            artifacts_sha256: ArtifactHash::of(&inventory_bytes),
            artifacts_bytes: inventory_bytes.len() as u64,
        }],
        reference_preview: None,
    };
    std::fs::write(
        site.join("data/index.json"),
        serde_json::to_vec(&index).expect("index json"),
    )
    .expect("index");

    CameraFixture {
        temp,
        site,
        bundle,
        bundle_id,
        target,
        target_report: target_report_id,
        controls_report: controls_report_path,
        assessed,
        assessed_settings,
        assessed_base_slug,
        sims: records,
    }
}

fn qualified_match(fixture: &CameraFixture) -> Command {
    let mut command = looklab();
    command
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .args(["--selection", "qualified"])
        .arg("--bundle-revision")
        .arg(fixture.bundle_id.to_hex())
        .arg("--report")
        .arg(format!(
            "target@1=report:{}",
            fixture.target_report.to_hex()
        ))
        .arg("--controls-report")
        .arg(&fixture.controls_report)
        .arg("--publication")
        .arg(&fixture.site)
        .arg("--permalink")
        .arg("match.permalink")
        .args(["--seed", "7"]);
    command
}

fn verify(permalink: &Path, observed: &Path, publication: &Path) -> Output {
    looklab()
        .arg("verify")
        .arg("--permalink")
        .arg(permalink)
        .arg("--observed")
        .arg(observed)
        .arg("--publication")
        .arg(publication)
        .output()
        .expect("run")
}

#[test]
fn a_qualified_camera_match_records_each_sims_own_revision() {
    let fixture = camera_fixture();
    let stdout = ok(qualified_match(&fixture));
    assert!(stdout.contains("selection: qualified"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            " of {} declared points",
            synth::probes().probes.len()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("reachability: reachable"), "{stdout}");

    let path = fixture.temp.path().join("match.permalink");
    let text = std::fs::read_to_string(&path).expect("permalink");
    let permalink = Permalink::parse(&text).expect("permalink parses");
    assert_eq!(permalink.sim, fixture.assessed.as_str());
    assert_eq!(
        permalink.realization_params_version,
        ASSESSED_PARAMS_VERSION
    );
    assert_eq!(permalink.target_slug, "target");
    assert_eq!(permalink.base_slug, fixture.assessed_base_slug);
    assert!(permalink.metric.is_rendered());
    assert!(permalink.output.is_some());
    let selection = permalink.selection.expect("recorded selection");
    assert_eq!(selection.mode, SelectionMode::Qualified);
    assert_eq!(selection.bundle.to_hex(), fixture.bundle_id.to_hex());
    assert_eq!(selection.reports.len(), 2);
}

#[test]
fn unselected_camera_matching_records_the_winners_revision() {
    let fixture = camera_fixture();
    let path = fixture.temp.path().join("unselected.permalink");
    let mut command = looklab();
    command
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--permalink")
        .arg(&path)
        .args(["--seed", "7"]);
    ok(command);
    let permalink =
        Permalink::parse(&std::fs::read_to_string(&path).expect("permalink")).expect("parse");
    let winner = fixture
        .sims
        .iter()
        .find(|record| record.sim.as_str() == permalink.sim)
        .expect("winner is a listed sim");
    assert_eq!(permalink.realization_params_version, winner.params_version);
    assert!(permalink.selection.is_none());
}

#[test]
fn unselected_camera_matching_rejects_physical_context_flags() {
    let fixture = camera_fixture();
    for flag in ["--firmware", "--wb-mode"] {
        let value = if flag == "--firmware" {
            "fujifilm/x-t5@1:1.00"
        } else {
            "daylight"
        };
        let mut command = looklab();
        command
            .current_dir(fixture.temp.path())
            .arg("match")
            .arg("--look")
            .arg(&fixture.target)
            .arg("--bundle")
            .arg(&fixture.bundle)
            .arg(flag)
            .arg(value)
            .args(["--seed", "7"]);
        let stderr = rejected(command.output().expect("run"));
        assert!(stderr.contains("--selection"), "{stderr}");
    }
}

#[test]
fn verify_downgrades_a_calibrated_qualified_selection_and_replays_it() {
    let fixture = camera_fixture();
    let first_stdout = ok_output(qualified_match(&fixture).output().expect("run"));
    assert!(
        !first_stdout.contains("destination realization:"),
        "{first_stdout}"
    );
    let permalink_path = fixture.temp.path().join("match.permalink");
    let original = Permalink::parse(&std::fs::read_to_string(&permalink_path).expect("permalink"))
        .expect("parse");
    let original_selection = original.selection.clone().expect("selection");
    let observed = fixture.observed_at(
        "observed-first",
        &fixture.assessed_settings,
        &fixture.assessed,
    );

    let stdout = ok_output(verify(&permalink_path, &observed, &fixture.site));
    assert!(
        stdout.contains("the local calibration makes the next selection experimental"),
        "{stdout}"
    );
    assert!(
        stdout.contains("destination realization: the camera realization is experimental"),
        "{stdout}"
    );

    let updated_path = fixture.temp.path().join("match.updated.permalink");
    let updated_text = std::fs::read_to_string(&updated_path).expect("updated permalink");
    assert!(
        updated_text.starts_with("looklab-match/4"),
        "{updated_text}"
    );
    let updated = Permalink::parse(&updated_text).expect("updated permalink parses");
    assert!(updated.metric.is_rendered());
    assert!(updated.output.is_some());
    let selection = updated.selection.clone().expect("recorded selection");
    assert_eq!(selection.mode, SelectionMode::Experimental);
    assert_eq!(selection.bundle, original_selection.bundle);
    assert_eq!(selection.reports, original_selection.reports);
    assert!(!updated.calibration.is_empty());

    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.temp.path().join("match.updated.result.json"))
            .expect("updated result"),
    )
    .expect("result json");
    let settings: Settings =
        serde_json::from_value(result["settings"].clone()).expect("second settings");
    let second_sim = SimKey::new(updated.sim.as_str()).expect("second sim");
    let replay_observed = fixture.observed_at("observed-replay", &settings, &second_sim);
    let stdout = ok_output(verify(&updated_path, &replay_observed, &fixture.site));
    assert!(stdout.contains("verify:"), "{stdout}");

    let replayed_path = fixture.temp.path().join("match.updated.updated.permalink");
    let replayed =
        Permalink::parse(&std::fs::read_to_string(&replayed_path).expect("replayed permalink"))
            .expect("replayed permalink parses");
    assert_eq!(
        replayed.selection.as_ref().map(|selection| selection.mode),
        Some(SelectionMode::Experimental)
    );
}

#[test]
fn verify_discovers_the_camera_bundle_beside_a_pinned_artifact() {
    let fixture = camera_fixture();
    ok(qualified_match(&fixture));
    let permalink_path = fixture.temp.path().join("match.permalink");
    let observed = fixture.observed_at("observed", &fixture.assessed_settings, &fixture.assessed);
    let stdout = ok_output(verify(&permalink_path, &observed, &fixture.site));
    assert!(stdout.contains("verify:"), "{stdout}");
}

#[test]
fn verify_rejects_contradictory_camera_pins() {
    let fixture = camera_fixture();
    ok(qualified_match(&fixture));
    let permalink_path = fixture.temp.path().join("match.permalink");
    let text = std::fs::read_to_string(&permalink_path).expect("permalink");
    let original = Permalink::parse(&text).expect("parse");
    let observed = fixture.observed_at("observed", &fixture.assessed_settings, &fixture.assessed);

    let other = "0".repeat(64);
    let elsewhere = fixture.temp.path().join("elsewhere.looklab");
    std::fs::write(
        &elsewhere,
        LookDocument::from_look(
            identity("elsewhere", "1.0.0").expect("identity"),
            fixture.sims[1].truth.model.base_look.clone(),
        )
        .expect("document")
        .encode()
        .expect("encode"),
    )
    .expect("elsewhere base");

    let mutations: Vec<PermalinkMutation<'_>> = vec![
        (
            "transform",
            Box::new(|permalink| permalink.input_transform_version = 2),
            "input transform version",
        ),
        (
            "base",
            Box::new(|permalink| permalink.base_slug = "elsewhere".to_owned()),
            "base Look",
        ),
        (
            "crm",
            Box::new(|permalink| permalink.crm_version += 1),
            "control-response model version",
        ),
        (
            "realization",
            Box::new(|permalink| permalink.realization_params_version += 1),
            "realization version",
        ),
        (
            "source",
            Box::new(|permalink| {
                permalink.sources.base_look = Some("elsewhere.looklab".to_owned())
            }),
            "does not match the bundle content",
        ),
    ];
    for (label, mutate, expected) in mutations {
        let mut permalink = original.clone();
        mutate(&mut permalink);
        let path = fixture.temp.path().join(format!("{label}.permalink"));
        std::fs::write(&path, permalink.canonical()).expect("mutated permalink");
        let stderr = rejected(verify(&path, &observed, &fixture.site));
        assert!(stderr.contains(expected), "{label}: {stderr}");
    }

    let unselected_path = fixture.temp.path().join("unselected.permalink");
    let mut command = looklab();
    command
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--permalink")
        .arg(&unselected_path)
        .args(["--seed", "7"]);
    ok(command);
    let mut unselected =
        Permalink::parse(&std::fs::read_to_string(&unselected_path).expect("permalink"))
            .expect("unselected permalink parses");
    assert!(unselected.selection.is_none());
    unselected.body = "nikon/z8".to_owned();
    let path = fixture.temp.path().join("body.permalink");
    std::fs::write(&path, unselected.canonical()).expect("mutated permalink");
    let stderr = rejected(
        looklab()
            .arg("verify")
            .arg("--permalink")
            .arg(&path)
            .arg("--observed")
            .arg(&observed)
            .output()
            .expect("run"),
    );
    assert!(stderr.contains("camera bundle is for body"), "{stderr}");

    let mut permalink = original.clone();
    permalink.selection.as_mut().expect("selection").bundle =
        Sha256Hex::parse(&other).expect("digest");
    let path = fixture.temp.path().join("bundle.permalink");
    std::fs::write(&path, permalink.canonical()).expect("mutated permalink");
    let stderr = rejected(
        looklab()
            .arg("verify")
            .arg("--permalink")
            .arg(&path)
            .arg("--observed")
            .arg(&observed)
            .arg("--publication")
            .arg(&fixture.site)
            .arg("--bundle")
            .arg(&fixture.bundle)
            .output()
            .expect("run"),
    );
    assert!(
        stderr.contains("does not match the pinned bundle revision"),
        "{stderr}"
    );
}

fn unselected_adobe_match(fixture: &CameraFixture) -> (PathBuf, String) {
    let path = fixture.temp.path().join("adobe.permalink");
    let mut command = looklab();
    command
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .args(["--output-encoding", "adobe-rgb"])
        .arg("--permalink")
        .arg(&path)
        .args(["--seed", "7"]);
    let stdout = ok(command);
    (path, stdout)
}

#[test]
fn a_camera_match_pins_the_requested_output_encoding() {
    let fixture = camera_fixture();
    let (path, stdout) = unselected_adobe_match(&fixture);
    assert!(
        stdout.contains(&format!(
            " of {} declared points",
            synth::probes().probes.len()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("reachability: reachable"), "{stdout}");
    let permalink =
        Permalink::parse(&std::fs::read_to_string(&path).expect("permalink")).expect("parse");
    let output = permalink.output.expect("output context");
    assert_eq!(output.camera.encoding, OutputEncoding::AdobeRgb);
}

#[test]
fn rendered_verification_rejects_an_observed_encoding_mismatch() {
    let fixture = camera_fixture();
    let (path, _) = unselected_adobe_match(&fixture);
    let observed = fixture.observed_at(
        "observed-srgb",
        &fixture.assessed_settings,
        &fixture.assessed,
    );
    let stderr = rejected(verify(&path, &observed, &fixture.site));
    assert!(stderr.to_lowercase().contains("encoding"), "{stderr}");
}
