// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(set) = looklab_samples::decode(data) {
        let bytes = looklab_samples::encode(&set).expect("a decoded set re-encodes");
        let again = looklab_samples::decode(&bytes).expect("re-encoded bytes decode");
        assert_eq!(again, set);
    }
});
