//! Signal entity ids. A backup's `chat_id`, `author_id` and
//! `recipient_id` are autoincrements local to that backup file, and the
//! backup carries no identifier for the account that made it, so the
//! ids are scoped to the configured source. A recipient's e164 or ACI
//! would be a better key, but `recipients.identifier` is nullable, and
//! a scope that is sometimes there re-keys every row the day it appears.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Signal;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_CHAT: &str = "chat";
pub const KIND_PERIOD: &str = "chat_period";
pub const KIND_MESSAGE: &str = "message";

fn identity(
    source_id: &str,
    entity_kind: &'static str,
    natural_key: String,
    date_ms: Option<i64>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn chat(source_id: &str, chat_id: &str) -> Identity {
    identity(source_id, KIND_CHAT, chat_id.to_string(), None)
}

pub fn period(source_id: &str, chat_id: &str, period_key: &str) -> Identity {
    identity(
        source_id,
        KIND_PERIOD,
        composite_key(&[chat_id, period_key]),
        None,
    )
}

/// `date_sent` is both part of the key and the item's stamp.
pub fn message(source_id: &str, chat_id: &str, author_id: &str, date_sent: i64) -> Identity {
    identity(
        source_id,
        KIND_MESSAGE,
        composite_key(&[chat_id, author_id, &date_sent.to_string()]),
        Some(date_sent),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            chat("sig", "1"),
            period("sig", "1", "2024-03"),
            message("sig", "1", "2", 1_700_000_000_999),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "sig",
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn two_sources_and_two_kinds_separate() {
        assert_ne!(chat("a", "1").uuid, chat("b", "1").uuid);
        assert_ne!(chat("a", "1").uuid, period("a", "1", "all").uuid);
    }

    #[test]
    fn a_message_carries_its_date_sent_to_the_second() {
        assert_eq!(
            stamp_of(&message("s", "1", "2", 1_700_000_000_999).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&chat("s", "1").uuid), None);
    }
}
