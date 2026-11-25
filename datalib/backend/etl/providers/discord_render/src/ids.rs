//! Discord entity ids. A snowflake is unique across the service, so each
//! natural key is the object's own id, as the raw store keys it, and no
//! account is part of it: a message two accounts can read is one message.

use datalib_etl_discord::ingest::snowflake;
use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Discord;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Millis;

pub const KIND_SERVER: &str = "server";
pub const KIND_CHANNEL: &str = "channel";
pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_REACTION: &str = "reaction";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

/// When the record with this id was made: a snowflake carries it.
pub fn ms(id: &str) -> Option<i64> {
    snowflake::parse(id).map(snowflake::unix_ms)
}

pub fn server(source_id: &str, guild_id: &str) -> Identity {
    IDS.mint(source_id, KIND_SERVER, guild_id.to_string(), None)
}

/// A channel as the place its conversation and its threads are in.
pub fn channel(source_id: &str, channel_id: &str) -> Identity {
    IDS.mint(source_id, KIND_CHANNEL, channel_id.to_string(), None)
}

/// What is read together: a channel's own messages, a thread's, a direct
/// message's. Its row's stamps are derived from what is in it.
pub fn conversation(source_id: &str, channel_id: &str) -> Identity {
    IDS.mint(source_id, KIND_CONVERSATION, channel_id.to_string(), None)
}

pub fn message(source_id: &str, message_id: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_MESSAGE,
        message_id.to_string(),
        ms(message_id),
    )
}

/// `who` is `me` for the account's own reaction and empty for everyone
/// else's, whom Discord counts and does not name.
pub fn reaction(source_id: &str, message_id: &str, emoji: &str, who: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_REACTION,
        composite_key(&[message_id, emoji, who]),
        ms(message_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const ID: &str = "175928847299117063";

    /// A channel, its conversation and a thread's first message can share
    /// a snowflake; each is its own entity.
    #[test]
    fn one_snowflake_is_several_entities() {
        let uuids = [
            server("src", ID).uuid,
            channel("src", ID).uuid,
            conversation("src", ID).uuid,
            message("src", ID).uuid,
            reaction("src", ID, "🖖", "").uuid,
            reaction("src", ID, "🖖", "me").uuid,
        ];
        let distinct: std::collections::HashSet<&String> = uuids.iter().collect();
        assert_eq!(distinct.len(), uuids.len());
    }

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            server("src", ID),
            channel("src", ID),
            conversation("src", ID),
            message("src", ID),
            reaction("src", ID, "🖖", "me"),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at
                ),
            );
        }
    }

    #[test]
    fn a_messages_stamp_is_the_one_in_its_id() {
        assert_eq!(stamp_of(&message("src", ID).uuid), Some(1_462_015_105_796));
        assert_eq!(stamp_of(&conversation("src", ID).uuid), None);
        assert_eq!(stamp_of(&message("src", "not-an-id").uuid), None);
    }
}
