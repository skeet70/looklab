// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;
use looklab_upload::{EnvelopeParts, Limits, encode_envelope, parse_envelope_strict, peek_nonce};

fuzz_target!(|data: &[u8]| {
    let _ = peek_nonce(data);
    if let Ok(envelope) = parse_envelope_strict(data, &Limits::default()) {
        let bytes = encode_envelope(&EnvelopeParts {
            nonce: envelope.nonce,
            send_manifest: &envelope.send_manifest,
            manifest: &envelope.manifest,
            assertion: &envelope.assertion,
            pow: &envelope.pow,
            serial: envelope.serial.as_ref(),
            blobs: &envelope.blobs,
        });
        assert_eq!(bytes, data);
    }
});
