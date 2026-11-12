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
    BatchManifest, BlobKind, BodyId, CardDeclaration, Challenge, CommunitySnapshotVersion,
    ContributionKind, ControlEntry, ControlKey, ControlSettings, EnvelopeParts, FirmwareId,
    FirmwareVersion, FirmwareVersionString, Intent, ManifestHash, ManifestImage,
    MonochromeAuthorization, Nonce, POW_K, PowParams, RawSerial, RenderClass, RerenderGroupId,
    SendManifest, SettingSource, SimKey, StepIndex, Tagged, WebAuthnAssertion, encode_envelope,
    manifest_hash, parse_render_tag, send_manifest_hash, solve,
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
const CONFIRMATION_FIRMWARE: &str = "owner-reported-1.00";
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
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| build_fixture_in("verify-payload", None));
    &FIXTURE
}

const CARD_TAG: &str = "looklab-card/1 fw=acme-sim/2.0.1 look=provia@1.0.0 cid=0123456789abcdef";
const MONO_CARD_TAG: &str =
    "looklab-card/1 fw=acme-sim/2.0.1 look=acros@1.0.0 cid=0123456789abcdef";

fn card_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> =
        LazyLock::new(|| build_fixture_in("verify-payload-card", Some(CARD_TAG)));
    &FIXTURE
}

fn mono_card_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> =
        LazyLock::new(|| build_fixture_in("verify-payload-card-mono", Some(MONO_CARD_TAG)));
    &FIXTURE
}

/// One full-frame render and one centred 16:9 crop of the same RAW.
fn cropped_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
        build_fixture("verify-payload-cropped", None, |renders| {
            let decoded = decode_jpeg(&renders[0]).expect("decode render");
            let width = decoded.image.width;
            let height = decoded.image.height;
            let crop_height = width * 9 / 16;
            let y0 = (height - crop_height) / 2;
            let mut data = Vec::with_capacity((width * crop_height) as usize);
            for y in y0..y0 + crop_height {
                for x in 0..width {
                    data.push(decoded.image.data[(y * width + x) as usize]);
                }
            }
            let cropped = Image3::new(width, crop_height, data).expect("cropped image");
            renders[0] = splice_exif(&renders[0], &jpeg_from_image(&cropped, 0.9));
        })
    });
    &FIXTURE
}

/// Two colour renders of one RAW: identical EXIF apart from the film simulation.
fn simulation_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
        build_fixture("verify-payload-simulations", None, |renders| {
            renders[1] = with_fuji_tag(&renders[1], 0x1401, 0x0600);
        })
    });
    &FIXTURE
}

/// A fixture whose second render reports firmware 1.10 while the RAW reports 1.00.
fn render_firmware_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
        build_fixture("verify-payload-render-firmware", None, |renders| {
            renders[1] = with_software_firmware(&renders[1], "1.10");
        })
    });
    &FIXTURE
}

/// A fixture whose second render's makernotes select Acros.
fn monochrome_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
        build_fixture("verify-payload-monochrome", None, |renders| {
            renders[1] = with_fuji_tag(&renders[1], 0x1003, 0x0500);
        })
    });
    &FIXTURE
}

fn simulation_batches(fixture: &Fixture) -> (Site, Site) {
    (
        Site::from_fixture(fixture).only(&[0]),
        Site::from_fixture(fixture)
            .only(&[1])
            .with_sim("classic_chrome"),
    )
}

fn assert_upload_passes(fixture: &Fixture, site: &Site, name: &str) {
    let har = har_path(&format!("{name}-control"), &default_har(site).json());
    let state = state_path(&format!("{name}-control"));
    let (success, output) = run(fixture, &[har.as_path()], &state);
    assert!(success, "the control upload does not verify: {output}");
}

/// A fixture whose schema maps no makernote tag for `color_chrome_effect`.
fn declared_fixture() -> &'static Fixture {
    static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
        let fixture = build_fixture_in("verify-payload-declared", None);
        let schema_path = fixture.data.join("bodies/fujifilm-x-s10.yaml");
        let schema = std::fs::read_to_string(&schema_path).expect("schema");
        let unmapped = schema.replace(
            "        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32, strong: 64 } }\n",
            "",
        );
        assert_ne!(unmapped, schema, "the mapping line moved");
        std::fs::write(&schema_path, unmapped).expect("schema without the mapping");
        fixture
    });
    &FIXTURE
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn build_fixture_in(name: &str, render_tag: Option<&str>) -> Fixture {
    build_fixture(name, render_tag, |_| {})
}

fn build_fixture(
    name: &str,
    render_tag: Option<&str>,
    adjust: impl FnOnce(&mut Vec<Vec<u8>>),
) -> Fixture {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let workspace = workspace_root();
    let fixture = workspace.join("fixtures/cache/fujifilm-x-s10.raf");
    let raw = std::fs::read(&fixture).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            fixture.display()
        )
    });
    let raw_path = dir.join("fujifilm-x-s10.raf");
    std::fs::write(&raw_path, &raw).expect("RAW copy");

    let embedded = raf_jpeg(&raw);
    let decoded = decode_jpeg(&embedded).expect("the embedded JPEG decodes");
    let untagged = vec![
        embedded.clone(),
        splice_exif(&embedded, &jpeg_from_image(&decoded.image, 0.7)),
    ];
    let mut renders: Vec<Vec<u8>> = match render_tag {
        Some(token) => untagged
            .iter()
            .map(|jpeg| with_render_tag(jpeg, token))
            .collect(),
        None => untagged,
    };
    adjust(&mut renders);
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

    std::fs::create_dir_all(data.join("manuals")).expect("manuals directory");
    let manual = std::fs::read_to_string(workspace.join("data/manuals/fujifilm-x-t5.yaml"))
        .expect("manual catalog")
        .replace("id: fujifilm/x-t5", "id: fujifilm/x-s10")
        .replace("model: X-T5", "model: X-S10");
    std::fs::write(data.join("manuals/fujifilm-x-s10.yaml"), manual).expect("manual copy");

    let seeds = dir.join("seeds.json");
    let seed_text = format!(
        "{{\"format\":\"looklab-seeds/1\",\"prior_transform_version\":{PRIOR_VERSION},\"entries\":[{{\"key\":\"fujifilm-x-s10.raf\",\"seed\":\"{}\"}}]}}",
        hex(&SEED)
    );
    std::fs::write(&seeds, seed_text).expect("seed file");

    let module = module();
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

fn module() -> PathBuf {
    static MODULE: LazyLock<PathBuf> = LazyLock::new(verified_module);
    MODULE.clone()
}

fn verified_module() -> PathBuf {
    const SCRIPT: &str = "site/scripts/wasm-assets.sh";
    let root = workspace_root();
    let output = ProcessCommand::new("sh")
        .current_dir(&root)
        .args([SCRIPT, "--prep"])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "run `nix develop -c sh {SCRIPT} --prep` in {}: {error}",
                root.display()
            )
        });
    assert!(
        output.status.success(),
        "the prep module could not be built; run `nix develop -c sh {SCRIPT} --prep` in {}:\n{}{}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let wasm = root.join("site/generated/wasm/looklab_prep_module.wasm");
    assert!(
        wasm.is_file(),
        "{} is missing; run `nix develop -c sh {SCRIPT} --prep`",
        wasm.display()
    );
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

fn with_render_tag(jpeg: &[u8], token: &str) -> Vec<u8> {
    let segment = exif_segment(jpeg).expect("EXIF segment");
    let start = segment.as_ptr() as usize - jpeg.as_ptr() as usize;
    let tiff = &segment[10..];
    let little = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        other => panic!("unknown TIFF byte order {other:?}"),
    };
    let read_u16 = |at: usize| {
        let bytes: [u8; 2] = tiff[at..at + 2].try_into().expect("two bytes");
        if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    };
    let read_u32 = |at: usize| {
        let bytes: [u8; 4] = tiff[at..at + 4].try_into().expect("four bytes");
        if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    };

    let mut payload = b"ASCII\0\0\0".to_vec();
    payload.extend_from_slice(token.as_bytes());

    let ifd0 = read_u32(4) as usize;
    let count0 = read_u16(ifd0) as usize;
    let mut exif_ifd = None;
    for index in 0..count0 {
        let at = ifd0 + 2 + index * 12;
        if read_u16(at) == 0x8769 {
            exif_ifd = Some(read_u32(at + 8) as usize);
        }
    }
    let ifd = exif_ifd.unwrap_or(ifd0);
    let count = read_u16(ifd) as usize;
    let mut chosen = None;
    for index in 0..count {
        let at = ifd + 2 + index * 12;
        let offset = read_u32(at + 8) as usize;
        if read_u16(at) == 0x9286
            && read_u16(at + 2) == 7
            && read_u32(at + 4) as usize >= payload.len()
            && offset + payload.len() <= tiff.len()
        {
            chosen = Some((at, offset));
            break;
        }
    }
    let (entry, offset) = chosen.expect("a UserComment entry with room for the tag");

    let mut tagged = jpeg.to_vec();
    let base = start + 10;
    let put_u16 = |out: &mut [u8], at: usize, value: u16| {
        let bytes = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        out[at..at + 2].copy_from_slice(&bytes);
    };
    put_u16(&mut tagged, base + entry, 0x9286);
    put_u16(&mut tagged, base + entry + 2, 7);
    let length = if little {
        (payload.len() as u32).to_le_bytes()
    } else {
        (payload.len() as u32).to_be_bytes()
    };
    tagged[base + entry + 4..base + entry + 8].copy_from_slice(&length);
    tagged[base + offset..base + offset + payload.len()].copy_from_slice(&payload);
    tagged
}

