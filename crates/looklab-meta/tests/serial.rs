// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_meta::{MetaError, RawSerial, SerialHmacKey};

#[test]
fn hmac_matches_reference_vector() {
    let serial = RawSerial::new(b"SERIAL123").expect("ASCII serial");
    let key = SerialHmacKey::new(core::array::from_fn(|index| index as u8));
    let tag = serial.hmac(&key, "FUJIFILM", "X-T5");
    let hex: String = tag
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        hex,
        "0a799fb04a6ea49a414bf42d689846ab8eeb35341a3e6685330c83899fc9029a"
    );
}

#[test]
fn hmac_covers_make_model_and_serial() {
    let serial = RawSerial::new(b"SERIAL123").expect("ASCII serial");
    let key = SerialHmacKey::new([7u8; 32]);
    let base = serial.hmac(&key, "FUJIFILM", "X-T5");
    assert!(base != serial.hmac(&key, "FUJIFILM", "X-H2"));
    assert!(base != serial.hmac(&key, "FUJIFILM", "X-T5 "));
    assert!(base != serial.hmac(&SerialHmacKey::new([8u8; 32]), "FUJIFILM", "X-T5"));
}

#[test]
fn envelope_field_is_present_tag_length_then_ascii() {
    let serial = RawSerial::new(b"SERIAL123").expect("ASCII serial");
    let mut out = Vec::new();
    serial.write_envelope_field(&mut out);
    assert_eq!(&out[..], b"\x01\x09SERIAL123");
}

#[test]
fn envelope_constructor_validates_ascii_and_length() {
    assert!(RawSerial::from_envelope_ascii(b"SERIAL123").is_ok());

    let max = vec![b'A'; 64];
    assert!(RawSerial::from_envelope_ascii(&max).is_ok());

    let too_long = vec![b'A'; 65];
    assert!(matches!(
        RawSerial::from_envelope_ascii(&too_long),
        Err(MetaError::InvalidSerial)
    ));
    assert!(matches!(
        RawSerial::from_envelope_ascii(&[]),
        Err(MetaError::InvalidSerial)
    ));
    assert!(matches!(
        RawSerial::from_envelope_ascii(&[0xff]),
        Err(MetaError::InvalidSerial)
    ));
    assert!(matches!(
        RawSerial::from_envelope_ascii(b"AB\n"),
        Err(MetaError::InvalidSerial)
    ));
}
