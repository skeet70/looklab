// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(table) = looklab_samples::decode_patch_table(data) {
        let bytes =
            looklab_samples::encode_patch_table(&table).expect("a decoded table re-encodes");
        let again = looklab_samples::decode_patch_table(&bytes).expect("re-encoded bytes decode");
        assert_eq!(again, table);
    }
});