/// Sets a Fujifilm makernote `u16` tag in a JPEG's EXIF.
fn with_fuji_tag(jpeg: &[u8], tag: u16, value: u16) -> Vec<u8> {
    let segment = exif_segment(jpeg).expect("EXIF segment");
    let start = segment.as_ptr() as usize - jpeg.as_ptr() as usize;
    let tiff = &segment[10..];
    let little = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        other => panic!("unknown TIFF byte order {other:?}"),
    };
    let read_u16 = |at: usize| {
        let bytes: [u8; 2] = tiff[at..at + 2].try_into().expect("two bytes");
        if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    };
    let read_u32 = |at: usize| {
        let bytes: [u8; 4] = tiff[at..at + 4].try_into().expect("four bytes");
        if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    };

    let ifd0 = read_u32(4) as usize;
    let count0 = read_u16(ifd0) as usize;
    let mut directories = vec![ifd0];
    for index in 0..count0 {
        let at = ifd0 + 2 + index * 12;
        if read_u16(at) == 0x8769 {
            directories.push(read_u32(at + 8) as usize);
        }
    }
    for directory in directories {
        let count = read_u16(directory) as usize;
        for index in 0..count {
            let at = directory + 2 + index * 12;
            if read_u16(at) != 0x927c || read_u16(at + 2) != 7 {
                continue;
            }
            let length = read_u32(at + 4) as usize;
            let offset = read_u32(at + 8) as usize;
            let note = &tiff[offset..offset + length];
            assert!(note.starts_with(b"FUJIFILM"), "the MakerNote is Fujifilm's");
            let start_of = u32::from_le_bytes(note[8..12].try_into().expect("four bytes"));
            let start_of = start_of as usize;
            let entries = u16::from_le_bytes(note[start_of..start_of + 2].try_into().expect("two"));
            for entry in 0..usize::from(entries) {
                let at = start_of + 2 + entry * 12;
                if u16::from_le_bytes(note[at..at + 2].try_into().expect("two bytes")) != tag {
                    continue;
                }
                assert_eq!(
                    u16::from_le_bytes(note[at + 2..at + 4].try_into().expect("two bytes")),
                    3,
                    "a u16 tag"
                );
                assert_eq!(
                    u32::from_le_bytes(note[at + 4..at + 8].try_into().expect("four bytes")),
                    1,
                    "a single value"
                );
                let mut patched = jpeg.to_vec();
                let base = start + 10 + offset;
                patched[base + at + 8..base + at + 10].copy_from_slice(&value.to_le_bytes());
                return patched;
            }
            panic!("the Fujifilm makernote has no tag {tag:#06x}");
        }
    }
    panic!("EXIF has no MakerNote");
}

/// Rewrites the `Ver1.00` token of the fixture JPEG's EXIF `Software` tag.
fn with_software_firmware(jpeg: &[u8], firmware: &str) -> Vec<u8> {
    let segment = exif_segment(jpeg).expect("EXIF segment");
    let start = segment.as_ptr() as usize - jpeg.as_ptr() as usize;
    let token = b"Ver1.00";
    let at = segment
        .windows(token.len())
        .position(|window| window == token)
        .expect("the Software tag's Ver token");
    let replacement = format!("Ver{firmware}");
    assert_eq!(replacement.len(), token.len(), "the token keeps its length");
    let mut patched = jpeg.to_vec();
    patched[start + at..start + at + token.len()].copy_from_slice(replacement.as_bytes());
    patched
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
        "grain_size",
        "clarity",
        "noise_reduction",
    ]
    .into_iter()
    .map(|key| {
        ControlEntry::reproduced(
            ControlKey::new(key).expect("control key"),
            StepIndex::new(0),
        )
    })
    .collect();
    ControlSettings::new(entries).expect("unique settings")
}

/// The settings the schema derives for one prepared render, with applicable values.
fn derived_control_settings(fixture: &Fixture, index: usize) -> ControlSettings {
    derived_settings(fixture, index, None, None)
}

fn derived_card_settings(fixture: &Fixture, index: usize, family: &str) -> ControlSettings {
    derived_settings(fixture, index, Some(family), Some(true))
}

fn derived_settings(
    fixture: &Fixture,
    index: usize,
    family_override: Option<&str>,
    monochrome_override: Option<bool>,
) -> ControlSettings {
    let yaml = std::fs::read_to_string(fixture.data.join("bodies/fujifilm-x-s10.yaml"))
        .expect("control schema");
    let schema = looklab_schema::validate_control_schema(&yaml).expect("schema validates");
    let metadata = &fixture.images[index].metadata;
    let firmware = metadata.firmware.as_deref().expect("firmware");
    let epoch = schema.epoch_for(firmware).expect("epoch");
    let values = looklab_schema::MakernoteValues::from(metadata);
    let classification =
        looklab_schema::classify_render(epoch, &values).expect("the render classifies");
    let family = match family_override {
        Some(family) => looklab_schema::LookId::new(family).expect("family"),
        None => classification.family.clone(),
    };
    let monochrome = monochrome_override
        .unwrap_or(classification.render_class == looklab_schema::RenderClass::Monochrome);
    let reach = looklab_schema::applicable_controls(epoch, &family);
    let (derived, _) =
        looklab_schema::settings_from_makernote_for(&schema, epoch.id, &family, &values);
    let mut entries: Vec<ControlEntry> = derived
        .entries()
        .iter()
        .map(|setting| ControlEntry::reproduced(setting.key.clone(), StepIndex::new(setting.step)))
        .collect();
    let required = reach
        .mapped
        .iter()
        .chain(&reach.unmapped)
        .chain(monochrome.then_some(&reach.spatial).into_iter().flatten());
    for key in required {
        if !entries.iter().any(|entry| &entry.key == key) {
            entries.push(ControlEntry::declared(key.clone(), StepIndex::new(0)));
        }
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    ControlSettings::new(entries).expect("unique settings")
}

/// The verifier arguments a monochrome batch needs: its authorization and declarations.
fn monochrome_extra(fixture: &Fixture) -> Vec<String> {
    let settings = derived_control_settings(fixture, 1);
    let mut args = vec![
        "--monochrome-authorization".to_owned(),
        "model-development-180-days".to_owned(),
    ];
    for entry in settings
        .entries()
        .iter()
        .filter(|entry| entry.source == SettingSource::Declared)
    {
        args.push("--declare-setting".to_owned());
        args.push(format!("{}={}", entry.key, entry.step.get()));
    }
    args
}

fn card_extra(fixture: &Fixture, family: &str) -> Vec<String> {
    let settings = derived_card_settings(fixture, 0, family);
    let mut args = vec![
        "--monochrome-authorization".to_owned(),
        "model-development-180-days".to_owned(),
    ];
    for entry in settings
        .entries()
        .iter()
        .filter(|entry| entry.source == SettingSource::Declared)
    {
        args.push("--declare-setting".to_owned());
        args.push(format!("{}={}", entry.key, entry.step.get()));
    }
    args
}

#[derive(Clone)]
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
    nonce: Nonce,
}

