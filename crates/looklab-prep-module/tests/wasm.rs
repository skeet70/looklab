// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::Command;

use jpeg_encoder::{ColorType, Encoder};
use looklab_color::RgbSpace;
use looklab_prep::{Image3, PrepParams};
use looklab_prep_module::{
    Function, ModuleError, PrepareRequest, PriorTransformSpec, Response, dispatch,
};
use looklab_raw::{DecodedRaw, Orientation, RawDecoder, RawlerDecoder};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

#[allow(clippy::disallowed_methods)]
fn fixture_raw() -> Vec<u8> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cache/fujifilm-x-s10.raf");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            path.display()
        )
    })
}

struct Guest {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    call: TypedFunc<(u32, u32, u32), u64>,
}

impl Guest {
    fn invoke(&mut self, fn_id: u32, input: &[u8]) -> Vec<u8> {
        let in_ptr = self
            .alloc
            .call(&mut self.store, input.len() as u32)
            .expect("allocation succeeds");
        self.memory
            .write(&mut self.store, in_ptr as usize, input)
            .expect("input is written");
        let packed = self
            .call
            .call(&mut self.store, (fn_id, in_ptr, input.len() as u32))
            .expect("call succeeds");
        self.free
            .call(&mut self.store, (in_ptr, input.len() as u32))
            .expect("input frees");

        let out_ptr = (packed >> 32) as u32;
        let out_len = (packed & u64::from(u32::MAX)) as u32;
        let mut output = vec![0u8; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut output)
            .expect("output is read");
        self.free
            .call(&mut self.store, (out_ptr, out_len))
            .expect("output frees");
        output
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("module crate is under crates/")
        .to_path_buf()
}

fn build_module() -> PathBuf {
    let root = workspace_root();
    let target_dir =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let target_dir = root.join(target_dir).join("prep-module-wasm");
    let status = Command::new(env!("CARGO"))
        .current_dir(&root)
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
    let status = Command::new("wasm-opt")
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

fn jpeg_from_fixture() -> Vec<u8> {
    let decoded = RawlerDecoder
        .decode(&fixture_raw())
        .expect("fixture decodes");
    let image = raw_image(&decoded);
    let codes: Vec<u8> = image
        .data
        .iter()
        .flat_map(|rgb| {
            rgb.map(|channel| {
                let linear = f64::from(channel.clamp(0.0, 1.0));
                (RgbSpace::Srgb.encode(linear) * 255.0).round() as u8
            })
        })
        .collect();
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, 95)
        .encode(
            &codes,
            image.width as u16,
            image.height as u16,
            ColorType::Rgb,
        )
        .expect("JPEG encodes");
    bytes
}

fn raw_image(decoded: &DecodedRaw) -> Image3 {
    let wb = decoded.as_shot_wb;
    let (pixels, _) = decoded.rgb.as_chunks::<3>();
    let data = pixels
        .iter()
        .map(|pixel| {
            [
                (f64::from(pixel[0]) * wb[0]) as f32,
                (f64::from(pixel[1]) * wb[1]) as f32,
                (f64::from(pixel[2]) * wb[2]) as f32,
            ]
        })
        .collect();
    Image3::new(decoded.width as u32, decoded.height as u32, data)
        .expect("dimensions match")
        .apply_orientation(meta_orientation(decoded.orientation))
}

fn meta_orientation(orientation: Orientation) -> looklab_meta::Orientation {
    match orientation {
        Orientation::Normal => looklab_meta::Orientation::Normal,
        Orientation::HorizontalFlip => looklab_meta::Orientation::FlipHorizontal,
        Orientation::Rotate180 => looklab_meta::Orientation::Rotate180,
        Orientation::VerticalFlip => looklab_meta::Orientation::FlipVertical,
        Orientation::Transpose => looklab_meta::Orientation::Transpose,
        Orientation::Rotate90 => looklab_meta::Orientation::Rotate90Clockwise,
        Orientation::Transverse => looklab_meta::Orientation::Transverse,
        Orientation::Rotate270 => looklab_meta::Orientation::Rotate270Clockwise,
    }
}

fn request(jpeg: Vec<u8>) -> PrepareRequest {
    PrepareRequest {
        raw: fixture_raw(),
        jpegs: vec![jpeg],
        prior: PriorTransformSpec::CameraMatrices { version: 3 },
        seed: [0x10; 32],
        params: PrepParams::default(),
    }
}

#[test]
fn wasm_prepare_is_byte_identical_across_runs() {
    let wasm = build_module();
    let engine = Engine::default();
    let module = Module::from_file(&engine, &wasm).expect("module loads");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("module has no imports");
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("module exports memory");
    let mut guest = Guest {
        alloc: instance
            .get_typed_func::<u32, u32>(&mut store, "ll_alloc")
            .expect("ll_alloc is exported"),
        free: instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "ll_free")
            .expect("ll_free is exported"),
        call: instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut store, "ll_call")
            .expect("ll_call is exported"),
        store,
        memory,
    };

    let jpeg = jpeg_from_fixture();
    let request = request(jpeg);
    let input = postcard::to_allocvec(&request).expect("request encodes");
    let first = guest.invoke(Function::Prepare.id(), &input);
    let second = guest.invoke(Function::Prepare.id(), &input);
    assert_eq!(first, second, "two wasmtime runs differ");

    let response: Result<Response, ModuleError> =
        postcard::from_bytes(&first).expect("response decodes");
    let images = match response.expect("preparation succeeds") {
        Response::Prepare(response) => response.images,
        other => panic!("unexpected response {other:?}"),
    };
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].metadata.make.as_deref(), Some("FUJIFILM"));
    assert!(images[0].alignment.correlation > 0.98);
    let samples = looklab_samples::decode(&images[0].blob).expect("sample blob decodes");
    assert!(samples.n() > 100);

    let native = dispatch(Function::Prepare.id(), &input);
    let native: Result<Response, ModuleError> =
        postcard::from_bytes(&native).expect("native response decodes");
    let native = match native.expect("native preparation succeeds") {
        Response::Prepare(response) => response,
        other => panic!("unexpected response {other:?}"),
    };
    if native.images[0].blob != images[0].blob {
        println!("note: native and wasm32 sample blobs differ");
    }
}
