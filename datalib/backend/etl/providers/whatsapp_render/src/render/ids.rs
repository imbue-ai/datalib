//! WhatsApp entity ids. A chat's JID and a message's `(chat_jid,
//! key_id, from_me)` are unique within one account, and msgstore's
//! `chat.account_jid_row_id` is nullable (the fixture leaves it so), so
//! the account is not a scope this can rely on: the ids are scoped to
//! the configured source instead, which is what the old recipe did.

use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Whatsapp;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_CHAT: &str = "chat";
pub const KIND_PERIOD: &str = "chat_period";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_REACTION: &str = "reaction";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

pub fn chat(source_id: &str, chat_jid: &str) -> Identity {
    IDS.mint(source_id, KIND_CHAT, chat_jid.to_string(), None)
}

pub fn period(source_id: &str, chat_jid: &str, period_key: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_PERIOD,
        composite_key(&[chat_jid, period_key]),
        None,
    )
}

fn message_key(chat_jid: &str, key_id: &str, from_me: i64) -> String {
    composite_key(&[chat_jid, key_id, &from_me.to_string()])
}

pub fn message(
    source_id: &str,
    chat_jid: &str,
    key_id: &str,
    from_me: i64,
    date_ms: Option<i64>,
) -> Identity {
    IDS.mint(
        source_id,
        KIND_MESSAGE,
        message_key(chat_jid, key_id, from_me),
        date_ms,
    )
}

pub fn reaction(
    source_id: &str,
    chat_jid: &str,
    key_id: &str,
    from_me: i64,
    date_ms: Option<i64>,
) -> Identity {
    IDS.mint(
        source_id,
        KIND_REACTION,
        message_key(chat_jid, key_id, from_me),
        date_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            chat("wa", "a@s.whatsapp.net"),
            period("wa", "a@s.whatsapp.net", "2024-03"),
            message("wa", "a@s.whatsapp.net", "K1", 1, MS),
            reaction("wa", "a@s.whatsapp.net", "K1", 0, MS),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "wa",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    /// The same key_id from each side of a chat is two messages.
    #[test]
    fn from_me_and_kind_separate() {
        assert_ne!(
            message("wa", "j", "K1", 0, MS).uuid,
            message("wa", "j", "K1", 1, MS).uuid
        );
        assert_ne!(
            message("wa", "j", "K1", 0, MS).uuid,
            reaction("wa", "j", "K1", 0, MS).uuid
        );
        assert_ne!(chat("wa", "j").uuid, chat("wa-2", "j").uuid);
    }

    #[test]
    fn messages_carry_their_stamp_to_the_second_and_chats_none() {
        assert_eq!(
            stamp_of(&message("wa", "j", "K", 0, MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&chat("wa", "j").uuid), None);
    }
}