impl Site {
    fn new() -> Self {
        Self::from_fixture(fixture())
    }

    fn from_fixture(fixture: &Fixture) -> Self {
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
                render_class: RenderClass::Colour,
                monochrome_authorization: None,
                body: BodyId::new(BODY).expect("body id"),
                firmware_version: FirmwareVersionString::new(FIRMWARE).expect("firmware"),
                sim_key: SimKey::new(SIM).expect("sim key"),
                community_snapshot_version: CommunitySnapshotVersion::SHIPPED,
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
            nonce: Nonce::from_bytes([0x42; 32]),
        }
    }

    fn with_nonce(mut self, nonce: Nonce) -> Self {
        self.nonce = nonce;
        self
    }

    fn with_intent(mut self, intent: Intent) -> Self {
        self.manifest.intent = intent;
        self
    }

    fn only(mut self, indices: &[usize]) -> Self {
        let images = indices
            .iter()
            .map(|&index| self.manifest.images[index].clone())
            .collect();
        let blobs = indices
            .iter()
            .map(|&index| self.blobs[index].clone())
            .collect();
        self.manifest.images = images;
        self.blobs = blobs;
        self
    }

    fn with_sim(mut self, sim: &str) -> Self {
        self.manifest.sim_key = SimKey::new(sim).expect("sim key");
        self
    }

    fn with_settings(mut self, settings: ControlSettings) -> Self {
        self.manifest.settings = settings;
        self
    }

    fn with_monochrome(mut self, sim: &str) -> Self {
        self.manifest.render_class = RenderClass::Monochrome;
        self.manifest.monochrome_authorization =
            Some(MonochromeAuthorization::ModelDevelopment180Days);
        self.manifest.sim_key = SimKey::new(sim).expect("sim key");
        self
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

    fn with_declared_color_chrome_effect(mut self, step: i64) -> Self {
        let mut entries = self.manifest.settings.entries().to_vec();
        entries.retain(|entry| entry.key.as_str() != "color_chrome_effect");
        entries.push(ControlEntry::declared(
            ControlKey::new("color_chrome_effect").expect("control key"),
            StepIndex::new(step),
        ));
        self.manifest.settings = ControlSettings::new(entries).expect("unique settings");
        self
    }

    fn with_hidden_setting(mut self) -> Self {
        let mut entries = self.manifest.settings.entries().to_vec();
        entries.push(ControlEntry::reproduced(
            ControlKey::new("hidden").expect("control key"),
            StepIndex::new(12_345),
        ));
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
        let nonce = self.nonce;
        let send =
            SendManifest::canonical(vec![manifest_hash(&self.manifest)]).expect("send manifest");
        let challenge = self
            .challenge
            .unwrap_or_else(|| Challenge::for_send(&nonce, &send_manifest_hash(&send)));
        let assertion = assertion(
            &challenge,
            &self.origin,
            &self.rp_id,
            &self.credential_id,
            self.client_extra.as_ref(),
            self.extensions.as_deref(),
        );
        self.body_with(&nonce, &send, &assertion)
    }

    fn body_with(
        &self,
        nonce: &Nonce,
        send: &SendManifest,
        assertion: &WebAuthnAssertion,
    ) -> Vec<u8> {
        let challenge = Challenge::new(nonce, &manifest_hash(&self.manifest));
        let pow = solve(&challenge, self.manifest.images.len() as u32, &POW, 0)
            .expect("proof-of-work solves");
        let serial = self
            .serial
            .as_deref()
            .map(|ascii| RawSerial::from_envelope_ascii(ascii).expect("valid serial"));
        let parts = EnvelopeParts {
            nonce: *nonce,
            send_manifest: send,
            manifest: &self.manifest,
            assertion,
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
            br#"{"handle":"registration-handle","credential":{"id":"Y3JlZA","rawId":"Y3JlZA","type":"public-key","response":{"clientDataJSON":"e30","attestationObject":"oA"},"clientExtensionResults":{}}}"#,
        )
}

/// One send whose single assertion covers every site's batch.
fn send_har(sites: &[&Site]) -> HarBuilder {
    let uploaded = (0..sites.len()).collect::<Vec<_>>();
    send_har_of(sites, &uploaded)
}

/// The manifest covers every site; only `uploaded` batches are in the HAR.
fn send_har_of(sites: &[&Site], uploaded: &[usize]) -> HarBuilder {
    let nonce = Nonce::from_bytes([0x42; 32]);
    let hashes = sites
        .iter()
        .map(|site| manifest_hash(&site.manifest))
        .collect::<Vec<_>>();
    let send = SendManifest::canonical(hashes).expect("send manifest");
    let first = sites[0];
    let challenge = first
        .challenge
        .unwrap_or_else(|| Challenge::for_send(&nonce, &send_manifest_hash(&send)));
    let assertion = assertion(
        &challenge,
        &first.origin,
        &first.rp_id,
        &first.credential_id,
        first.client_extra.as_ref(),
        first.extensions.as_deref(),
    );
    let mut har = HarBuilder::new().post("https://looklab.test/api/nonce");
    for &index in uploaded {
        har = har.upload(sites[index].body_with(&nonce, &send, &assertion));
    }
    har.get("https://looklab.test/status/abc123")
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

    fn confirmation(mut self, body: &[u8]) -> Self {
        let mut value = entry(
            "POST",
            "https://looklab.test/api/manual-confirmations",
            Some(body),
        );
        value["request"]["postData"]["mimeType"] = json!("application/json");
        self.entries.push(value);
        self
    }

    fn withdrawal(mut self, body: &[u8]) -> Self {
        let mut value = entry(
            "POST",
            "https://looklab.test/api/manual-confirmations/withdraw",
            Some(body),
        );
        value["request"]["postData"]["mimeType"] = json!("application/json");
        self.entries.push(value);
        self
    }

    fn manual_summary(mut self, camera: &str) -> Self {
        self.entries.push(entry(
            "GET",
            &format!("https://looklab.test/api/manual-confirmations/{camera}"),
            None,
        ));
        self
    }

    fn get_body(mut self, url: &str, body: &[u8]) -> Self {
        self.entries.push(entry("GET", url, Some(body)));
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
    run_with(fixture, hars, state, &["--intent", "native"])
}

fn run_with(
    fixture: &Fixture,
    hars: &[&Path],
    state: &Path,
    intent_args: &[&str],
) -> (bool, String) {
    run_extra(fixture, hars, state, intent_args, &[])
}

fn run_extra(
    fixture: &Fixture,
    hars: &[&Path],
    state: &Path,
    intent_args: &[&str],
    extra: &[&str],
) -> (bool, String) {
    let files = std::iter::once(fixture.raw.as_path())
        .chain(fixture.jpegs.iter().map(PathBuf::as_path))
        .collect::<Vec<_>>();
    run_files(fixture, hars, state, intent_args, extra, &files)
}

fn run_files(
    fixture: &Fixture,
    hars: &[&Path],
    state: &Path,
    intent_args: &[&str],
    extra: &[&str],
    files: &[&Path],
) -> (bool, String) {
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
        .args(["--kind", "library"])
        .args(intent_args)
        .args(extra)
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
        .args(files);
    let output = command.output().expect("command runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

fn run_minimal(fixture: &Fixture, har: &Path, state: &Path) -> (bool, String) {
    run_minimal_with_data(fixture, har, state, &fixture.data)
}

fn run_minimal_with_data(
    fixture: &Fixture,
    har: &Path,
    state: &Path,
    data: &Path,
) -> (bool, String) {
    let mut command = Command::cargo_bin("looklab").expect("built binary");
    command
        .args(["verify-payload", "--module"])
        .arg(&fixture.module)
        .args(["--origin", ORIGIN, "--rp-id", RP_ID, "--data"])
        .arg(data)
        .args(["--kind", "library", "--intent", "native"])
        .arg("--har")
        .arg(har)
        .arg("--state")
        .arg(state);
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

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("target directory");
    for entry in std::fs::read_dir(source).expect("source directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &target.join(entry.file_name()));
        } else {
            std::fs::copy(&path, target.join(entry.file_name())).expect("file copy");
        }
    }
}

fn batch_manifest_start(body: &[u8]) -> usize {
    let send_len =
        u32::from_le_bytes(body[40..44].try_into().expect("send manifest length")) as usize;
    44 + send_len + 4
}

fn splice_manifest(body: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut out = body.to_vec();
    let start = batch_manifest_start(&out);
    let len = u32::from_le_bytes(out[start - 4..start].try_into().expect("manifest length"));
    let len = len as usize;
    out.splice(start + len..start + len, extra.iter().copied());
    out[start - 4..start].copy_from_slice(&((len + extra.len()) as u32).to_le_bytes());
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

fn session_body(site: &Site, digest: [u8; 32], extra: Option<(&str, Value)>) -> Vec<u8> {
    let nonce = Nonce::from_bytes([0x24; 32]);
    let challenge = Challenge::new(&nonce, &ManifestHash::from_bytes(digest));
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut value = json!({
        "nonce": base64.encode(nonce.as_bytes()),
        "credential_id": base64.encode(&assertion.credential_id),
        "authenticator_data": base64.encode(&assertion.authenticator_data),
        "client_data_json": base64.encode(&assertion.client_data_json),
        "signature": base64.encode(&assertion.signature),
        "pow": vec![0_u64; POW_K],
    });
    if let Some((key, extra)) = extra {
        value[key] = extra;
    }
    serde_json::to_vec(&value).expect("session body JSON")
}

#[test]
fn reproduces_a_cropped_upload_and_passes() {
    let fixture = cropped_fixture();
    let site = Site::from_fixture(fixture);
    assert_upload_passes(fixture, &site, "cropped");
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
fn the_file_order_within_a_group_does_not_change_the_reproduction() {
    let fixture = fixture();
    let site = Site::new();
    let har = har_path("file-order", &default_har(&site).json());
    let state = state_path("file-order");
    let files = std::iter::once(fixture.raw.as_path())
        .chain(fixture.jpegs.iter().rev().map(PathBuf::as_path))
        .collect::<Vec<_>>();
    let (success, output) = run_files(
        fixture,
        &[har.as_path()],
        &state,
        &["--intent", "native"],
        &[],
        &files,
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn multiple_native_batches_from_one_raw_verify() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let har = har_path("simulations", &send_har(&[&provia, &chrome]).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("simulations"));

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn a_mixed_colour_and_monochrome_group_verifies() {
    let fixture = monochrome_fixture();
    let colour = Site::from_fixture(fixture).only(&[0]);
    let monochrome = Site::from_fixture(fixture)
        .only(&[1])
        .with_monochrome("acros")
        .with_settings(derived_control_settings(fixture, 1));
    let har = har_path("mixed", &send_har(&[&colour, &monochrome]).json());
    let extra = monochrome_extra(fixture);
    let extra_refs = extra.iter().map(String::as_str).collect::<Vec<_>>();
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("mixed"),
        &["--intent", "native"],
        &extra_refs,
    );

    assert!(success, "{output}");
}

#[test]
fn a_monochrome_batch_without_authorization_fails() {
    let fixture = monochrome_fixture();
    let colour = Site::from_fixture(fixture).only(&[0]);
    let monochrome = Site::from_fixture(fixture)
        .only(&[1])
        .with_monochrome("acros")
        .with_settings(derived_control_settings(fixture, 1));
    let har = har_path(
        "mixed-unconsented",
        &send_har(&[&colour, &monochrome]).json(),
    );
    let extra = monochrome_extra(fixture);
    let declarations = extra[2..].iter().map(String::as_str).collect::<Vec<_>>();
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("mixed-unconsented"),
        &["--intent", "native"],
        &declarations,
    );

    assert!(!success, "{output}");
}

#[test]
fn a_colour_label_on_a_bw_batch_fails() {
    let fixture = monochrome_fixture();
    let colour = Site::from_fixture(fixture).only(&[0]);
    let mislabelled = Site::from_fixture(fixture)
        .only(&[1])
        .with_sim("acros")
        .with_settings(derived_control_settings(fixture, 1));
    let har = har_path("mislabelled-bw", &send_har(&[&colour, &mislabelled]).json());
    let extra = monochrome_extra(fixture);
    let extra_refs = extra.iter().map(String::as_str).collect::<Vec<_>>();
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("mislabelled-bw"),
        &["--intent", "native"],
        &extra_refs,
    );

    assert!(!success, "{output}");
}

#[test]
fn one_send_split_across_hars_verifies() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let first = har_path("split-a", &send_har_of(&[&provia, &chrome], &[0]).json());
    let second = har_path("split-b", &send_har_of(&[&provia, &chrome], &[1]).json());
    let (success, output) = run(
        fixture,
        &[first.as_path(), second.as_path()],
        &state_path("split-across-hars"),
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn conflicting_send_manifests_across_hars_fail() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let first = har_path("conflict-a", &default_har(&provia).json());
    let second = har_path("conflict-b", &default_har(&chrome).json());
    let (success, output) = run(
        fixture,
        &[first.as_path(), second.as_path()],
        &state_path("conflicting-sends"),
    );

    assert!(!success, "{output}");
}

#[test]
fn each_render_verifies_its_own_firmware() {
    let fixture = render_firmware_fixture();
    let raw_version = Site::from_fixture(fixture).only(&[0]);
    let render_version = Site::from_fixture(fixture).only(&[1]).with_firmware("1.10");
    let har = har_path(
        "render-firmware",
        &send_har(&[&raw_version, &render_version]).json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("render-firmware"));

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn a_matching_declare_setting_passes() {
    let fixture = declared_fixture();
    let site = Site::from_fixture(fixture).with_declared_color_chrome_effect(2);
    let har = har_path("declared-pass", &default_har(&site).json());
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("declared-pass"),
        &["--intent", "native"],
        &["--declare-setting", "color_chrome_effect=2"],
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn a_missing_declare_setting_fails() {
    let fixture = declared_fixture();
    let site = Site::from_fixture(fixture).with_declared_color_chrome_effect(2);
    let control = har_path("declared-missing-control", &default_har(&site).json());
    let (success, output) = run_extra(
        fixture,
        &[control.as_path()],
        &state_path("declared-missing-control"),
        &["--intent", "native"],
        &["--declare-setting", "color_chrome_effect=2"],
    );
    assert!(success, "the control upload does not verify: {output}");

    let har = har_path("declared-missing", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("declared-missing"));

    assert!(!success, "{output}");
}

#[test]
fn a_different_declare_setting_fails() {
    let fixture = declared_fixture();
    let site = Site::from_fixture(fixture).with_declared_color_chrome_effect(2);
    let control = har_path("declared-wrong-control", &default_har(&site).json());
    let (success, output) = run_extra(
        fixture,
        &[control.as_path()],
        &state_path("declared-wrong-control"),
        &["--intent", "native"],
        &["--declare-setting", "color_chrome_effect=2"],
    );
    assert!(success, "the control upload does not verify: {output}");

    let har = har_path("declared-wrong", &default_har(&site).json());
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("declared-wrong"),
        &["--intent", "native"],
        &["--declare-setting", "color_chrome_effect=1"],
    );

    assert!(!success, "{output}");
}

#[test]
fn fails_on_a_fabricated_batch_field() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "firmware");

    let site = Site::new().with_firmware("1.01");
    let har = har_path("firmware", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("firmware"));

    assert!(!success, "{output}");
}

#[test]
fn reproduces_a_tagged_card_upload_and_passes() {
    let fixture = card_fixture();
    assert!(
        fixture
            .images
            .iter()
            .all(|image| image.metadata.render_tag.is_some()),
        "the fixture JPEGs carry render tags"
    );
    let tag = parse_render_tag(CARD_TAG).expect("render tag");
    let site = Site::from_fixture(fixture).with_intent(Intent::Card(CardDeclaration::tagged(tag)));
    let har = har_path("card-tagged", &default_har(&site).json());
    let (success, output) = run_with(
        fixture,
        &[har.as_path()],
        &state_path("card-tagged"),
        &["--intent", "card"],
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn reproduces_a_monochrome_card_upload_and_passes() {
    let fixture = mono_card_fixture();
    let tag = parse_render_tag(MONO_CARD_TAG).expect("render tag");
    let site = Site::from_fixture(fixture)
        .with_intent(Intent::Card(CardDeclaration::tagged(tag)))
        .with_monochrome("provia")
        .with_settings(derived_card_settings(fixture, 0, "acros"));
    let har = har_path("card-mono", &default_har(&site).json());
    let extra = card_extra(fixture, "acros");
    let extra_refs = extra.iter().map(String::as_str).collect::<Vec<_>>();
    let (success, output) = run_extra(
        fixture,
        &[har.as_path()],
        &state_path("card-mono"),
        &["--intent", "card"],
        &extra_refs,
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn fails_on_a_fabricated_render_tag() {
    let fixture = card_fixture();
    let tag = parse_render_tag(CARD_TAG).expect("render tag");
    let control =
        Site::from_fixture(fixture).with_intent(Intent::Card(CardDeclaration::tagged(tag.clone())));
    let control_har = har_path("card-fabricated-control", &default_har(&control).json());
    let (success, output) = run_with(
        fixture,
        &[control_har.as_path()],
        &state_path("card-fabricated-control"),
        &["--intent", "card"],
    );
    assert!(success, "the control upload does not verify: {output}");

    let mut forged = tag;
    forged.cid_prefix[0] ^= 0xff;
    let site =
        Site::from_fixture(fixture).with_intent(Intent::Card(CardDeclaration::tagged(forged)));
    let har = har_path("card-fabricated", &default_har(&site).json());
    let (success, output) = run_with(
        fixture,
        &[har.as_path()],
        &state_path("card-fabricated"),
        &["--intent", "card"],
    );

    assert!(!success, "{output}");
}

#[test]
fn accepts_a_user_declared_card_with_matching_firmware() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture).with_intent(user_declared_card("acme-sim", "2.0.1"));
    let har = har_path("card-declared", &default_har(&site).json());
    let (success, output) = run_with(
        fixture,
        &[har.as_path()],
        &state_path("card-declared"),
        &["--intent", "card", "--firmware", "acme-sim/2.0.1"],
    );

    assert!(success, "{output}");
    assert!(output.contains("PASS"), "{output}");
}

#[test]
fn fails_on_a_user_declared_card_firmware_mismatch() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture).with_intent(user_declared_card("acme-sim", "2.0.1"));
    let control = har_path("card-mismatch-control", &default_har(&site).json());
    let (success, output) = run_with(
        fixture,
        &[control.as_path()],
        &state_path("card-mismatch-control"),
        &["--intent", "card", "--firmware", "acme-sim/2.0.1"],
    );
    assert!(success, "the control upload does not verify: {output}");

    let har = har_path("card-mismatch", &default_har(&site).json());
    let (success, output) = run_with(
        fixture,
        &[har.as_path()],
        &state_path("card-mismatch"),
        &["--intent", "card", "--firmware", "acme-sim/9.9.9"],
    );

    assert!(!success, "{output}");
}

fn user_declared_card(firmware: &str, version: &str) -> Intent {
    Intent::Card(CardDeclaration::user_declared(
        FirmwareId::new(firmware).expect("firmware id"),
        FirmwareVersion::new(version).expect("firmware version"),
    ))
}

#[test]
fn fails_on_a_setting_outside_the_schema() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "hidden-setting");

    let site = Site::new().with_hidden_setting();
    let har = har_path("hidden-setting", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("hidden-setting"));

    assert!(!success, "{output}");
}

#[test]
fn fails_on_signed_zero_manifest_bytes() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "signed-zero");

    let site = Site::new().with_positive_zero_tone();
    let har = har_path("signed-zero", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("signed-zero"));

    assert!(!success, "{output}");
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
    assert_upload_passes(fixture, &Site::new(), "reordered");

    let site = Site::new().reversed();
    let har = har_path("reordered", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("reordered"));

    assert!(!success, "{output}");
}

