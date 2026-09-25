//! Google Takeout entity ids: the Google Chat and Google Voice feeds.
//! A Chat message id is `<space>/<topic>/<message>`, unique across
//! Google Chat; a Voice row's id is the one the ingest minted from the
//! export, unique across Voice. Both are provider-global.

use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::GoogleTakeout;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_SPACE: &str = "space";
pub const KIND_SPACE_MONTH: &str = "space_month";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_VOICE_CONVERSATION: &str = "voice_conversation";
pub const KIND_VOICE_MONTH: &str = "voice_month";
pub const KIND_VOICE_MESSAGE: &str = "voice_message";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

pub fn space(source_id: &str, space: &str) -> Identity {
    IDS.mint(source_id, KIND_SPACE, space.to_string(), None)
}

pub fn space_month(source_id: &str, space: &str, period_key: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_SPACE_MONTH,
        composite_key(&[space, period_key]),
        None,
    )
}

pub fn message(source_id: &str, message_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_MESSAGE, message_id.to_string(), date_ms)
}

/// `chat_id` carries its `voice:` prefix, as the bucket does.
pub fn voice_conversation(source_id: &str, chat_id: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_VOICE_CONVERSATION,
        chat_id.to_string(),
        None,
    )
}

pub fn voice_month(source_id: &str, chat_id: &str, period_key: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_VOICE_MONTH,
        composite_key(&[chat_id, period_key]),
        None,
    )
}

pub fn voice_message(source_id: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_VOICE_MESSAGE, row_id.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            space("src", "s"),
            space_month("src", "s", "2024-03"),
            message("src", "s/t/m", MS),
            voice_conversation("src", "voice:+1555"),
            voice_month("src", "voice:+1555", "2024-03"),
            voice_message("src", "r1", MS),
        ] {
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
    fn messages_carry_their_stamp_to_the_second_and_chats_none() {
        assert_eq!(
            stamp_of(&message("src", "m", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            stamp_of(&voice_message("src", "r", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&space("src", "s").uuid), None);
        assert_eq!(stamp_of(&voice_month("src", "v", "2024-01").uuid), None);
    }

    #[test]
    fn the_two_feeds_do_not_alias() {
        assert_ne!(space("src", "x").uuid, voice_conversation("src", "x").uuid);
        assert_ne!(
            message("src", "x", None).uuid,
            voice_message("src", "x", None).uuid
        );
    }
}
