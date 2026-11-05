// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use looklab_publish::{LUT_BYTES, Scene, SceneName};
use looklab_scene_module::dispatch;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

struct Guest {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    call: TypedFunc<(u32, u32, u32), u64>,
    last_error: TypedFunc<(), u64>,
}

impl Guest {
    fn take_output(&mut self, packed: u64) -> Vec<u8> {
        let ptr = (packed >> 32) as u32;
        let len = packed as u32;
        let mut output = vec![0; len as usize];
        self.memory
            .read(&self.store, ptr as usize, &mut output)
            .expect("read output");
        self.free
            .call(&mut self.store, (ptr, len))
            .expect("free output");
        output
    }

    fn invoke(&mut self, function: u32, input: &[u8]) -> u64 {
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
        packed
    }
}

#[test]
fn import_free_wasm_matches_native_and_returns_owned_errors() {
    let path = std::env::var_os("LOOKLAB_SCENE_MODULE_WASM").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../site/generated/wasm/looklab_scene_module.wasm")
        },
        PathBuf::from,
    );
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)
        .expect("build scene wasm with site/scripts/wasm-assets.sh before running this test");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate without imports");
    let mut guest = Guest {
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
        last_error: instance
            .get_typed_func(&mut store, "llw_last_error")
            .expect("last error"),
        store,
    };
    let png = Scene::new(
        SceneName::new("parity").expect("name"),
        3,
        1,
        vec![[0.1, 0.2, 0.3], [-0.01, 0.01, 1.0], [5.0, 3.0, 1.0]],
    )
    .expect("scene")
    .to_png()
    .expect("PNG");
    let prepared = dispatch(1, &png).expect("native prepare");
    for _ in 0..2 {
        let packed = guest.invoke(1, &png);
        assert_ne!(packed, 0);
        assert_eq!(guest.take_output(packed), prepared);
    }
    let lut = (0..LUT_BYTES / 4)
        .flat_map(|i| ((i % 257) as f32 / 256.0).to_le_bytes())
        .collect::<Vec<_>>();
    let mut request = (prepared.len() as u32).to_le_bytes().to_vec();
    request.extend_from_slice(&prepared);
    request.extend_from_slice(&lut);
    let rgba = dispatch(2, &request).expect("native render");
    for _ in 0..2 {
        let packed = guest.invoke(2, &request);
        assert_ne!(packed, 0);
        assert_eq!(guest.take_output(packed), rgba);
    }
    for (function, input) in [(99, &[][..]), (1, &[][..]), (2, &[0, 0, 0][..])] {
        assert_eq!(guest.invoke(function, input), 0);
        let packed = guest
            .last_error
            .call(&mut guest.store, ())
            .expect("get error");
        assert_ne!(packed, 0);
        let error = guest.take_output(packed);
        assert!(!String::from_utf8(error).expect("UTF-8 error").is_empty());
        assert_eq!(
            guest
                .last_error
                .call(&mut guest.store, ())
                .expect("error consumed"),
            0
        );
    }
    assert_eq!(guest.invoke(99, &[]), 0);
    let packed = guest.invoke(1, &png);
    assert_eq!(guest.take_output(packed), prepared);
    assert_eq!(
        guest
            .last_error
            .call(&mut guest.store, ())
            .expect("error cleared"),
        0
    );
}