#[test]
fn fails_on_altered_samples_with_a_matching_manifest_hash() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "altered-samples");

    let mut site = Site::new();
    let samples = looklab_samples::decode(&site.blobs[0]).expect("the samples decode");
    let mut out_rgb = samples.out_rgb().to_vec();
    out_rgb[0][0] = if out_rgb[0][0] == 0.0 {
        0.5
    } else {
        out_rgb[0][0] / 2.0
    };
    let altered = looklab_samples::SampleSet::new(
        samples.header().clone(),
        samples.in_rgb().to_vec(),
        out_rgb,
        samples.density_w().to_vec(),
        samples.flags().to_vec(),
    )
    .expect("the altered samples validate");
    site.blobs[0] = looklab_samples::encode(&altered).expect("the altered samples encode");
    site.manifest.images[0].blob_sha256 = sha256(&site.blobs[0]);
    let har = har_path("altered-samples", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("altered-samples"));

    assert!(!success, "{output}");
}

#[test]
fn fails_on_a_batch_declaring_another_render_of_the_same_raw() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let control = har_path("mislabelled-control", &send_har(&[&provia, &chrome]).json());
    let (success, output) = run(
        fixture,
        &[control.as_path()],
        &state_path("mislabelled-control"),
    );
    assert!(success, "the control upload does not verify: {output}");

    let mislabelled = Site::from_fixture(fixture)
        .only(&[0])
        .with_sim("classic_chrome");
    let har = har_path("mislabelled", &send_har(&[&provia, &mislabelled]).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("mislabelled"));

    assert!(!success, "{output}");
}

