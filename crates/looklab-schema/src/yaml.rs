// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounds every untrusted YAML document before it is deserialized.
//!
//! A document that names an anchor or an alias is rejected outright; the scan
//! also bounds the bytes, the nesting depth, the nodes per collection and the
//! scalar lengths deserialization may allocate.

use saphyr_parser::{Event, Parser};

use crate::error::{ErrorKind, ValidationError};

/// The largest payload the browser form and the intake accept.
pub(crate) const MAX_YAML_BYTES: usize = 256 * 1024;

const MAX_EVENTS: usize = 20_000;
const MAX_DEPTH: usize = 32;
const MAX_COLLECTION_ITEMS: usize = 4096;
const MAX_SCALAR_BYTES: usize = 64 * 1024;

/// # Errors
/// A document over the byte limit, an anchor or alias, a parser error, or a
/// document that exceeds the depth, node or scalar bounds.
pub(crate) fn check(source: &str) -> Result<(), Vec<ValidationError>> {
    if source.len() > MAX_YAML_BYTES {
        return Err(reject("a document under 256 KiB"));
    }
    let mut events = 0usize;
    let mut containers: Vec<usize> = Vec::new();
    for item in Parser::new_from_str(source) {
        let (event, _span) = item.map_err(|error| reject_owned(format!("YAML: {error}")))?;
        events += 1;
        if events > MAX_EVENTS {
            return Err(reject("a document under 20 000 YAML events"));
        }
        if let Some(parent) = containers.last_mut() {
            *parent += 1;
            if *parent > MAX_COLLECTION_ITEMS {
                return Err(reject("collections under 4096 entries"));
            }
        }
        match event {
            Event::Alias(_) => return Err(reject("a document without aliases")),
            Event::Scalar(value, _, anchor, _) => {
                if anchor != 0 {
                    return Err(reject("a document without anchors"));
                }
                if value.len() > MAX_SCALAR_BYTES {
                    return Err(reject("scalars under 64 KiB"));
                }
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if anchor != 0 {
                    return Err(reject("a document without anchors"));
                }
                if containers.len() >= MAX_DEPTH {
                    return Err(reject("nesting under 32 levels"));
                }
                containers.push(0);
            }
            Event::SequenceEnd | Event::MappingEnd => {
                containers.pop();
            }
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd => {}
        }
    }
    Ok(())
}

fn reject(expected: &'static str) -> Vec<ValidationError> {
    vec![ValidationError::new(
        "",
        ErrorKind::InvalidValue { expected },
    )]
}

fn reject_owned(message: String) -> Vec<ValidationError> {
    vec![ValidationError::new("", ErrorKind::YamlRejected(message))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_document_passes() {
        let document = "body: fujifilm/x-t5\nepochs:\n  - id: 1\n    controls: [a, b]\n";
        check(document).expect("plain document");
    }

    #[test]
    fn an_alias_is_rejected() {
        let document = "a: &x [1, 2]\nb: *x\n";
        assert!(check(document).is_err());
    }

    #[test]
    fn an_unused_anchor_is_rejected() {
        assert!(check("a: &x 1\nb: 2\n").is_err());
        assert!(check("a: &x\n  b: 2\n").is_err());
    }

    #[test]
    fn anchor_characters_inside_scalars_are_accepted() {
        check("exif_aliases: [X-T5*, a&b]\nnote: \"&not an anchor\"\n").expect("scalars");
    }

    #[test]
    fn a_deep_document_is_rejected() {
        let mut document = String::new();
        for depth in 0..40 {
            document.push_str(&" ".repeat(depth));
            document.push_str("- \n");
        }
        assert!(check(&document).is_err());
    }

    #[test]
    fn a_long_scalar_and_an_oversized_document_are_rejected() {
        assert!(check(&format!("a: \"{}\"\n", "x".repeat(MAX_SCALAR_BYTES + 1))).is_err());
        assert!(check(&"x".repeat(MAX_YAML_BYTES + 1)).is_err());
    }
}
