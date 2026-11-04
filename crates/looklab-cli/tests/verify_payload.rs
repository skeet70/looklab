// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab verify-payload` against a locally built upload: the prep module
//! under wasmtime, a software ECDSA P-256 authenticator and a HAR.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::LazyLock;

use assert_cmd::Command;
use base64::Engine as _;
use jpeg_encoder::{ColorType, Encoder};
use looklab_color::RgbSpace;
use looklab_prep::{Image3, PrepParams, decode_jpeg};
use looklab_prep_module::{
    Function, ModuleError, PrepareRequest, PreparedImageView, PriorTransformSpec, Response,
};
use looklab_upload::{
    BatchManifest, BlobKind, BodyId, Challenge, ContributionKind, ControlEntry, ControlKey,
    ControlSettings, EnvelopeParts, FirmwareVersionString, Intent, ManifestImage, Nonce, PowParams,
    RawSerial, RerenderGroupId, SimKey, StepIndex, Tagged, WebAuthnAssertion, encode_envelope,
    manifest_hash, solve,
};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

const ORIGIN: &str = "https://looklab.test";
const RP_ID: &str = "looklab.test";
const SEED: [u8; 32] = [0x10; 32];
const PRIOR_VERSION: u32 = 3;
const CREDENTIAL: &[u8] = b"credential-alpha";
const OTHER_CREDENTIAL: &[u8] = b"credential-beta";
const BODY: &str = "fujifilm/x-s10";
const FIRMWARE: &str = "1.00";
const SIM: &str = "provia";
const POW: PowParams = PowParams {
    memory_kib: 8,
    iterations: 1,
    difficulty_bits_per_image: 0.0,
};

struct Fixture {
    dir: PathBuf,
    data: PathBuf,
    module: PathBuf,
    raw: PathBuf,
    jpegs: Vec<PathBuf>,
    seeds: PathBuf,
    images: Vec<PreparedImageView>,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(build_fixture);
    &FIXTURE
}

fn build_fixture() -> Fixture {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("verify-payload");
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let raw = std::fs::read(workspace.join("fixtures/fujifilm-x-s10.raf")).expect("RAF fixture");
    let raw_path = dir.join("fujifilm-x-s10.raf");
    std::fs::write(&raw_path, &raw).expect("RAW copy");

    let embedded = raf_jpeg(&raw);
    let decoded = decode_jpeg(&embedded).expect("the embedded JPEG decodes");
    let renders = vec![
        embedded.clone(),
        splice_exif(&embedded, &jpeg_from_image(&decoded.image, 0.7)),
    ];
    let jpegs: Vec<PathBuf> = ["render-a.jpg", "render-b.jpg"]
        .iter()
        .map(|name| dir.join(name))
        .collect();
    for (path, bytes) in jpegs.iter().zip(&renders) {
        std::fs::write(path, bytes).expect("JPEG copy");
    }

    let data = dir.join("data");
    std::fs::create_dir_all(data.join("bodies")).expect("data directory");
    let body = std::fs::read_to_string(workspace.join("data/bodies/fujifilm-x-t5.body.yaml"))
        .expect("body entity")
        .replace("id: fujifilm/x-t5", "id: fujifilm/x-s10")
        .replace("model: X-T5", "model: X-S10")
        .replace("exif_aliases: [X-T5]", "exif_aliases: [X-S10]");
    std::fs::write(data.join("bodies/fujifilm-x-s10.body.yaml"), body).expect("body copy");
    let schema = std::fs::read_to_string(workspace.join("data/bodies/fujifilm-x-t5.yaml"))
        .expect("control schema")
        .replace("body: fujifilm/x-t5", "body: fujifilm/x-s10");
    std::fs::write(data.join("bodies/fujifilm-x-s10.yaml"), schema).expect("schema copy");

    let seeds = dir.join("seeds.json");
    let seed_text = format!(
        "{{\"format\":\"looklab-seeds/1\",\"prior_transform_version\":{PRIOR_VERSION},\"entries\":[{{\"key\":\"fujifilm-x-s10.raf\",\"seed\":\"{}\"}}]}}",
        hex(&SEED)
    );
    std::fs::write(&seeds, seed_text).expect("seed file");

    let module = build_module();
    let images = prepare(&module, &raw, &renders);
    Fixture {
        dir,
        data,
        module,
        raw: raw_path,
        jpegs,
        seeds,
        images,
    }
}

