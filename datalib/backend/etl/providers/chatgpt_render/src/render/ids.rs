//! ChatGPT entity ids.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Chatgpt;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

pub fn conversation(source_id: &str, conversation_id: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_CONVERSATION,
        conversation_id.to_string(),
        None,
    )
}

/// `date_ms` is the item's `date_ms` after the fallback to the previous
/// item's stamp, so it is what the row stores.
pub fn message(source_id: &str, message_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_MESSAGE, message_id.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_000);

    #[test]
    fn upstream_ids_no_longer_leak_into_our_keyspace() {
        // The fixture's ids are the shape that put `chatgpt` on
        // NON_UUID_PK_PROVIDERS in the first place.
        for got in [
            conversation("src", "68fa0001-fake-7000-8000-positronic0001"),
            message("src", "msg-fake-poly-0001", MS),
        ] {
            assert_eq!(got.uuid.len(), 36, "{}", got.uuid);
            assert!(
                got.uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{} must be hex+dashes",
                got.uuid,
            );
        }
    }

    /// `upstream_id` must regenerate `uuid`; see the equivalent
    /// test in the claude ids module.
    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [conversation("src", "c1"), message("src", "m1", MS)] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn a_message_carries_its_stamp_to_the_second() {
        assert_eq!(
            stamp_of(&message("src", "m1", Some(1_700_000_000_999)).uuid),
            MS
        );
        assert_eq!(stamp_of(&conversation("src", "c1").uuid), None);
    }

    #[test]
    fn kinds_separate_ids_over_the_same_key() {
        // OpenAI does not promise its conversation and message id
        // spaces are disjoint, and we no longer depend on it.
        assert_ne!(
            conversation("src", "x").uuid,
            message("src", "x", None).uuid
        );
    }
}
