// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scalar RGB16 PNG preparation and rendering for an import-free worker.

use looklab_publish::{PreparedScene, PublishError, Scene, SceneName, render_preview};

#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("unknown scene function {0}")]
    UnknownFunction(u32),
    #[error("invalid scene render request")]
    RenderRequest,
    #[error(transparent)]
    Publish(#[from] PublishError),
}

#[derive(Clone, Copy)]
enum Function {
    Prepare,
    Render,
}

impl TryFrom<u32> for Function {
    type Error = ModuleError;

    fn try_from(id: u32) -> Result<Self, Self::Error> {
        match id {
            1 => Ok(Self::Prepare),
            2 => Ok(Self::Render),
            _ => Err(ModuleError::UnknownFunction(id)),
        }
    }
}

/// Function 1 takes raw PNG bytes. Function 2 takes a little-endian prepared byte count, prepared bytes and LUT bytes.
///
/// # Errors
/// Rejects unknown functions, invalid scenes and invalid preview LUT lengths.
pub fn dispatch(fn_id: u32, input: &[u8]) -> Result<Vec<u8>, ModuleError> {
    let function = Function::try_from(fn_id)?;
    let name = SceneName::new("worker-scene")?;
    match function {
        Function::Prepare => Ok(PreparedScene::new(Scene::from_png(name, input)?).to_bytes()),
        Function::Render => {
            let header = input.get(..4).ok_or(ModuleError::RenderRequest)?;
            let len = u32::from_le_bytes(header.try_into().expect("four-byte length")) as usize;
            let end = 4usize.checked_add(len).ok_or(ModuleError::RenderRequest)?;
            let prepared = input.get(4..end).ok_or(ModuleError::RenderRequest)?;
            let scene = PreparedScene::from_bytes(name, prepared)?;
            Ok(render_preview(&input[end..], &scene)?)
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod exports {
    use std::cell::RefCell;

    thread_local! {
        static LAST_ERROR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    fn output(bytes: Vec<u8>) -> u64 {
        if bytes.is_empty() {
            return 0;
        }
        let boxed = bytes.into_boxed_slice();
        let len = u32::try_from(boxed.len()).expect("wasm32 memory is at most 4 GiB");
        let ptr = Box::into_raw(boxed) as *mut u8 as u32;
        (u64::from(ptr) << 32) | u64::from(len)
    }

    /// The returned allocation must be freed exactly once with the original length.
    #[unsafe(no_mangle)]
    pub extern "C" fn llw_alloc(len: u32) -> u32 {
        let boxed = vec![0u8; len as usize].into_boxed_slice();
        Box::into_raw(boxed) as *mut u8 as u32
    }

    /// # Safety
    /// `ptr` and `len` must identify a live allocation returned by this module.
    #[unsafe(no_mangle)]
    pub extern "C" fn llw_free(ptr: u32, len: u32) {
        if len == 0 {
            return;
        }
        // SAFETY: the caller supplies a live boxed slice with its original length.
        unsafe {
            let slice = core::ptr::slice_from_raw_parts_mut(ptr as usize as *mut u8, len as usize);
            drop(Box::from_raw(slice));
        }
    }

    /// Returns `(ptr << 32) | len` for raw success bytes, or zero on error.
    /// Each call clears the previous error.
    ///
    /// # Safety
    /// `in_ptr` and `in_len` must identify a live allocation from `llw_alloc` for this call.
    #[unsafe(no_mangle)]
    pub extern "C" fn llw_call(fn_id: u32, in_ptr: u32, in_len: u32) -> u64 {
        LAST_ERROR.with_borrow_mut(Vec::clear);
        let input = if in_len == 0 {
            &[]
        } else {
            // SAFETY: the caller keeps the input allocation alive for this call.
            unsafe { core::slice::from_raw_parts(in_ptr as usize as *const u8, in_len as usize) }
        };
        match crate::dispatch(fn_id, input) {
            Ok(bytes) => output(bytes),
            Err(error) => {
                LAST_ERROR.with_borrow_mut(|last| *last = error.to_string().into_bytes());
                0
            }
        }
    }

    /// Takes the last error as owned UTF-8 bytes, packed like `llw_call`; zero means no error.
    /// The returned allocation must be freed with `llw_free`.
    #[unsafe(no_mangle)]
    pub extern "C" fn llw_last_error() -> u64 {
        output(LAST_ERROR.with_borrow_mut(std::mem::take))
    }
}

#[cfg(test)]
mod tests {
    use super::{ModuleError, dispatch};
    use looklab_publish::{
        LUT_BYTES, PreparedScene, PublishError, Scene, SceneName, render_preview,
    };

    #[test]
    fn png_preparation_and_render_match_scalar_results_byte_for_byte() {
        let name = SceneName::new("reference").expect("name");
        let png = Scene::new(name.clone(), 2, 1, vec![[0.2, 0.3, 0.1], [-0.01, 0.1, 1.0]])
            .expect("scene")
            .to_png()
            .expect("PNG");
        let reference = PreparedScene::new(Scene::from_png(name, &png).expect("decode"));
        let bytes = dispatch(1, &png).expect("prepare");
        assert_eq!(bytes, reference.to_bytes());
        assert_eq!(dispatch(1, &png).expect("repeat prepare"), bytes);
        let lut = (0..LUT_BYTES / 4)
            .flat_map(|i| ((i % 257) as f32 / 256.0).to_le_bytes())
            .collect::<Vec<_>>();
        let mut request = (bytes.len() as u32).to_le_bytes().to_vec();
        request.extend_from_slice(&bytes);
        request.extend_from_slice(&lut);
        let rgba = dispatch(2, &request).expect("render");
        assert_eq!(
            rgba,
            render_preview(&lut, &reference).expect("scalar render")
        );
        assert_eq!(dispatch(2, &request).expect("repeat render"), rgba);
    }

    #[test]
    fn malformed_requests_return_errors() {
        assert!(matches!(
            dispatch(99, &[]),
            Err(ModuleError::UnknownFunction(99))
        ));
        assert!(matches!(
            dispatch(1, &[]),
            Err(ModuleError::Publish(PublishError::ScenePng(_)))
        ));
        for bytes in [&[][..], &[0, 0, 0][..], &[255, 255, 255, 255][..]] {
            assert!(matches!(
                dispatch(2, bytes),
                Err(ModuleError::RenderRequest)
            ));
        }
        assert!(matches!(
            dispatch(2, &[0; 4]),
            Err(ModuleError::Publish(PublishError::PreparedScene))
        ));
    }
}
