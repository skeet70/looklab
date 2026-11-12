// SPDX-License-Identifier: AGPL-3.0-or-later
//! P5.3: the browser worker and `looklab match` agree byte for byte.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use looklab_match::{ScoringMetric, synth};
use looklab_model::LookDocument;
use looklab_model::realization::encode_crm;
use looklab_publish::{
    ArtifactEntry, ArtifactHash, ArtifactName, BundleEntry, MANIFEST_NAME, bundle_artifacts_path,
    bundle_dir, bundle_manifest_path,
};
use looklab_schema::OutputEncoding;
use looklab_schema::{
    BodyRealizationId, BodyRealizationVersionId, BodyStateVersion, BundleSimEntry, ControlEpochId,
    FamilyTargetId, FirmwareId, FirmwareOffset, LookId, MatchBundleRevision, MatchBundleRevisionId,
    MatchParams, MatchParamsFormat, MatchRealizationParams, MatchStage, MatchTransform,
    RealizationRevisionId, RealizationVersion, Sha256Hex, SourceRef, TargetVersion, WbModeKey,
    WbModeOffset,
};
use looklab_wasm::match_api::SourceNames;
use looklab_wasm::{
    MatchInputs, MatchRequest, SimInputs, SimSources, decode_match_reply, encode_match_request,
    run_match,
};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

const FN_MATCH: u32 = looklab_wasm::WORKER_FN_MATCH;

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn write_document(path: &Path, slug: &str, look: &looklab_model::Look) {
    let identity = looklab_model::file::identity(slug, "1.0.0").expect("identity");
    let document = LookDocument::from_look(identity, look.clone()).expect("document");
    std::fs::write(path, document.encode().expect("encode")).expect("write look");
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn match_worker() -> PathBuf {
    const SCRIPT: &str = "site/scripts/wasm-assets.sh";
    let root = workspace_root();
    let output = ProcessCommand::new("sh")
        .current_dir(&root)
        .args([SCRIPT, "--match", "--check"])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "run `nix develop -c sh {SCRIPT} --match --check` in {}: {error}",
                root.display()
            )
        });
    assert!(
        output.status.success(),
        "the match worker is stale or missing; run `nix develop -c sh {SCRIPT} --match` in {}:\n{}{}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let wasm = root.join("site/generated/wasm/looklab_match.wasm");
    assert!(
        wasm.is_file(),
        "{} is missing; run `nix develop -c sh {SCRIPT} --match`",
        wasm.display()
    );
    wasm
}

struct Guest {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    call: TypedFunc<(u32, u32, u32), u64>,
}

impl Guest {
    fn instantiate(path: &Path) -> Self {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path).expect("the match worker loads");
        let mut store = Store::new(&engine, ());
        let instance =
            Instance::new(&mut store, &module, &[]).expect("instantiate without imports");
        Self {
            memory: instance.get_memory(&mut store, "memory").expect("memory"),
            alloc: instance
                .get_typed_func(&mut store, "llw_alloc")
                .expect("alloc"),
            free: instance
                .get_typed_func(&mut store, "llw_free")
                .expect("free"),
            call: instance
                .get_typed_func(&mut store, "llw_call")
                .expect("call"),
            store,
        }
    }

    fn invoke(&mut self, function: u32, input: &[u8]) -> Vec<u8> {
        let len = input.len() as u32;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .expect("allocate input");
        self.memory
            .write(&mut self.store, ptr as usize, input)
            .expect("write input");
        let packed = self
            .call
            .call(&mut self.store, (function, ptr, len))
            .expect("call module");
        self.free
            .call(&mut self.store, (ptr, len))
            .expect("free input");
        let out_ptr = (packed >> 32) as u32;
        let out_len = packed as u32;
        let mut output = vec![0; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut output)
            .expect("read output");
        self.free
            .call(&mut self.store, (out_ptr, out_len))
            .expect("free output");
        output
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    target: PathBuf,
    base: PathBuf,
    crm: PathBuf,
    body: PathBuf,
    transform: PathBuf,
    permalink: PathBuf,
    sim: looklab_schema::SimKey,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let body = synth::body(9).expect("synthetic body");
    let target = temp.path().join("target.looklab");
    let base = temp.path().join("base.looklab");
    write_document(&target, "target", &body.model.base_look);
    write_document(&base, "base", &body.model.base_look);
    let crm = temp.path().join("crm.json");
    std::fs::write(&crm, encode_crm(&body.model.crm).expect("encode crm")).expect("write crm");
    let body_path = temp.path().join("body.yaml");
    std::fs::copy(
        workspace_root().join("data/bodies/fujifilm-x-t5.yaml"),
        &body_path,
    )
    .expect("copy body schema");
    let transform = looklab_cli::TransformFile {
        format: "looklab-transform/1".to_string(),
        version: 1,
        body: None,
        cct: None,
        camera_to_xyz: Some(synth::transform()),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image: Vec::new(),
    };
    let transform_path = temp.path().join("transform.yaml");
    std::fs::write(&transform_path, transform.to_yaml().expect("yaml")).expect("write transform");
    Fixture {
        permalink: temp.path().join("match.permalink"),
        temp,
        target,
        base,
        crm,
        body: body_path,
        transform: transform_path,
        sim: body.model.crm.base_sim.clone(),
    }
}