fn build_module() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the CLI crate is under crates/");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || root.join("target"),
        |dir| {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        },
    );
    let target_dir = target_dir.join("verify-payload-wasm");
    let status = ProcessCommand::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "looklab-prep-module",
            "--target-dir",
        ])
        .arg(&target_dir)
        .status()
        .expect("cargo runs");
    assert!(status.success(), "building the prep module failed");
    let wasm = target_dir.join("wasm32-unknown-unknown/release/looklab_prep_module.wasm");
    let optimized = wasm.with_extension("opt.wasm");
    let status = ProcessCommand::new("wasm-opt")
        .arg("--remove-imports")
        .arg(&wasm)
        .arg("-o")
        .arg(&optimized)
        .status()
        .expect("wasm-opt runs");
    assert!(status.success(), "removing wasm-bindgen imports failed");
    std::fs::rename(&optimized, &wasm).expect("optimized module replaces the artifact");
    wasm
}

/// The JPEG preview embedded in a RAF: offset at 0x54, length at 0x58, big-endian.
fn raf_jpeg(raw: &[u8]) -> Vec<u8> {
    let offset = u32::from_be_bytes(raw[0x54..0x58].try_into().expect("offset")) as usize;
    let length = u32::from_be_bytes(raw[0x58..0x5c].try_into().expect("length")) as usize;
    raw[offset..offset + length].to_vec()
}

fn exif_segment(jpeg: &[u8]) -> Option<&[u8]> {
    let mut index = 2;
    while index + 4 <= jpeg.len() {
        if jpeg[index] != 0xff {
            return None;
        }
        let marker = jpeg[index + 1];
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
        if marker == 0xe1 && jpeg.get(index + 4..index + 10)? == b"Exif\0\0" {
            return Some(&jpeg[index..index + 2 + length]);
        }
        index += 2 + length;
    }
    None
}

fn splice_exif(original: &[u8], render: &[u8]) -> Vec<u8> {
    let Some(segment) = exif_segment(original) else {
        return render.to_vec();
    };
    let mut out = Vec::with_capacity(render.len() + segment.len());
    out.extend_from_slice(&render[..2]);
    out.extend_from_slice(segment);
    out.extend_from_slice(&render[2..]);
    out
}

fn jpeg_from_image(image: &Image3, exposure: f64) -> Vec<u8> {
    let codes: Vec<u8> = image
        .data
        .iter()
        .flat_map(|rgb| {
            rgb.map(|channel| {
                let linear = (f64::from(channel) * exposure).clamp(0.0, 1.0);
                (RgbSpace::Srgb.encode(linear) * 255.0).round() as u8
            })
        })
        .collect();
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, 92)
        .encode(
            &codes,
            image.width as u16,
            image.height as u16,
            ColorType::Rgb,
        )
        .expect("JPEG encodes");
    bytes
}

fn prepare(module: &Path, raw: &[u8], jpegs: &[Vec<u8>]) -> Vec<PreparedImageView> {
    let request = PrepareRequest {
        raw: raw.to_vec(),
        jpegs: jpegs.to_vec(),
        prior: PriorTransformSpec::CameraMatrices {
            version: PRIOR_VERSION,
        },
        seed: SEED,
        params: PrepParams::default(),
    };
    let input = postcard::to_allocvec(&request).expect("prepare request encodes");
    let output = call_module(module, Function::Prepare.id(), &input);
    let (result, rest) = postcard::take_from_bytes::<Result<Response, ModuleError>>(&output)
        .expect("module response decodes");
    assert!(rest.is_empty(), "module response has trailing bytes");
    match result.expect("preparation succeeds") {
        Response::Prepare(response) => response.images,
        other => panic!("unexpected module response {other:?}"),
    }
}