#[test]
fn fails_when_the_send_manifest_lists_a_batch_the_har_lacks() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let har = har_path(
        "missing-batch",
        &send_har_of(&[&provia, &chrome], &[0]).json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("missing-batch"));

    assert!(!success, "{output}");
    assert!(output.contains("missing from the HAR"), "{output}");
}

#[test]
fn fails_when_a_batch_is_uploaded_twice() {
    let fixture = fixture();
    let site = Site::new();
    let body = site.body();
    let har = har_path(
        "extra-batch",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(body.clone())
            .upload(body)
            .json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("extra-batch"));

    assert!(!success, "{output}");
    assert!(output.contains("more than once"), "{output}");
}

#[test]
fn fails_when_a_batch_is_not_in_the_send_manifest() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let mut bytes = provia.body();
    let wrong = SendManifest::canonical(vec![manifest_hash(&chrome.manifest)]).expect("send");
    let send_len =
        u32::from_le_bytes(bytes[40..44].try_into().expect("send manifest length")) as usize;
    let replacement = looklab_upload::canonical_send_manifest_bytes(&wrong);
    assert_eq!(replacement.len(), send_len, "one-batch send manifests");
    bytes[44..44 + send_len].copy_from_slice(&replacement);
    let har = har_path(
        "not-in-manifest",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(bytes)
            .json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("not-in-manifest"));

    assert!(!success, "{output}");
    assert!(
        output.contains("not listed in the send manifest"),
        "{output}"
    );
}

#[test]
fn fails_when_a_batch_is_signed_by_another_credential() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let nonce = Nonce::from_bytes([0x42; 32]);
    let send = SendManifest::canonical(vec![
        manifest_hash(&provia.manifest),
        manifest_hash(&chrome.manifest),
    ])
    .expect("send");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let first = assertion(
        &challenge,
        &provia.origin,
        &provia.rp_id,
        &provia.credential_id,
        None,
        None,
    );
    let other = assertion(
        &challenge,
        &provia.origin,
        &provia.rp_id,
        b"credential-beta",
        None,
        None,
    );
    let har = har_path(
        "another-credential",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(provia.body_with(&nonce, &send, &first))
            .upload(chrome.body_with(&nonce, &send, &other))
            .json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("another-credential"));

    assert!(!success, "{output}");
    assert!(output.contains("different assertions"), "{output}");
}