fn run_match_cli(fixture: &Fixture, permalink: &Path) -> String {
    let output = looklab()
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--body")
        .arg(&fixture.body)
        .arg("--crm")
        .arg(&fixture.crm)
        .arg("--base-look")
        .arg(&fixture.base)
        .arg("--transform")
        .arg(&fixture.transform)
        .arg("--permalink")
        .arg(permalink)
        .args(["--seed", "7", "--realization-version", "1"])
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    std::fs::read_to_string(permalink).expect("permalink")
}

fn browser_inputs(fixture: &Fixture) -> MatchInputs {
    MatchInputs {
        target_look: std::fs::read(&fixture.target).expect("target look"),
        sims: vec![SimInputs {
            sim: fixture.sim.clone(),
            base_look: std::fs::read(&fixture.base).expect("base look"),
            crm: std::fs::read(&fixture.crm).expect("crm"),
            covariance: None,
            sources: SimSources {
                base: Some("base.looklab".to_owned()),
                crm: Some("crm.json".to_owned()),
                covariance: None,
            },
            realization_version: None,
            params: None,
            realization_params: None,
            wb_mode: None,
            firmware: None,
        }],
        excluded: Vec::new(),
        body_yaml: std::fs::read_to_string(&fixture.body).expect("body schema"),
        transform_yaml: Some(std::fs::read_to_string(&fixture.transform).expect("transform yaml")),
        emphasis: "balanced".to_owned(),
        epoch: 1,
        input_transform_version: 1,
        realization_version: 1,
        seed: 7,
        sources: SourceNames {
            target_look: Some("target.looklab".to_owned()),
            body: Some("body.yaml".to_owned()),
            transform: Some("transform.yaml".to_owned()),
        },
        calibration: looklab_match::Calibration::default(),
        selection: None,
        metric: ScoringMetric::RenderedPostClipV1,
        output_encoding: Some(OutputEncoding::Srgb),
        approximated_base: None,
        destination_scope: None,
    }
}

#[test]
fn browser_match_equals_the_cli_permalink_and_evaluates_identically() {
    let fixture = fixture();
    let cli_permalink = run_match_cli(&fixture, &fixture.permalink);
    let again = run_match_cli(&fixture, &fixture.permalink);
    assert_eq!(
        cli_permalink, again,
        "the same inputs and seed write different permalinks"
    );

    let request = MatchRequest {
        inputs: browser_inputs(&fixture),
    };
    let native = run_match(&request).expect("native match");
    assert_eq!(native.permalink, cli_permalink);

    let mut guest = Guest::instantiate(&match_worker());
    let reply = guest.invoke(FN_MATCH, &encode_match_request(&request));
    let wasm = decode_match_reply(&reply).expect("decode the worker reply");
    assert_eq!(wasm.permalink, cli_permalink);
    assert_eq!(
        wasm, native,
        "the worker's evaluation differs from the native facade"
    );
}

