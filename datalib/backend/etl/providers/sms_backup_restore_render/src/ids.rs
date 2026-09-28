//! SMS Backup & Restore entity ids. A row's id is the one the ingest
//! minted from the export (number, date, body hash), unique across the
//! provider; a conversation is keyed on its number.

use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::SmsBackupRestore;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MONTH: &str = "conversation_month";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_CALL: &str = "call";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

/// `key` carries its `sms:` prefix, as the bucket does.
pub fn conversation(source_id: &str, key: &str) -> Identity {
    IDS.mint(source_id, KIND_CONVERSATION, key.to_string(), None)
}

pub fn month(source_id: &str, key: &str, period_key: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_MONTH,
        composite_key(&[key, period_key]),
        None,
    )
}

pub fn message(source_id: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_MESSAGE, row_id.to_string(), date_ms)
}

pub fn call(source_id: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_CALL, row_id.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            conversation("src", "sms:+1555"),
            month("src", "sms:+1555", "2024-03"),
            message("src", "r1", MS),
            call("src", "r1", MS),
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
    fn items_carry_their_stamp_to_the_second_and_chats_none() {
        assert_eq!(
            stamp_of(&message("src", "r", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&call("src", "r", None).uuid), None);
        assert_eq!(stamp_of(&conversation("src", "c").uuid), None);
    }
}
