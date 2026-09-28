//! Beeper entity ids. The raw store keys rooms, users and events by
//! their Matrix ids, which are unique across Matrix, so the natural key
//! is the raw key and the scope is provider-global.

use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Beeper;
/// Beeper's timestamps are meaningful below the second.
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Millis;

pub const KIND_ROOM: &str = "room";
pub const KIND_PERIOD: &str = "room_period";
pub const KIND_USER: &str = "user";
pub const KIND_EVENT: &str = "event";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

pub fn room(source_id: &str, native_room_id: &str) -> Identity {
    IDS.mint(source_id, KIND_ROOM, native_room_id.to_string(), None)
}

pub fn period(source_id: &str, native_room_id: &str, period_key: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_PERIOD,
        composite_key(&[native_room_id, period_key]),
        None,
    )
}

pub fn user(source_id: &str, native_user_id: &str) -> Identity {
    IDS.mint(source_id, KIND_USER, native_user_id.to_string(), None)
}

/// An event's stamp is its `timestamp_ms`, which is also the raw row's;
/// a reaction is an event too. The events table is keyed by this, so
/// a sync's new events land in adjacent leaves of the raw store as
/// well as the render store.
pub fn event(source_id: &str, native_event_id: &str, timestamp_ms: i64) -> Identity {
    IDS.mint(
        source_id,
        KIND_EVENT,
        native_event_id.to_string(),
        Some(timestamp_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            room("src", "!r:beeper.local"),
            period("src", "!r:beeper.local", "2024-03"),
            user("src", "@u:beeper.local"),
            event("src", "$e", 1_700_000_000_123),
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
    fn an_event_carries_its_stamp_to_the_millisecond() {
        assert_eq!(
            stamp_of(&event("src", "$e", 1_700_000_000_123).uuid),
            Some(1_700_000_000_123)
        );
        assert_eq!(stamp_of(&room("src", "!r").uuid), None);
    }

    #[test]
    fn kinds_separate() {
        assert_ne!(room("src", "x").uuid, user("src", "x").uuid);
    }
}