fn call_module(module_path: &Path, fn_id: u32, input: &[u8]) -> Vec<u8> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, module_path).expect("module loads");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("module has no imports");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("module exports memory");
    let alloc: TypedFunc<u32, u32> = instance
        .get_typed_func(&mut store, "ll_alloc")
        .expect("ll_alloc is exported");
    let free: TypedFunc<(u32, u32), ()> = instance
        .get_typed_func(&mut store, "ll_free")
        .expect("ll_free is exported");
    let call: TypedFunc<(u32, u32, u32), u64> = instance
        .get_typed_func(&mut store, "ll_call")
        .expect("ll_call is exported");

    let in_ptr = alloc
        .call(&mut store, input.len() as u32)
        .expect("allocation succeeds");
    memory
        .write(&mut store, in_ptr as usize, input)
        .expect("input is written");
    let packed = call
        .call(&mut store, (fn_id, in_ptr, input.len() as u32))
        .expect("call succeeds");
    free.call(&mut store, (in_ptr, input.len() as u32))
        .expect("input frees");
    let out_ptr = (packed >> 32) as u32;
    let out_len = (packed & u64::from(u32::MAX)) as u32;
    let mut output = vec![0u8; out_len as usize];
    memory
        .read(&store, out_ptr as usize, &mut output)
        .expect("output is read");
    free.call(&mut store, (out_ptr, out_len))
        .expect("output frees");
    output
}

/// The fixture's makernotes: DR100, neutral tones, Saturation 0, WB shift 0/0, EC 0,
/// grain off, clarity 0, noise reduction 0, FilmMode provia.
fn settings() -> ControlSettings {
    let entries = [
        "dynamic_range",
        "highlight_tone",
        "shadow_tone",
        "color",
        "color_chrome_effect",
        "color_chrome_fx_blue",
        "wb_shift_r",
        "wb_shift_b",
        "exposure_compensation",
        "grain",
        "clarity",
        "noise_reduction",
    ]
    .into_iter()
    .map(|key| ControlEntry {
        key: ControlKey::new(key).expect("control key"),
        step: StepIndex::new(0),
    })
    .collect();
    ControlSettings::new(entries).expect("unique settings")
}

struct Site {
    manifest: BatchManifest,
    blobs: Vec<Vec<u8>>,
    credential_id: Vec<u8>,
    origin: String,
    rp_id: String,
    client_extra: Option<(String, Value)>,
    extensions: Option<Vec<(String, i64)>>,
    challenge: Option<Challenge>,
    serial: Option<Vec<u8>>,
}

impl Site {
    fn new() -> Self {
        let fixture = fixture();
        let images = fixture
            .images
            .iter()
            .map(|image| ManifestImage {
                meta: image.metadata.clone(),
                rerender_group: image.rerender_group.map(RerenderGroupId::new),
                blob_sha256: sha256(&image.blob),
                blob_kind: BlobKind::Samples,
            })
            .collect();
        Self {
            manifest: BatchManifest {
                kind: ContributionKind::Library,
                intent: Intent::Native,
                body: BodyId::new(BODY).expect("body id"),
                firmware_version: FirmwareVersionString::new(FIRMWARE).expect("firmware"),
                sim_key: SimKey::new(SIM).expect("sim key"),
                settings: settings(),
                images,
            },
            blobs: fixture
                .images
                .iter()
                .map(|image| image.blob.clone())
                .collect(),
            credential_id: CREDENTIAL.to_vec(),
            origin: ORIGIN.to_owned(),
            rp_id: RP_ID.to_owned(),
            client_extra: None,
            extensions: None,
            challenge: None,
            serial: None,
        }
    }

    fn with_credential(mut self, credential: &[u8]) -> Self {
        self.credential_id = credential.to_vec();
        self
    }

    fn with_extra_key(mut self, key: &str, value: Value) -> Self {
        self.client_extra = Some((key.to_owned(), value));
        self
    }