const BODY: &str = "fujifilm/x-t5";
const EPOCH: u32 = 1;
const REALIZATION_VERSION: u32 = 2;

struct ExperimentalFixture {
    temp: tempfile::TempDir,
    target: PathBuf,
    bundle: PathBuf,
    bundle_id: String,
    site: PathBuf,
    permalink: PathBuf,
    sim: looklab_schema::SimKey,
    images: looklab_match::ImageCount,
    base_name: String,
    crm_name: String,
    params: Vec<u8>,
    realization_params: Vec<u8>,
    base_source: String,
    crm_source: String,
    body_source: String,
    transform_source: String,
}

fn masked_document(look: &looklab_model::Look, support: Vec<f64>) -> LookDocument {
    let count = looklab_model::probes::v1().probes.len();
    let mut covariance = vec![0.0_f64; count * 9];
    for (index, chunk) in covariance.as_chunks_mut::<9>().0.iter_mut().enumerate() {
        let variance = if index % 2 == 0 { 0.01 } else { 9.0 };
        chunk[0] = variance;
        chunk[4] = variance;
        chunk[8] = variance;
    }
    let identity = looklab_model::file::identity("target", "1.0.0").expect("identity");
    let mut document = LookDocument::from_look(identity, look.clone()).expect("document");
    document
        .file
        .provenance
        .bodies
        .push(looklab_model::file::BodyProvenance {
            body: BODY.to_owned(),
            epoch_map_version: 1,
            epochs: vec![EPOCH],
            input_transform_version: 1,
            realization_params_version: REALIZATION_VERSION,
            control_response_version: 1,
            clip_k: look.clip.k,
            images: 1,
            credentials: 1,
        });
    document
        .with_uncertainty(looklab_model::file::UncertaintyData {
            probe_cov: Some(
                looklab_model::file::ProbeCovariance::new(count, covariance.clone())
                    .expect("covariance"),
            ),
            probe_cov_post: Some(
                looklab_model::file::ProbeCovariance::new(count, covariance).expect("covariance"),
            ),
            support: Some(looklab_model::file::ProbeScalars::new(support).expect("support mask")),
            density: Some(looklab_model::file::Density::new(vec![1.0; count]).expect("density")),
            ..looklab_model::file::UncertaintyData::default()
        })
        .expect("uncertainty")
}

