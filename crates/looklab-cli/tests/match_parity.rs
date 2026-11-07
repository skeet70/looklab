// SPDX-License-Identifier: AGPL-3.0-or-later
//! P5.3: the browser worker and `looklab match` agree byte for byte.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use looklab_match::synth;
use looklab_model::LookDocument;
use looklab_model::realization::encode_crm;
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
        .args(["--seed", "7"])
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
        }],
        excluded: Vec::new(),
        body_yaml: std::fs::read_to_string(&fixture.body).expect("body schema"),
        transform_yaml: Some(std::fs::read_to_string(&fixture.transform).expect("transform yaml")),
        emphasis: "balanced".to_owned(),
        epoch: 1,
        input_transform_version: 1,
        realization_version: 0,
        seed: 7,
        sources: SourceNames {
            target_look: Some("target.looklab".to_owned()),
            body: Some("body.yaml".to_owned()),
            transform: Some("transform.yaml".to_owned()),
        },
        calibration: looklab_match::Calibration::default(),
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