    fn with_extensions(mut self, entries: &[(&str, i64)]) -> Self {
        self.extensions = Some(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), *value))
                .collect(),
        );
        self
    }

    fn with_origin(mut self, origin: &str) -> Self {
        self.origin = origin.to_owned();
        self
    }

    fn with_rp_id(mut self, rp_id: &str) -> Self {
        self.rp_id = rp_id.to_owned();
        self
    }

    fn with_challenge(mut self, challenge: Challenge) -> Self {
        self.challenge = Some(challenge);
        self
    }

    fn with_firmware(mut self, firmware: &str) -> Self {
        self.manifest.firmware_version = FirmwareVersionString::new(firmware).expect("firmware");
        self
    }

    fn with_serial(mut self, serial: &[u8]) -> Self {
        self.serial = Some(serial.to_vec());
        self
    }

    fn with_hidden_setting(mut self) -> Self {
        let mut entries = self.manifest.settings.entries().to_vec();
        entries.push(ControlEntry {
            key: ControlKey::new("hidden").expect("control key"),
            step: StepIndex::new(12_345),
        });
        self.manifest.settings = ControlSettings::new(entries).expect("unique settings");
        self
    }

    fn with_positive_zero_tone(mut self) -> Self {
        if let Some(look) = self.manifest.images[0].meta.look.as_mut() {
            look.highlight_tone = Some(Tagged {
                raw: 0,
                value: Some(0.0),
            });
        }
        self
    }

    fn reversed(mut self) -> Self {
        self.manifest.images.reverse();
        self.blobs.reverse();
        self
    }

    fn body(&self) -> Vec<u8> {
        let nonce = Nonce::from_bytes([0x42; 32]);
        let hash = manifest_hash(&self.manifest);
        let challenge = self
            .challenge
            .unwrap_or_else(|| Challenge::new(&nonce, &hash));
        let assertion = assertion(
            &challenge,
            &self.origin,
            &self.rp_id,
            &self.credential_id,
            self.client_extra.as_ref(),
            self.extensions.as_deref(),
        );
        let pow = solve(&challenge, self.manifest.images.len() as u32, &POW, 0)
            .expect("proof-of-work solves");
        let serial = self
            .serial
            .as_deref()
            .map(|ascii| RawSerial::from_envelope_ascii(ascii).expect("valid serial"));
        let parts = EnvelopeParts {
            nonce,
            manifest: &self.manifest,
            assertion: &assertion,
            pow: &pow,
            serial: serial.as_ref(),
            blobs: &self.blobs,
        };
        encode_envelope(&parts)
    }
}

fn assertion(
    challenge: &Challenge,
    origin: &str,
    rp_id: &str,
    credential_id: &[u8],
    extra: Option<&(String, Value)>,
    extensions: Option<&[(String, i64)]>,
) -> WebAuthnAssertion {
    let mut client_data_json = format!(
        "{{\"type\":\"webauthn.get\",\"challenge\":\"{}\",\"origin\":{},\"crossOrigin\":false",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge.as_bytes()),
        serde_json::to_string(origin).expect("origin quotes")
    );
    if let Some((key, value)) = extra {
        client_data_json.push(',');
        client_data_json.push_str(&serde_json::to_string(key).expect("key quotes"));
        client_data_json.push(':');
        client_data_json.push_str(&value.to_string());
    }
    client_data_json.push('}');
    let client_data_json = client_data_json.into_bytes();

    let mut authenticator_data = Vec::new();
    authenticator_data.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
    let flags = 0x01 | if extensions.is_some() { 0x80 } else { 0 };
    authenticator_data.push(flags);
    authenticator_data.extend_from_slice(&0u32.to_be_bytes());
    if let Some(entries) = extensions {
        let map = ciborium::value::Value::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    (
                        ciborium::value::Value::Text(key.clone()),
                        ciborium::value::Value::Integer((*value).into()),
                    )
                })
                .collect(),
        );
        ciborium::ser::into_writer(&map, &mut authenticator_data).expect("extensions encode");
    }

    let mut message = authenticator_data.clone();
    message.extend_from_slice(&Sha256::digest(&client_data_json));
    let key = SigningKey::from_slice(&[7u8; 32]).expect("valid P-256 scalar");
    let signature: Signature = key.sign(&message);
    WebAuthnAssertion {
        credential_id: credential_id.to_vec(),
        authenticator_data,
        client_data_json,
        signature: signature.to_der().as_bytes().to_vec(),
    }
}

fn default_har(site: &Site) -> HarBuilder {
    HarBuilder::new()
        .post("https://looklab.test/api/nonce")
        .upload(site.body())
        .get("https://looklab.test/status/abc123")
        .post_body(
            "https://looklab.test/api/passkey/register/start",
            br#"{"display_handle":"alice"}"#,
        )
        .post_body(
            "https://looklab.test/api/passkey/register/finish",
            br#"{"handle":"registration-handle","credential":{"id":"Y3JlZA"}}"#,
        )
}

struct HarBuilder {
    entries: Vec<Value>,
}

impl HarBuilder {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(mut self, url: &str) -> Self {
        self.entries.push(entry("GET", url, None));
        self
    }

    fn post(mut self, url: &str) -> Self {
        self.entries.push(entry("POST", url, None));
        self
    }

    fn post_body(mut self, url: &str, body: &[u8]) -> Self {
        self.entries.push(entry("POST", url, Some(body)));
        self
    }