#[test]
fn fails_when_a_send_mixes_nonces() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let first_nonce = Nonce::from_bytes([0x42; 32]);
    let second_nonce = Nonce::from_bytes([0x43; 32]);
    let send = SendManifest::canonical(vec![
        manifest_hash(&provia.manifest),
        manifest_hash(&chrome.manifest),
    ])
    .expect("send");
    let first_challenge = Challenge::for_send(&first_nonce, &send_manifest_hash(&send));
    let first_assertion = assertion(
        &first_challenge,
        &provia.origin,
        &provia.rp_id,
        &provia.credential_id,
        None,
        None,
    );
    let second_challenge = Challenge::for_send(&second_nonce, &send_manifest_hash(&send));
    let second_assertion = assertion(
        &second_challenge,
        &chrome.origin,
        &chrome.rp_id,
        &chrome.credential_id,
        None,
        None,
    );
    let har = har_path(
        "mixed-nonce",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(provia.body_with(&first_nonce, &send, &first_assertion))
            .upload(chrome.body_with(&second_nonce, &send, &second_assertion))
            .json(),
    );
    let (success, output) = run(fixture, &[har.as_path()], &state_path("mixed-nonce"));

    assert!(!success, "{output}");
    assert!(output.contains("missing from the HAR"), "{output}");
}

#[test]
fn fails_on_the_same_image_twice_in_one_upload() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "repeated-image");

    let mut site = Site::new();
    let image = site.manifest.images[0].clone();
    let blob = site.blobs[0].clone();
    site.manifest.images.push(image);
    site.blobs.push(blob);
    let har = har_path("repeated-image", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("repeated-image"));

    assert!(!success, "{output}");
}

#[test]
fn a_har_that_omits_a_prepared_image_fails() {
    let fixture = fixture();
    assert_upload_passes(fixture, &Site::new(), "omitted-image");

    let site = Site::new().only(&[0]);
    let har = har_path("omitted-image", &default_har(&site).json());
    let (success, output) = run(fixture, &[har.as_path()], &state_path("omitted-image"));

    assert!(!success, "{output}");
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
fn accepts_a_passkey_session_request() {
    let fixture = fixture();
    let site = Site::new();
    let digest = sha256(b"looklab-request-v1:session:");
    let body = session_body(&site, digest, None);
    let har = default_har(&site)
        .post_body("https://looklab.test/api/session", &body)
        .json();
    let har = har_path("session", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("session"));

    assert!(success, "{output}");
    assert!(
        output.contains("[expected] POST https://looklab.test/api/session"),
        "{output}"
    );
    assert!(
        output.contains("H(nonce ‖ H(looklab-request-v1:session:)) matches client_data_json"),
        "{output}"
    );
}

#[test]
fn fails_on_a_session_body_with_an_extra_field() {
    let fixture = fixture();
    let site = Site::new();
    let digest = sha256(b"looklab-request-v1:session:");
    let body = session_body(&site, digest, Some(("hidden", json!(1))));
    let har = default_har(&site)
        .post_body("https://looklab.test/api/session", &body)
        .json();
    let har = har_path("session-extra", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("session-extra"));

    assert!(!success, "{output}");
    assert!(output.contains("unaccounted key \"hidden\""), "{output}");
}

#[test]
fn fails_on_a_session_challenge_over_another_payload() {
    let fixture = fixture();
    let site = Site::new();
    let digest = sha256(b"looklab-request-v1:mod-session:");
    let body = session_body(&site, digest, None);
    let har = default_har(&site)
        .post_body("https://looklab.test/api/session", &body)
        .json();
    let har = har_path("session-digest", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("session-digest"));

    assert!(!success, "{output}");
    assert!(output.contains("challenge"), "{output}");
}

#[test]
fn session_restoration_requires_the_upload_capability_and_no_body_or_query() {
    let fixture = fixture();
    let site = Site::new();
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0x31; 32]);
    let other = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0x32; 32]);
    for (name, authorization, url, body, expected) in [
        (
            "valid",
            format!("Bearer {token}"),
            "https://looklab.test/api/session",
            None,
            None,
        ),
        (
            "mismatch",
            format!("Bearer {other}"),
            "https://looklab.test/api/session",
            None,
            Some("differs from the upload response"),
        ),
        (
            "invalid",
            "Bearer payload".to_owned(),
            "https://looklab.test/api/session",
            None,
            Some("32-byte base64url status token"),
        ),
        (
            "query",
            format!("Bearer {token}"),
            "https://looklab.test/api/session?payload=secret",
            None,
            Some("unexpected request"),
        ),
        (
            "body",
            format!("Bearer {token}"),
            "https://looklab.test/api/session",
            Some(b"payload".as_slice()),
            Some("carries a body"),
        ),
    ] {
        let mut har = default_har(&site);
        let upload = har
            .entries
            .iter_mut()
            .find(|entry| entry["request"]["url"] == "https://looklab.test/api/upload")
            .expect("upload request");
        upload["response"] =
            json!({"content": {"text": json!({"status_token": token}).to_string()}});
        har.entries.push(entry("GET", url, body));
        let har = har.header("Authorization", &authorization);
        let har = har_path(&format!("restore-{name}"), &har.json());
        let (success, output) = run(
            fixture,
            &[har.as_path()],
            &state_path(&format!("restore-{name}")),
        );
        assert_eq!(success, expected.is_none(), "{name}: {output}");
        if let Some(finding) = expected {
            assert!(output.contains(finding), "{name}: {output}");
        }
    }
}

#[test]
fn fails_on_payload_bytes_in_a_session_body() {
    let fixture = fixture();
    let site = Site::new();
    let body = session_body(
        &site,
        sha256(b"looklab-request-v1:session:"),
        Some((
            "payload",
            json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(site.body())),
        )),
    );
    let har = default_har(&site)
        .post_body("https://looklab.test/api/session", &body)
        .json();
    let har = har_path("session-payload", &har);
    let (success, output) = run(fixture, &[har.as_path()], &state_path("session-payload"));
    assert!(!success, "{output}");
    assert!(output.contains("unaccounted key \"payload\""), "{output}");
}

#[test]
fn fails_on_payload_bytes_inside_registration_credentials() {
    let fixture = fixture();
    let site = Site::new();
    let body = serde_json::to_vec(&json!({
        "handle": "registration-handle",
        "credential": {
            "id": "Y3JlZA", "rawId": "Y3JlZA", "type": "public-key",
            "response": {"clientDataJSON": "e30", "attestationObject": "oA", "payload": [1, 2, 3]},
            "clientExtensionResults": {}
        }
    }))
    .expect("registration JSON");
    let har = default_har(&site)
        .post_body("https://looklab.test/api/passkey/register/finish", &body)
        .json();
    let har = har_path("registration-payload", &har);
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("registration-payload"),
    );
    assert!(!success, "{output}");
    assert!(
        output.contains("registration response carries an unaccounted key \"payload\""),
        "{output}"
    );
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
fn two_sends_with_one_credential_verify_and_a_changed_credential_fails() {
    let fixture = simulation_fixture();
    let (provia, chrome) = simulation_batches(fixture);
    let second_nonce = Nonce::from_bytes([0x43; 32]);
    let first = har_path("credential-a", &default_har(&provia).json());
    let second = har_path(
        "credential-b",
        &default_har(&chrome.clone().with_nonce(second_nonce)).json(),
    );
    let (success, output) = run(
        fixture,
        &[first.as_path(), second.as_path()],
        &state_path("credential-control"),
    );
    assert!(success, "{output}");

    let changed = har_path(
        "credential-changed",
        &default_har(
            &chrome
                .with_nonce(second_nonce)
                .with_credential(OTHER_CREDENTIAL),
        )
        .json(),
    );
    let (success, output) = run(
        fixture,
        &[first.as_path(), changed.as_path()],
        &state_path("credential-changed"),
    );
    assert!(!success, "{output}");
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
    let start = batch_manifest_start(&out);
    let manifest_len =
        u32::from_le_bytes(out[start - 4..start].try_into().expect("manifest length")) as usize;
    let mut index = start + manifest_len;
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

fn manual_camera(fixture: &Fixture) -> looklab_schema::ManualCamera {
    let source = std::fs::read_to_string(fixture.data.join("manuals/fujifilm-x-s10.yaml"))
        .expect("fixture manual catalog");
    let catalog = looklab_schema::validate_manual_catalog(&source).expect("valid manual catalog");
    catalog
        .camera(&BodyId::new(BODY).expect("body id"))
        .expect("documented fixture camera")
        .clone()
}

fn manual_confirmation(fixture: &Fixture) -> looklab_schema::ManualConfirmation {
    let camera = manual_camera(fixture);
    let key = looklab_schema::CapabilityKey::NativeSimulations;
    let capability = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("documented native simulations");
    looklab_schema::ManualConfirmation {
        version: looklab_schema::MANUAL_CONFIRMATION_VERSION,
        camera: camera.id.clone(),
        camera_digest: camera.definition_digest(),
        firmware: looklab_schema::ReportedFirmware::new(CONFIRMATION_FIRMWARE).expect("firmware"),
        scope: looklab_schema::ConfirmationScope::Controls { keys: vec![key] },
        observations: vec![looklab_schema::ControlObservation {
            key,
            definition_digest: capability.definition_digest(&camera),
            finding: looklab_schema::Finding::Matches,
        }],
    }
}

fn confirmation_send(
    site: &Site,
    confirmation: &looklab_schema::ManualConfirmation,
) -> (SendManifest, WebAuthnAssertion) {
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&site.manifest)],
        vec![looklab_upload::AttestationHash::from_bytes(
            *digest.as_bytes(),
        )],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&site.nonce, &send_manifest_hash(&send));
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    (send, assertion)
}