fn experimental_fixture(support: Vec<f64>) -> ExperimentalFixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let body = synth::body(9).expect("synthetic body");
    let target = temp.path().join("target.looklab");
    let document = masked_document(&body.model.base_look, support);
    std::fs::write(&target, document.encode().expect("encode")).expect("write target");
    let body_bytes = std::fs::read(workspace_root().join("data/bodies/fujifilm-x-t5.yaml"))
        .expect("body schema");
    let transform = looklab_cli::TransformFile {
        format: "looklab-transform/1".to_string(),
        version: 1,
        body: None,
        cct: None,
        camera_to_xyz: Some(synth::transform()),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image: Vec::new(),
    };
    let transform_bytes = transform.to_yaml().expect("yaml").into_bytes();

    let sim = body.model.crm.base_sim.clone();
    let body_id = looklab_schema::BodyId::new(BODY).expect("body");
    let epoch = ControlEpochId::new(EPOCH);
    let family = FamilyTargetId {
        family: LookId::new("target").expect("family"),
        version: TargetVersion::new(1).expect("target version"),
    };
    let realization = BodyRealizationVersionId {
        realization: BodyRealizationId {
            body: body_id.clone(),
            epoch,
            native_sim: sim.clone(),
        },
        version: RealizationVersion::new(REALIZATION_VERSION).expect("realization version"),
        target: family.clone(),
    };
    let params = MatchParams {
        format: MatchParamsFormat::V1,
        stage: MatchStage::Native,
        body: body_id.clone(),
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
    let realization_params = MatchRealizationParams {
        format: MatchParamsFormat::V1,
        realization: realization.clone(),
        family: family.clone(),
        version: REALIZATION_VERSION,
        control_response_version: body.model.crm.version,
        clip_k: body.model.base_look.clip.k,
        firmware: vec![FirmwareOffset {
            firmware: FirmwareId::parse("fujifilm/x-t5@1:1.00").expect("firmware"),
            t_shift: 0.0,
            log_g_shift: 0.0,
        }],
    };
    let params_bytes = serde_json::to_vec(&params).expect("params json");
    let realization_bytes = serde_json::to_vec(&realization_params).expect("realization json");
    let base_bytes = LookDocument::from_look(
        looklab_model::file::identity("base", "1.0.0").expect("identity"),
        body.model.base_look.clone(),
    )
    .expect("base document")
    .encode()
    .expect("base bytes");
    let crm_bytes = encode_crm(&body.model.crm).expect("crm bytes");
    let digest = |bytes: &[u8]| Sha256Hex::parse(ArtifactHash::of(bytes).as_str()).expect("digest");
    let base_name = ArtifactHash::of(&base_bytes).as_str().to_owned();
    let crm_name = ArtifactHash::of(&crm_bytes).as_str().to_owned();
    let realization_name = ArtifactHash::of(&realization_bytes).as_str().to_owned();

    let manifest = MatchBundleRevision {
        id: MatchBundleRevisionId::from_digest([0; 32]),
        manifest_hash: MatchBundleRevisionId::from_digest([0; 32]),
        body: body_id.clone(),
        epoch,
        schema_snapshot_version: 1,
        input_transform_version: 1,
        body_state_version: BodyStateVersion::new(1).expect("body state"),
        body_realization_params_version: REALIZATION_VERSION,
        sims: vec![BundleSimEntry {
            native_sim: sim.clone(),
            realization: RealizationRevisionId::for_version(&realization),
            family_version: family.clone(),
            base_artifact: digest(&base_bytes),
            control_response: digest(&crm_bytes),
            covariance: None,
            control_response_version: body.model.crm.version,
            clip_k: body.model.base_look.clip.k,
            realization_params_version: REALIZATION_VERSION,
            body_params: Some(digest(&params_bytes)),
            realization_params: Some(digest(&realization_bytes)),
        }],
        excluded: Vec::new(),
        producer: SourceRef::new("looklab-cli-tests/1").expect("producer"),
        created_at: 0,
    }
    .sealed();
    let manifest_text = manifest.manifest();
    let bundle_id = manifest.id;
    let site = temp.path().join("site");
    let bundle = site.join(
        bundle_dir(&body_id, EPOCH, &bundle_id)
            .expect("bundle dir")
            .as_str(),
    );
    std::fs::create_dir_all(&bundle).expect("bundle directory");
    let mut inventory = Vec::new();
    for (name, bytes) in [
        ("body.yaml".to_owned(), body_bytes),
        ("transform.yaml".to_owned(), transform_bytes),
        ("body_params.json".to_owned(), params_bytes.clone()),
        (base_name.clone(), base_bytes),
        (crm_name.clone(), crm_bytes),
        (realization_name.clone(), realization_bytes.clone()),
    ] {
        std::fs::write(bundle.join(&name), &bytes).expect("bundle artifact");
        inventory.push(ArtifactEntry {
            name: ArtifactName::new(name).expect("artifact name"),
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    let inventory_bytes = serde_json::to_vec(&inventory).expect("inventory json");
    std::fs::write(bundle.join("artifacts.json"), &inventory_bytes).expect("inventory");
    std::fs::write(bundle.join(MANIFEST_NAME), &manifest_text).expect("manifest");

    let target_bytes = std::fs::read(&target).expect("target bytes");
    let index = looklab_publish::SiteIndex {
        format: looklab_publish::SITE_FORMAT.to_owned(),
        community_snapshot_version: 0,
        bodies: Vec::new(),
        manuals: Vec::new(),
        interfaces: Vec::new(),
        input_transforms: Vec::new(),
        looks: vec![looklab_publish::LookSummary {
            slug: looklab_model::file::Slug::new("target").expect("slug"),
            name: "Target".to_owned(),
            kind: looklab_model::file::LookKind::NativeFamily,
            wb_policy: looklab_model::file::WbPolicy::Adaptive,
            visibility: looklab_publish::LookVisibility::Public,
            head: None,
            selected: None,
            versions: vec![looklab_publish::VersionSummary {
                support: looklab_publish::PublishedSupport::Unstated,
                version: looklab_model::file::LookVersion::new("1.0.0").expect("version"),
                status: looklab_publish::LookStatus::Published,
                matchable: true,
                card_eligible: false,
                approximated: false,
                applicability: looklab_schema::Applicability::Current,
                eligibility_status: None,
                card: None,
                manifest: looklab_publish::SitePath::new("data/looks/target/1.0.0/manifest.json")
                    .expect("manifest path"),
                manifest_sha256: looklab_publish::ArtifactHash::of(b""),
                manifest_bytes: 0,
                artifact: looklab_publish::ArtifactName::new("look.looklab").expect("artifact"),
                sha256: looklab_publish::ArtifactHash::of(&target_bytes),
                bytes: target_bytes.len() as u64,
            }],
        }],
        scenes: Vec::new(),
        reports: Vec::new(),
        eligibility: Vec::new(),
        current: Vec::new(),
        bundles: vec![BundleEntry {
            bundle: bundle_id,
            body: body_id.clone(),
            epoch: EPOCH,
            path: bundle_manifest_path(&body_id, EPOCH, &bundle_id).expect("manifest path"),
            sha256: ArtifactHash::of(manifest_text.as_bytes()),
            bytes: manifest_text.len() as u64,
            artifacts_path: bundle_artifacts_path(&body_id, EPOCH, &bundle_id)
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

    let relative = |path: &Path| {
        path.strip_prefix(temp.path())
            .expect("inside the fixture")
            .to_string_lossy()
            .into_owned()
    };
    let base_source = relative(&bundle.join(&base_name));
    let crm_source = relative(&bundle.join(&crm_name));
    let body_source = relative(&bundle.join("body.yaml"));
    let transform_source = relative(&bundle.join("transform.yaml"));

    ExperimentalFixture {
        permalink: temp.path().join("experimental.permalink"),
        images: looklab_match::ImageCount::new(1),
        bundle_id: bundle_id.to_hex(),
        temp,
        target,
        bundle,
        site,
        sim,
        base_name,
        crm_name,
        params: params_bytes,
        realization_params: realization_bytes,
        base_source,
        crm_source,
        body_source,
        transform_source,
    }
}

fn run_experimental_cli(fixture: &ExperimentalFixture) -> (String, String) {
    let output = looklab()
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .args(["--selection", "experimental"])
        .arg("--bundle-revision")
        .arg(&fixture.bundle_id)
        .arg("--publication")
        .arg(&fixture.site)
        .args(["--seed", "7"])
        .arg("--permalink")
        .arg(&fixture.permalink)
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let permalink = std::fs::read_to_string(&fixture.permalink).expect("permalink");
    (permalink, stdout)
}

fn run_experimental_cli_without_selection(fixture: &ExperimentalFixture) -> std::process::Output {
    looklab()
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--publication")
        .arg(&fixture.site)
        .args(["--seed", "7"])
        .arg("--permalink")
        .arg(fixture.temp.path().join("unselected.permalink"))
        .output()
        .expect("run")
}

fn experimental_inputs(fixture: &ExperimentalFixture) -> MatchInputs {
    MatchInputs {
        target_look: std::fs::read(&fixture.target).expect("target look"),
        sims: vec![SimInputs {
            sim: fixture.sim.clone(),
            base_look: std::fs::read(fixture.bundle.join(&fixture.base_name)).expect("base look"),
            crm: std::fs::read(fixture.bundle.join(&fixture.crm_name)).expect("crm"),
            covariance: None,
            sources: SimSources {
                base: Some(fixture.base_source.clone()),
                crm: Some(fixture.crm_source.clone()),
                covariance: None,
            },
            realization_version: Some(REALIZATION_VERSION),
            params: Some(fixture.params.clone()),
            realization_params: Some(fixture.realization_params.clone()),
            wb_mode: None,
            firmware: None,
        }],
        excluded: Vec::new(),
        body_yaml: std::fs::read_to_string(fixture.bundle.join("body.yaml")).expect("body schema"),
        transform_yaml: Some(
            std::fs::read_to_string(fixture.bundle.join("transform.yaml")).expect("transform yaml"),
        ),
        emphasis: "balanced".to_owned(),
        epoch: EPOCH,
        input_transform_version: 1,
        realization_version: REALIZATION_VERSION,
        seed: 7,
        sources: SourceNames {
            target_look: Some("target.looklab".to_owned()),
            body: Some(fixture.body_source.clone()),
            transform: Some(fixture.transform_source.clone()),
        },
        calibration: looklab_match::Calibration::default(),
        selection: Some(looklab_wasm::match_api::MatchSelection {
            bundle: Sha256Hex::parse(&fixture.bundle_id).expect("bundle digest"),
            mode: looklab_match::SelectionMode::Experimental,
            reports: Vec::new(),
            firmware: None,
            wb_mode: None,
            controls: Vec::new(),
        }),
        metric: ScoringMetric::RenderedPostClipV1,
        output_encoding: Some(OutputEncoding::Srgb),
        approximated_base: None,
        destination_scope: None,
    }
}

#[test]
fn an_empty_support_mask_matches_experimentally_in_the_cli_and_the_browser() {
    let fixture = experimental_fixture(vec![0.0; looklab_model::probes::v1().probes.len()]);
    let (cli_permalink, stdout) = run_experimental_cli(&fixture);
    assert!(
        stdout.contains("support: experimental estimate resting on"),
        "{stdout}"
    );
    assert!(
        cli_permalink.contains("|selection=experimental"),
        "{cli_permalink}"
    );

    let request = MatchRequest {
        inputs: experimental_inputs(&fixture),
    };
    let native = run_match(&request).expect("native experimental match");
    assert_eq!(native.permalink, cli_permalink);
    assert_eq!(native.card.experimental, Some(fixture.images));
    assert_eq!(
        native.card.confidence,
        looklab_wasm::dto::ConfidenceDto::Low,
        "the fallback caps confidence at low"
    );
    assert!(
        !run_experimental_cli_without_selection(&fixture)
            .status
            .success(),
        "an unselected CLI match refuses the zero-support target"
    );
    let mut unselected = experimental_inputs(&fixture);
    unselected.selection = None;
    let refusal = run_match(&MatchRequest { inputs: unselected })
        .expect_err("an unselected facade match refuses the zero-support target");
    assert!(matches!(
        refusal,
        looklab_wasm::FacadeError::Match(looklab_match::MatchError::NoSupportedProbes)
    ));

    let mut guest = Guest::instantiate(&match_worker());
    let reply = guest.invoke(FN_MATCH, &encode_match_request(&request));
    let wasm = decode_match_reply(&reply).expect("decode the worker reply");
    assert_eq!(wasm.permalink, cli_permalink);
    assert_eq!(
        wasm, native,
        "the worker's experimental evaluation differs from the native facade"
    );
}

#[test]
fn a_support_mask_is_used_even_when_experimental_selection_is_requested() {
    let mut support = vec![0.0; looklab_model::probes::v1().probes.len()];
    for index in (1..support.len()).step_by(2) {
        support[index] = 1.0;
    }
    let fixture = experimental_fixture(support);
    let request = MatchRequest {
        inputs: experimental_inputs(&fixture),
    };
    let native = run_match(&request).expect("native experimental match");
    assert_eq!(
        native.card.experimental, None,
        "a non-empty published mask is not the experimental fallback"
    );
    assert_eq!(run_experimental_cli(&fixture).0, native.permalink);
}
