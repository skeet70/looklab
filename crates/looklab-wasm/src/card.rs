// SPDX-License-Identifier: AGPL-3.0-or-later
//! Checks a render tag against the published card file ([docs/04 §8.4](../../../docs/04-matching-and-export.md)).

use looklab_export::card::{CardError, read_card};
use looklab_meta::RenderTag;

#[derive(Debug, thiserror::Error)]
pub enum CardCheckError {
    #[error("the published card could not be read: {0}")]
    Card(#[from] CardError),
    #[error("the render tag cid does not match the published card")]
    CidMismatch,
}

pub fn check_card(tag: &RenderTag, card: &[u8]) -> Result<(), CardCheckError> {
    let card = read_card(card)?;
    if card.cid_prefix() == tag.cid_prefix {
        Ok(())
    } else {
        Err(CardCheckError::CidMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_meta::parse_render_tag;

    const CARD: &[u8] = include_bytes!("../../../data/card/conformance-v1/valid/card-33.llk");

    fn tagged(prefix: [u8; 8]) -> RenderTag {
        let hex: String = prefix.iter().map(|byte| format!("{byte:02x}")).collect();
        parse_render_tag(&format!(
            "looklab-card/1 fw=acme-sim/2.0.1 look=conformance-tone@1.0.0 cid={hex}"
        ))
        .expect("render tag")
    }

    #[test]
    fn a_mismatched_cid_is_detected() {
        let card = read_card(CARD).expect("conformance card");
        let mut wrong = card.cid_prefix();
        wrong[0] ^= 0xff;

        assert!(check_card(&tagged(card.cid_prefix()), CARD).is_ok());
        assert!(matches!(
            check_card(&tagged(wrong), CARD),
            Err(CardCheckError::CidMismatch)
        ));
    }

    #[test]
    fn an_unreadable_card_is_reported() {
        assert!(matches!(
            check_card(&tagged([0; 8]), b"not a card"),
            Err(CardCheckError::Card(_))
        ));
    }
}