    fn upload(mut self, body: Vec<u8>) -> Self {
        self.entries.push(entry(
            "POST",
            "https://looklab.test/api/upload",
            Some(&body),
        ));
        self
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        let Some(last) = self.entries.last_mut() else {
            panic!("a header needs a request");
        };
        last["request"]["headers"]
            .as_array_mut()
            .expect("headers are an array")
            .push(json!({"name": name, "value": value}));
        self
    }

    fn json(&self) -> String {
        json!({
            "log": {
                "version": "1.2",
                "creator": {"name": "looklab-verify-payload-test", "version": "1"},
                "entries": self.entries,
            }
        })
        .to_string()
    }
}

fn entry(method: &str, url: &str, body: Option<&[u8]>) -> Value {
    let mut request = json!({
        "method": method,
        "url": url,
        "headers": [
            {"name": "Host", "value": "looklab.test"},
            {"name": "Accept", "value": "*/*"},
            {"name": "Sec-Fetch-Site", "value": "same-origin"}
        ],
    });
    if let Some(body) = body {
        request["postData"] = json!({
            "mimeType": "application/octet-stream",
            "text": base64::engine::general_purpose::STANDARD.encode(body),
            "encoding": "base64",
        });
    }
    json!({"request": request})
}

fn run(fixture: &Fixture, hars: &[&Path], state: &Path) -> (bool, String) {
    let mut command = Command::cargo_bin("looklab").expect("built binary");
    command
        .args(["verify-payload", "--seeds"])
        .arg(&fixture.seeds);
    for har in hars {
        command.arg("--har").arg(har);
    }
    command
        .arg("--module")
        .arg(&fixture.module)
        .args(["--origin", ORIGIN, "--rp-id", RP_ID, "--data"])
        .arg(&fixture.data)
        .args(["--kind", "library", "--intent", "native"])
        .arg("--state")
        .arg(state)
        .args([
            "--pow-memory-kib",
            "8",
            "--pow-iterations",
            "1",
            "--pow-difficulty",
            "0",
        ])
        .arg(&fixture.raw)
        .args(&fixture.jpegs);
    let output = command.output().expect("command runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

fn har_path(name: &str, json: &str) -> PathBuf {
    let path = fixture().dir.join(format!("har-{name}.json"));
    std::fs::write(&path, json).expect("HAR writes");
    path
}

fn state_path(name: &str) -> PathBuf {
    let path = fixture().dir.join(format!("state-{name}.json"));
    let _ = std::fs::remove_file(&path);
    path
}

fn splice_manifest(body: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut out = body.to_vec();
    let len = u32::from_le_bytes(out[40..44].try_into().expect("manifest length"));
    let len = len as usize;
    out.splice(44 + len..44 + len, extra.iter().copied());
    out[40..44].copy_from_slice(&((len + extra.len()) as u32).to_le_bytes());
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    output
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn reproduces_an_upload_and_passes() {
    let fixture = fixture();
    let site = Site::new();
    let har = har_path("pass", &default_har(&site).json());
    let state = state_path("pass");
    let (success, output) = run(fixture, &[har.as_path()], &state);

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
    assert!(output.contains("reproduced byte for byte"), "{output}");
    assert!(
        output.contains("reproduced and declared fields match"),
        "{output}"
    );
    assert!(
        output.contains("body: fujifilm/x-s10 (reproduced"),
        "{output}"
    );
    assert!(output.contains("firmware: 1.00 (reproduced"), "{output}");
    assert!(output.contains("sim: provia (reproduced"), "{output}");
    assert!(output.contains("kind: Library (declared)"), "{output}");
    assert!(output.contains("intent: native (declared)"), "{output}");
    assert!(output.contains("display_handle"), "{output}");
    assert!(output.contains("registration-handle"), "{output}");
    assert!(
        output.contains("Residual channels (documented, not closed):"),
        "{output}"
    );

    let recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(&state).expect("state file")).expect("json");
    assert_eq!(recorded["credentials"][0]["credential_id"], hex(CREDENTIAL));
}

#[test]
fn fails_on_a_fabricated_batch_field() {
    let fixture = fixture();
    let site = Site::new().with_firmware("1.01");
    let har = har_path("firmware", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("firmware"));

    assert!(!success, "{output}");
    assert!(output.contains("firmware version is 1.01"), "{output}");
    assert!(output.contains("reproduced 1.00"), "{output}");
}

#[test]
fn fails_on_a_setting_outside_the_schema() {
    let fixture = fixture();
    let site = Site::new().with_hidden_setting();
    let har = har_path("hidden-setting", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("hidden-setting"));

    assert!(!success, "{output}");
    assert!(output.contains("hidden"), "{output}");
    assert!(output.contains("not reproduced"), "{output}");
}

#[test]
fn fails_on_signed_zero_manifest_bytes() {
    let fixture = fixture();
    let site = Site::new().with_positive_zero_tone();
    let har = har_path("signed-zero", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("signed-zero"));

    assert!(!success, "{output}");
    assert!(
        output.contains("the manifest does not match the reproduced and declared manifest"),
        "{output}"
    );
}

#[test]
fn fails_on_an_unrelated_serial() {
    let fixture = fixture();
    let site = Site::new().with_serial(b"SERIAL999");
    let har = har_path("serial", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("serial"));

    assert!(!success, "{output}");
    assert!(
        output.contains("matches no input file's EXIF serial"),
        "{output}"
    );
}

#[test]
fn fails_on_an_extra_manifest_field() {
    let fixture = fixture();
    let site = Site::new();
    let mut har = default_har(&site);
    har.entries[1] = entry(
        "POST",
        "https://looklab.test/api/upload",
        Some(&splice_manifest(&site.body(), b"\x00")),
    );
    let har = har_path("extra-field", &har.json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("extra-field"));

    assert!(!success, "{output}");
    assert!(output.contains("FAIL"), "{output}");
    assert!(output.contains("manifest"), "{output}");
}

#[test]
fn fails_on_a_reordered_field() {
    let fixture = fixture();
    let site = Site::new().reversed();
    let har = har_path("reordered", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("reordered"));

    assert!(!success, "{output}");
    assert!(output.contains("image 0"), "{output}");
}

#[test]
fn fails_on_trailing_bytes() {
    let fixture = fixture();
    let site = Site::new();
    let mut body = site.body();
    body.push(0);
    let har = HarBuilder::new()
        .post("https://looklab.test/api/nonce")
        .upload(body)
        .json();
    let har = har_path("trailing", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("trailing"));

    assert!(!success, "{output}");
    assert!(output.contains("unexpected bytes"), "{output}");
}

#[test]
fn fails_on_altered_low_order_sample_bits() {
    let fixture = fixture();
    let site = Site::new();
    let mut body = site.body();
    let last = body.len() - 1;
    body[last] ^= 0x01;
    let har = HarBuilder::new()
        .post("https://looklab.test/api/nonce")
        .upload(body)
        .json();
    let har = har_path("bits", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("bits"));

    assert!(!success, "{output}");
    assert!(output.contains("does not hash"), "{output}");
}

#[test]
fn fails_on_an_unexpected_request() {
    let fixture = fixture();
    let site = Site::new();
    let har = default_har(&site)
        .get("https://looklab.test/collect?d=1")
        .json();
    let har = har_path("unexpected", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("unexpected"));

    assert!(!success, "{output}");
    assert!(output.contains("unexpected request"), "{output}");
    assert!(output.contains("/collect?d=1"), "{output}");
}

#[test]
fn fails_on_a_body_in_a_nonce_request() {
    let fixture = fixture();
    let site = Site::new();
    let har = HarBuilder::new()
        .post_body("https://looklab.test/api/nonce", b"{\"hidden\":1}")
        .upload(site.body())
        .json();
    let har = har_path("nonce-body", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("nonce-body"));

    assert!(!success, "{output}");
    assert!(output.contains("carries a body"), "{output}");
    assert!(output.contains("hidden"), "{output}");
}

#[test]
fn marks_a_custom_header_for_review() {
    let fixture = fixture();
    let site = Site::new();
    let har = default_har(&site).header("X-Secret", "hidden-value").json();
    let har = har_path("header", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("header"));

    assert!(success, "{output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains("X-Secret"), "{output}");
    assert!(output.contains("hidden-value"), "{output}");
}

#[test]
fn fails_on_a_wrong_challenge() {
    let fixture = fixture();
    let site = Site::new().with_challenge(Challenge::from_bytes([0x11; 32]));
    let har = har_path("challenge", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("challenge"));

    assert!(!success, "{output}");
    assert!(output.contains("challenge"), "{output}");
}

#[test]
fn fails_on_a_wrong_origin() {
    let fixture = fixture();
    let site = Site::new().with_origin("https://evil.test");
    let har = har_path("origin", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("origin"));

    assert!(!success, "{output}");
    assert!(output.contains("origin"), "{output}");
}

#[test]
fn fails_on_a_wrong_rp_id_hash() {
    let fixture = fixture();
    let site = Site::new().with_rp_id("other.test");
    let har = har_path("rp-id", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("rp-id"));

    assert!(!success, "{output}");
    assert!(output.contains("rpIdHash"), "{output}");
}

#[test]
fn fails_when_the_credential_id_changes_between_two_hars() {
    let fixture = fixture();
    let first = har_path("credential-a", &default_har(&Site::new()).json());
    let second = har_path(
        "credential-b",
        &default_har(&Site::new().with_credential(OTHER_CREDENTIAL)).json(),
    );
    let (success, output) = run(
        fixture,
        &[first.as_path(), second.as_path()],
        &state_path("credential-two-hars"),
    );

    assert!(!success, "{output}");
    assert!(output.contains("credential id differs"), "{output}");
}

#[test]
fn fails_when_the_credential_id_changes_between_runs() {
    let fixture = fixture();
    let state = state_path("credential-runs");
    let first = har_path("run-a", &default_har(&Site::new()).json());
    let (success, output) = run(fixture, &[first.as_path()], &state);
    assert!(success, "{output}");

    let second = har_path(
        "run-b",
        &default_har(&Site::new().with_credential(OTHER_CREDENTIAL)).json(),
    );
    let (success, output) = run(fixture, &[second.as_path()], &state);
    assert!(!success, "{output}");
    assert!(output.contains("records for this site"), "{output}");
}

#[test]
fn marks_an_unknown_client_data_key_for_review() {
    let fixture = fixture();
    let site = Site::new().with_extra_key("animal", json!("cat"));
    let har = har_path("client-extra", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("client-extra"));

    assert!(success, "{output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains("\"animal\""), "{output}");
    assert!(output.contains("\"cat\""), "{output}");
}

#[test]
fn marks_an_unexpected_top_origin_for_review() {
    let fixture = fixture();
    let site = Site::new().with_extra_key("topOrigin", json!("https://top.test"));
    let har = har_path("top-origin", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("top-origin"));

    assert!(success, "{output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains("topOrigin"), "{output}");
    assert!(output.contains("https://top.test"), "{output}");
}

#[test]
fn marks_a_non_canonical_client_data_json_for_review() {
    let fixture = fixture();
    let site = Site::new();
    let assertion = site.body();
    let har = default_har(&site);
    // Re-encode the same client_data_json with whitespace and a Unicode escape.
    let body = rewrite_client_data(&assertion, |json| {
        json.replacen("{\"type\"", "{ \"type\"", 1)
            .replace("https://looklab.test", "https:\\u002f\\u002flooklab.test")
    });
    let mut har = har;
    har.entries[1] = entry("POST", "https://looklab.test/api/upload", Some(&body));
    let har = har_path("non-canonical", &har.json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("non-canonical"));

    assert!(success, "{output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(
        output.contains("not the WebAuthn serialization"),
        "{output}"
    );
}

fn rewrite_client_data(body: &[u8], rewrite: impl Fn(String) -> String) -> Vec<u8> {
    let mut out = body.to_vec();
    let manifest_len =
        u32::from_le_bytes(out[40..44].try_into().expect("manifest length")) as usize;
    let mut index = 44 + manifest_len;
    let credential_len = usize::from(out[index]);
    index += 1 + credential_len;
    let authenticator_len = usize::from(u16::from_le_bytes([out[index], out[index + 1]]));
    index += 2 + authenticator_len;
    let client_start = index + 2;
    let client_len = usize::from(u16::from_le_bytes([out[index], out[index + 1]]));
    let client_end = client_start + client_len;
    let client = String::from_utf8(out[client_start..client_end].to_vec()).expect("client data");
    let replacement = rewrite(client).into_bytes();
    assert!(replacement.len() <= 1024, "client_data_json stays in limit");
    out.splice(client_start..client_end, replacement.iter().copied());
    out[index..index + 2].copy_from_slice(&(replacement.len() as u16).to_le_bytes());
    out
}

#[test]
fn marks_authenticator_extensions_for_review() {
    let fixture = fixture();
    let site = Site::new().with_extensions(&[("credProtect", 1)]);
    let har = har_path("extensions", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("extensions"));

    assert!(success, "{output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains("credProtect"), "{output}");
}
