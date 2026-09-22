//! Google Takeout entity ids: the Google Chat and Google Voice feeds.
//! A Chat message id is `<space>/<topic>/<message>`, unique across
//! Google Chat; a Voice row's id is the one the ingest minted from the
//! export, unique across Voice. Both are provider-global.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::GoogleTakeout;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_SPACE: &str = "space";
pub const KIND_SPACE_MONTH: &str = "space_month";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_VOICE_CONVERSATION: &str = "voice_conversation";
pub const KIND_VOICE_MONTH: &str = "voice_month";
pub const KIND_VOICE_MESSAGE: &str = "voice_message";

fn identity(entity_kind: &'static str, natural_key: String, date_ms: Option<i64>) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn space(space: &str) -> Identity {
    identity(KIND_SPACE, space.to_string(), None)
}

pub fn space_month(space: &str, period_key: &str) -> Identity {
    identity(KIND_SPACE_MONTH, composite_key(&[space, period_key]), None)
}

pub fn message(message_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_MESSAGE, message_id.to_string(), date_ms)
}

/// `chat_id` carries its `voice:` prefix, as the bucket does.
pub fn voice_conversation(chat_id: &str) -> Identity {
    identity(KIND_VOICE_CONVERSATION, chat_id.to_string(), None)
}

pub fn voice_month(chat_id: &str, period_key: &str) -> Identity {
    identity(
        KIND_VOICE_MONTH,
        composite_key(&[chat_id, period_key]),
        None,
    )
}

pub fn voice_message(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_VOICE_MESSAGE, row_id.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            space("s"),
            space_month("s", "2024-03"),
            message("s/t/m", MS),
            voice_conversation("voice:+1555"),
            voice_month("voice:+1555", "2024-03"),
            voice_message("r1", MS),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn messages_carry_their_stamp_to_the_second_and_chats_none() {
        assert_eq!(stamp_of(&message("m", MS).uuid), Some(1_700_000_000_000));
        assert_eq!(
            stamp_of(&voice_message("r", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&space("s").uuid), None);
        assert_eq!(stamp_of(&voice_month("v", "2024-01").uuid), None);
    }

    #[test]
    fn the_two_feeds_do_not_alias() {
        assert_ne!(space("x").uuid, voice_conversation("x").uuid);
        assert_ne!(message("x", None).uuid, voice_message("x", None).uuid);
    }
}
