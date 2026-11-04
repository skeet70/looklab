// SPDX-License-Identifier: AGPL-3.0-or-later
//! Callers keep each `(ptr, len)` pair from this module and free it exactly once.

#![allow(unsafe_code)]

#[cfg(target_arch = "wasm32")]
mod exports {
    /// # Safety
    /// The returned pair must be passed to `ll_free` with the same `len`.
    #[unsafe(no_mangle)]
    pub extern "C" fn ll_alloc(len: u32) -> u32 {
        let boxed = vec![0u8; len as usize].into_boxed_slice();
        Box::into_raw(boxed) as *mut u8 as u32
    }

    /// # Safety
    /// `ptr` and `len` must be a pair produced by this module and not yet freed.
    #[unsafe(no_mangle)]
    pub extern "C" fn ll_free(ptr: u32, len: u32) {
        if len == 0 {
            return;
        }
        // SAFETY: the caller passes a live allocation of exactly this length,
        // which we reconstruct as the box that produced it.
        unsafe {
            let slice = core::ptr::slice_from_raw_parts_mut(ptr as usize as *mut u8, len as usize);
            drop(Box::from_raw(slice));
        }
    }

    /// Returns `(out_ptr << 32) | out_len` for the postcard result of `fn_id`.
    ///
    /// # Safety
    /// `in_ptr` and `in_len` must be a live pair from `ll_alloc`.
    #[unsafe(no_mangle)]
    pub extern "C" fn ll_call(fn_id: u32, in_ptr: u32, in_len: u32) -> u64 {
        let input: &[u8] = if in_len == 0 {
            &[]
        } else {
            // SAFETY: the caller allocated in_len bytes at in_ptr and keeps them alive.
            unsafe { core::slice::from_raw_parts(in_ptr as usize as *const u8, in_len as usize) }
        };
        let boxed = crate::dispatch(fn_id, input).into_boxed_slice();
        let len = u32::try_from(boxed.len()).expect("wasm32 memory is at most 4 GiB");
        let out_ptr = Box::into_raw(boxed) as *mut u8 as u32;
        (u64::from(out_ptr) << 32) | u64::from(len)
    }
}
