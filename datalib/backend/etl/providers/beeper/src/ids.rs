//! Beeper entity ids. They are also the raw store's primary keys —
//! `rooms.id`, `users.id`, `events.id` — so an ingest mints them and
//! render reads them back, which is why this module is on the ingest
//! side. The scope is the store the row came from (`rooms.source`,
//! `"beeper_index"` today), so a second reader that happens to mint the
//! same native id never collides with this one.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Beeper;
/// Beeper's timestamps are meaningful below the second.
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Millis;

pub const KIND_ROOM: &str = "room";
pub const KIND_PERIOD: &str = "room_period";
pub const KIND_USER: &str = "user";
pub const KIND_EVENT: &str = "event";

fn identity(
    source: &str,
    entity_kind: &'static str,
    natural_key: String,
    date_ms: Option<i64>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::Upstream(source),
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn room(source: &str, native_room_id: &str) -> Identity {
    identity(source, KIND_ROOM, native_room_id.to_string(), None)
}

pub fn period(source: &str, native_room_id: &str, period_key: &str) -> Identity {
    identity(
        source,
        KIND_PERIOD,
        composite_key(&[native_room_id, period_key]),
        None,
    )
}

pub fn user(source: &str, native_user_id: &str) -> Identity {
    identity(source, KIND_USER, native_user_id.to_string(), None)
}

/// An event's stamp is its `timestamp_ms`, which is also the raw row's;
/// a reaction is an event too. The events table is keyed by this, so
/// a sync's new events land in adjacent leaves of the raw store as
/// well as the render store.
pub fn event(source: &str, native_event_id: &str, timestamp_ms: i64) -> Identity {
    identity(
        source,
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
            room("beeper_index", "!r:beeper.local"),
            period("beeper_index", "!r:beeper.local", "2024-03"),
            user("beeper_index", "@u:beeper.local"),
            event("beeper_index", "$e", 1_700_000_000_123),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    Scope::Upstream("beeper_index"),
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
            stamp_of(&event("s", "$e", 1_700_000_000_123).uuid),
            Some(1_700_000_000_123)
        );
        assert_eq!(stamp_of(&room("s", "!r").uuid), None);
    }

    #[test]
    fn kinds_and_stores_separate() {
        assert_ne!(room("s", "x").uuid, user("s", "x").uuid);
        assert_ne!(room("a", "x").uuid, room("b", "x").uuid);
    }
}
