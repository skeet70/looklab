// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fuzzes the RAW decode entry point.
//!
//! rawler catches unwinds inside `RawLoader::decode`, so caught panics never reach libFuzzer,
//! while on wasm32 they abort. The hook records each panic location and saves the first input.
//! `OMP_FUZZ_ABORT_ON_PANIC=1` aborts instead, which `cargo fuzz tmin` needs;
//! `OMP_FUZZ_ABORT_LOCATION` limits the abort to locations containing it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use looklab_raw::{RawDecoder, RawlerDecoder};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, Once};

struct PanicRecord {
    location: String,
    message: String,
    backtrace: String,
}

struct State {
    last: Option<PanicRecord>,
    seen: HashSet<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        last: None,
        seen: HashSet::new(),
    })
});

static HOOK: Once = Once::new();

/// Without a location filter, `-minimize_crash` reduces every input to the
/// easiest panic in the target.
fn abort_on_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    if std::env::var_os("OMP_FUZZ_ABORT_ON_PANIC").is_none() {
        return false;
    }
    match std::env::var("OMP_FUZZ_ABORT_LOCATION") {
        Ok(narrow) => info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()).contains(&narrow))
            .unwrap_or(false),
        Err(_) => true,
    }
}

fn panic_hook(info: &std::panic::PanicHookInfo<'_>) {
    if abort_on_panic(info) {
        eprintln!("panic: {info}");
        eprintln!("{}", std::backtrace::Backtrace::force_capture());
        std::process::abort();
    }
    let record = PanicRecord {
        location: info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_default(),
        message: info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default(),
        backtrace: format!("{}", std::backtrace::Backtrace::force_capture()),
    };
    if let Ok(mut state) = STATE.lock() {
        state.last = Some(record);
    }
}

fn install_hook() {
    HOOK.call_once(|| std::panic::set_hook(Box::new(panic_hook)));
}

fn artifact_path(data: &[u8]) -> PathBuf {
    static PREFIX: LazyLock<PathBuf> = LazyLock::new(|| {
        std::env::args()
            .find_map(|argument| {
                argument
                    .strip_prefix("-artifact_prefix=")
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("artifacts/raw_decode"))
    });
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    PREFIX.join(format!("panic-{:016x}", hasher.finish()))
}

fn record_panic(data: &[u8]) {
    let record = match STATE.lock() {
        Ok(mut state) => state.last.take(),
        Err(_) => None,
    };
    let Some(record) = record else {
        return;
    };
    let first = match STATE.lock() {
        Ok(mut state) => state.seen.insert(record.location.clone()),
        Err(_) => false,
    };
    if !first {
        return;
    }
    eprintln!("PANIC {}: {}", record.location, record.message);
    eprintln!("{}", record.backtrace);
    let path = artifact_path(data);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, data) {
        Ok(()) => eprintln!("saved {}", path.display()),
        Err(error) => eprintln!("could not save {}: {error}", path.display()),
    }
}

fuzz_target!(|data: &[u8]| {
    install_hook();
    let _ = RawlerDecoder.decode(data);
    record_panic(data);
});
