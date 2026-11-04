// SPDX-License-Identifier: AGPL-3.0-or-later
//! Plain C ABI for `site/workers/match-worker.js`: it instantiates without
//! JavaScript imports and only copies postcard bytes.

#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::match_api::{MatchRequest, VerifyRequestMessage, run_match, run_verify};

use crate::{
    WORKER_FN_MATCH as FN_MATCH, WORKER_FN_VERSION as FN_VERSION, WORKER_FN_VERIFY as FN_VERIFY,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerVersion {
    pub format: String,
    pub optimizer: String,
    pub abi: u32,
}

pub fn dispatch(fn_id: u32, input: &[u8]) -> Vec<u8> {
    match fn_id {
        FN_VERSION => reply(Ok(WorkerVersion {
            format: "looklab-wasm-worker/1".to_owned(),
            optimizer: looklab_match::OPTIMIZER_VERSION.to_owned(),
            abi: crate::MODULE_ABI_VERSION,
        })),
        FN_MATCH => reply(decode::<MatchRequest>(input).and_then(|request| {
            run_match(&request).map_err(|error| error.to_string())
        })),
        FN_VERIFY => reply(decode::<VerifyRequestMessage>(input).and_then(|request| {
            run_verify(&request).map_err(|error| error.to_string())
        })),
        other => reply::<WorkerVersion>(Err(format!("unknown worker function {other}"))),
    }
}

fn decode<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T, String> {
    postcard::from_bytes(input).map_err(|error| error.to_string())
}

fn reply<T: Serialize>(result: Result<T, String>) -> Vec<u8> {
    postcard::to_allocvec(&result).unwrap_or_default()
}

/// # Safety
/// The returned pair must be passed to [`llw_free`] with the same `len`.
#[unsafe(no_mangle)]
pub extern "C" fn llw_alloc(len: u32) -> u32 {
    let boxed = vec![0u8; len as usize].into_boxed_slice();
    Box::into_raw(boxed) as *mut u8 as u32
}

/// # Safety
/// `ptr` and `len` must be a pair produced by this module and not yet freed.
#[unsafe(no_mangle)]
pub extern "C" fn llw_free(ptr: u32, len: u32) {
    if len == 0 {
        return;
    }
    // SAFETY: the caller passes a live allocation of exactly this length, which
    // we reconstruct as the box that produced it.
    unsafe {
        let slice = core::ptr::slice_from_raw_parts_mut(ptr as usize as *mut u8, len as usize);
        drop(Box::from_raw(slice));
    }
}

/// Returns `(out_ptr << 32) | out_len` for the postcard reply of `fn_id`.
///
/// # Safety
/// `in_ptr` and `in_len` must be a live pair from [`llw_alloc`].
#[unsafe(no_mangle)]
pub extern "C" fn llw_call(fn_id: u32, in_ptr: u32, in_len: u32) -> u64 {
    let input: &[u8] = if in_len == 0 {
        &[]
    } else {
        // SAFETY: the caller allocated in_len bytes at in_ptr and keeps them alive.
        unsafe { core::slice::from_raw_parts(in_ptr as usize as *const u8, in_len as usize) }
    };
    let boxed = dispatch(fn_id, input).into_boxed_slice();
    let len = u32::try_from(boxed.len()).expect("wasm32 memory is at most 4 GiB");
    let out_ptr = Box::into_raw(boxed) as *mut u8 as u32;
    (u64::from(out_ptr) << 32) | u64::from(len)
}