fn confirmation_body(
    nonce: &Nonce,
    send: &SendManifest,
    assertion: &WebAuthnAssertion,
    confirmation: &looklab_schema::ManualConfirmation,
) -> Vec<u8> {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    looklab_upload::ManualConfirmationRequest {
        nonce: engine.encode(nonce.as_bytes()),
        credential_id: engine.encode(&assertion.credential_id),
        authenticator_data: engine.encode(&assertion.authenticator_data),
        client_data_json: engine.encode(&assertion.client_data_json),
        signature: engine.encode(&assertion.signature),
        send_manifest: engine.encode(looklab_upload::canonical_send_manifest_bytes(send)),
        confirmation: confirmation.clone(),
    }
    .canonical_bytes()
}

fn attestation_only_confirmation(
    site: &Site,
    confirmation: &looklab_schema::ManualConfirmation,
) -> (SendManifest, WebAuthnAssertion) {
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![looklab_upload::AttestationHash::from_bytes(
            *confirmation.digest().as_bytes(),
        )],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&site.nonce, &send_manifest_hash(&send));
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    (send, assertion)
}

#[test]
fn manual_confirmation_request_is_bound_to_the_send() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let confirmation = manual_confirmation(fixture);
    let (send, assertion) = confirmation_send(&site, &confirmation);
    let body = confirmation_body(&site.nonce, &send, &assertion, &confirmation);

    let har = har_path(
        "manual-confirmation",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(site.body_with(&site.nonce, &send, &assertion))
            .confirmation(&body)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("manual-confirmation"),
    );
    assert!(success, "the bound confirmation does not verify: {output}");
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains(CONFIRMATION_FIRMWARE), "{output}");
    assert!(
        output.contains(&hex(confirmation.camera_digest.as_bytes())),
        "the camera definition digest is not printed: {output}"
    );
    assert!(
        output.contains(&hex(confirmation.observations[0]
            .definition_digest
            .as_bytes())),
        "the observation definition digest is not printed: {output}"
    );
    assert!(
        output.contains(&hex(confirmation.digest().as_bytes())),
        "the confirmation digest is not printed: {output}"
    );

    let mut altered: looklab_upload::ManualConfirmationRequest =
        serde_json::from_slice(&body).expect("confirmation body");
    altered.confirmation.firmware =
        looklab_schema::ReportedFirmware::new("9.99").expect("firmware");
    let har = har_path(
        "manual-confirmation-altered",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(site.body_with(&site.nonce, &send, &assertion))
            .confirmation(&altered.canonical_bytes())
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("manual-confirmation-altered"),
    );
    assert!(
        !success,
        "an altered confirmation must not verify: {output}"
    );

    let mut extra: Value = serde_json::from_slice(&body).expect("confirmation body");
    extra["unbound"] = json!(true);
    let har = har_path(
        "manual-confirmation-extra",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(site.body_with(&site.nonce, &send, &assertion))
            .confirmation(&serde_json::to_vec(&extra).expect("extra body"))
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("manual-confirmation-extra"),
    );
    assert!(!success, "unbound content must not verify: {output}");

    let other_challenge =
        Challenge::for_send(&Nonce::from_bytes([0x56; 32]), &send_manifest_hash(&send));
    let other = self::assertion(
        &other_challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let swapped = confirmation_body(&site.nonce, &send, &other, &confirmation);
    let har = har_path(
        "manual-confirmation-assertion",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(site.body_with(&site.nonce, &send, &assertion))
            .confirmation(&swapped)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("manual-confirmation-assertion"),
    );
    assert!(
        !success,
        "a confirmation with a different assertion must not verify: {output}"
    );

    let canonical: Value = serde_json::from_slice(&body).expect("confirmation body");
    let mut non_canonical = serde_json::to_string_pretty(&canonical).expect("pretty json");
    non_canonical = non_canonical.replace("\"fujifilm/x-s10\"", "\"fujifilm\\/x-s10\"");
    let har = har_path(
        "manual-confirmation-non-canonical",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .upload(site.body_with(&site.nonce, &send, &assertion))
            .confirmation(non_canonical.as_bytes())
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("manual-confirmation-non-canonical"),
    );
    assert!(
        !success,
        "non-canonical request bytes must not verify: {output}"
    );
}

#[test]
fn declared_confirmation_payload_is_printed_for_review() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let mut confirmation = manual_confirmation(fixture);
    let encoded_firmware = "cHJpdmF0ZS1ieXRlcy1pbi1maXJtd2FyZQ";
    let encoded_value = "cHJpdmF0ZS1ieXRlcy1pbi1hLWNvcnJlY3RlZC1kb21haW4";
    confirmation.firmware =
        looklab_schema::ReportedFirmware::new(encoded_firmware).expect("firmware");
    confirmation.observations[0].finding = looklab_schema::Finding::Corrected {
        availability: looklab_schema::Availability::Available,
        domain: looklab_schema::CapabilityDomain::Enum {
            values: vec![encoded_value.to_owned()],
            default: None,
        },
    };

    let (send, assertion) = attestation_only_confirmation(&site, &confirmation);
    let body = confirmation_body(&site.nonce, &send, &assertion, &confirmation);
    let har = har_path(
        "declared-payload",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .confirmation(&body)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run_minimal(fixture, har.as_path(), &state_path("declared-payload"));
    assert!(
        success,
        "the declared confirmation does not verify: {output}"
    );
    assert!(output.contains("REVIEW NEEDED"), "{output}");
    assert!(output.contains(encoded_firmware), "{output}");
    assert!(output.contains(encoded_value), "{output}");
}

#[test]
fn structurally_invalid_confirmations_fail() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let base = manual_confirmation(fixture);
    let key = looklab_schema::CapabilityKey::NativeSimulations;

    let mut unsupported = base.clone();
    unsupported.version += 1;

    let mut empty = base.clone();
    empty.observations.clear();
    empty.scope = looklab_schema::ConfirmationScope::Complete;

    let mut duplicate = base.clone();
    let repeated = duplicate.observations[0].clone();
    duplicate.observations.push(repeated);
    duplicate.scope = looklab_schema::ConfirmationScope::Controls {
        keys: vec![key, key],
    };

    let mut mismatched = base.clone();
    mismatched.scope = looklab_schema::ConfirmationScope::Controls {
        keys: vec![looklab_schema::CapabilityKey::WhiteBalancePresets],
    };

    let mut too_many = base.clone();
    let observation = too_many.observations[0].clone();
    too_many.observations = (0..=looklab_schema::MAX_CONFIRMATION_OBSERVATIONS)
        .map(|_| observation.clone())
        .collect();

    let mut oversized = base.clone();
    oversized.observations[0].finding = looklab_schema::Finding::Corrected {
        availability: looklab_schema::Availability::Available,
        domain: looklab_schema::CapabilityDomain::Enum {
            values: vec!["x".repeat(looklab_schema::MAX_CONFIRMATION_BYTES)],
            default: None,
        },
    };

    let cases = [
        ("unsupported-version", unsupported),
        ("empty-observations", empty),
        ("duplicate-observations", duplicate),
        ("scope-mismatch", mismatched),
        ("too-many-observations", too_many),
        ("oversized-confirmation", oversized),
    ];
    for (name, confirmation) in cases {
        let (send, assertion) = attestation_only_confirmation(&site, &confirmation);
        let body = confirmation_body(&site.nonce, &send, &assertion, &confirmation);
        let har = har_path(
            &format!("invalid-{name}"),
            &HarBuilder::new()
                .post("https://looklab.test/api/nonce")
                .confirmation(&body)
                .get("https://looklab.test/status/abc123")
                .json(),
        );
        let (success, output) = run_minimal(
            fixture,
            har.as_path(),
            &state_path(&format!("invalid-{name}")),
        );
        assert!(
            !success,
            "a structurally invalid confirmation ({name}) must not verify: {output}"
        );
    }
}

#[test]
fn attestation_only_har_verifies_without_a_photo_upload() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let confirmation = manual_confirmation(fixture);
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![looklab_upload::AttestationHash::from_bytes(
            *digest.as_bytes(),
        )],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&site.nonce, &send_manifest_hash(&send));
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let body = confirmation_body(&site.nonce, &send, &assertion, &confirmation);
    let har = har_path(
        "attestation-only",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .confirmation(&body)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run_minimal(fixture, har.as_path(), &state_path("attestation-only"));
    assert!(
        success,
        "an attestation-only confirmation does not verify: {output}"
    );
    assert!(output.contains("REVIEW NEEDED"), "{output}");

    let batch_declared = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&site.manifest)],
        vec![looklab_upload::AttestationHash::from_bytes(
            *digest.as_bytes(),
        )],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&site.nonce, &send_manifest_hash(&batch_declared));
    let assertion = self::assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let body = confirmation_body(&site.nonce, &batch_declared, &assertion, &confirmation);
    let har = har_path(
        "attestation-only-batch",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .confirmation(&body)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run_minimal(
        fixture,
        har.as_path(),
        &state_path("attestation-only-batch"),
    );
    assert!(
        !success,
        "a declaration without its upload must not verify: {output}"
    );
}

#[test]
fn supplied_photos_without_an_upload_are_rejected() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let confirmation = manual_confirmation(fixture);
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![looklab_upload::AttestationHash::from_bytes(
            *digest.as_bytes(),
        )],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&site.nonce, &send_manifest_hash(&send));
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let body = confirmation_body(&site.nonce, &send, &assertion, &confirmation);
    let har = har_path(
        "attestation-only-with-files",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .confirmation(&body)
            .get("https://looklab.test/status/abc123")
            .json(),
    );
    let (success, output) = run(
        fixture,
        &[har.as_path()],
        &state_path("attestation-only-with-files"),
    );
    assert!(
        !success,
        "supplied photos with no upload must not verify: {output}"
    );
}

fn withdrawal_body(site: &Site, nonce: &Nonce, camera: &str) -> Vec<u8> {
    let digest = looklab_server::request_digest(
        looklab_server::RequestDomain::WithdrawManualConfirmation,
        camera.as_bytes(),
    );
    let challenge = Challenge::new(nonce, &digest);
    let assertion = assertion(
        &challenge,
        &site.origin,
        &site.rp_id,
        &site.credential_id,
        None,
        None,
    );
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    looklab_upload::ManualWithdrawalRequest {
        nonce: engine.encode(nonce.as_bytes()),
        credential_id: engine.encode(&assertion.credential_id),
        authenticator_data: engine.encode(&assertion.authenticator_data),
        client_data_json: engine.encode(&assertion.client_data_json),
        signature: engine.encode(&assertion.signature),
        camera: BodyId::new(camera).expect("camera id"),
    }
    .canonical_bytes()
}

#[test]
fn withdrawal_har_is_recognized_and_bound_to_the_camera() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let nonce = Nonce::from_bytes([0x57; 32]);
    let camera = site.manifest.body.as_str();
    let body = withdrawal_body(&site, &nonce, camera);
    let har = har_path(
        "withdrawal",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .manual_summary(camera)
            .json(),
    );
    let (success, output) = run_minimal(fixture, har.as_path(), &state_path("withdrawal"));
    assert!(success, "a bound withdrawal does not verify: {output}");
    assert!(
        output.contains(camera),
        "the withdrawal target is not printed: {output}"
    );
    assert!(
        output.contains("camera summary target"),
        "the browser's summary refresh is not audited: {output}"
    );

    let unknown = "leak/00112233445566778899aabbccddeeff";
    let body = withdrawal_body(&site, &nonce, unknown);
    let har = har_path(
        "withdrawal-unknown-camera",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .json(),
    );
    let (success, output) = run_minimal(
        fixture,
        har.as_path(),
        &state_path("withdrawal-unknown-camera"),
    );
    assert!(
        !success,
        "a matched challenge for an undocumented camera must not verify: {output}"
    );

    let body = withdrawal_body(&site, &nonce, camera);
    let spaced = String::from_utf8(body.clone())
        .expect("utf-8 body")
        .replacen(":\"", ": \"", 1);
    let har = har_path(
        "withdrawal-spaced",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(spaced.as_bytes())
            .json(),
    );
    let (success, output) = run_minimal(fixture, har.as_path(), &state_path("withdrawal-spaced"));
    assert!(
        !success,
        "a whitespace-encoded withdrawal must not verify: {output}"
    );

    let escaped = String::from_utf8(body)
        .expect("utf-8 body")
        .replace(camera, &camera.replace('/', "\\/"));
    let har = har_path(
        "withdrawal-escaped",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(escaped.as_bytes())
            .json(),
    );
    let (success, output) = run_minimal(fixture, har.as_path(), &state_path("withdrawal-escaped"));
    assert!(!success, "an escaped withdrawal must not verify: {output}");

    let data = fixture.dir.join("data-without-manuals");
    let _ = std::fs::remove_dir_all(&data);
    copy_dir(&fixture.data.join("bodies"), &data.join("bodies"));
    let body = withdrawal_body(&site, &nonce, camera);
    let har = har_path(
        "withdrawal-no-catalog",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .json(),
    );
    let (success, output) = run_minimal_with_data(
        fixture,
        har.as_path(),
        &state_path("withdrawal-no-catalog"),
        &data,
    );
    assert!(
        !success,
        "a withdrawal without a local manual catalog must not verify: {output}"
    );
}

#[test]
fn the_camera_summary_request_is_checked() {
    let fixture = fixture();
    let site = Site::from_fixture(fixture);
    let nonce = Nonce::from_bytes([0x58; 32]);
    let camera = site.manifest.body.as_str();
    let body = withdrawal_body(&site, &nonce, camera);
    let url = format!("https://looklab.test/api/manual-confirmations/{camera}");

    let query = format!("{url}?after=1");
    let har = har_path(
        "withdrawal-summary-query",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .get(&query)
            .json(),
    );
    let (success, output) = run_minimal(
        fixture,
        har.as_path(),
        &state_path("withdrawal-summary-query"),
    );
    assert!(
        !success,
        "a camera summary with a query string must not verify: {output}"
    );

    let har = har_path(
        "withdrawal-summary-body",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .get_body(&url, b"{}")
            .json(),
    );
    let (success, output) = run_minimal(
        fixture,
        har.as_path(),
        &state_path("withdrawal-summary-body"),
    );
    assert!(
        !success,
        "a camera summary with a body must not verify: {output}"
    );

    let har = har_path(
        "withdrawal-summary-unknown-camera",
        &HarBuilder::new()
            .post("https://looklab.test/api/nonce")
            .withdrawal(&body)
            .manual_summary("leak/00112233445566778899aabbccddeeff")
            .json(),
    );
    let (success, output) = run_minimal(
        fixture,
        har.as_path(),
        &state_path("withdrawal-summary-unknown-camera"),
    );
    assert!(
        !success,
        "a camera summary for an undocumented camera must not verify: {output}"
    );
}
