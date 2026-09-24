//! Calendar entity ids. An event's own id (an iCalendar `UID`, a Google
//! event id) is unique within its calendar, so the calendar leads the
//! key. No stamp: an event's time moves when it is rescheduled, and its
//! id must not.

use datalib_id::{composite_key, IdNamespace, Identity};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Calendar;

/// A one-off event or a recurring series: the same upstream thing, and
/// the same id when a one-off becomes recurring.
pub const KIND_EVENT: &str = "event";
/// One changed occurrence of a series.
pub const KIND_OCCURRENCE: &str = "occurrence";
pub const KIND_CALENDAR: &str = "calendar";

fn identity(source_id: &str, entity_kind: &'static str, natural_key: String) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        None,
        entity_kind,
        natural_key,
        None,
    )
}

pub fn event(source_id: &str, calendar_id: &str, event_id: &str) -> Identity {
    identity(
        source_id,
        KIND_EVENT,
        composite_key(&[calendar_id, event_id]),
    )
}

/// An iCalendar occurrence, keyed by its series' `UID` and the
/// `RECURRENCE-ID` it overrides, spelled by `EventTime::key`.
pub fn ics_occurrence(source_id: &str, calendar_id: &str, uid: &str, recurrence: &str) -> Identity {
    identity(
        source_id,
        KIND_OCCURRENCE,
        composite_key(&[calendar_id, uid, recurrence]),
    )
}

/// A Google occurrence: Google gives each its own event id.
pub fn google_occurrence(source_id: &str, calendar_id: &str, event_id: &str) -> Identity {
    identity(
        source_id,
        KIND_OCCURRENCE,
        composite_key(&[calendar_id, event_id]),
    )
}

pub fn calendar(source_id: &str, calendar_id: &str) -> Identity {
    identity(source_id, KIND_CALENDAR, calendar_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            event("c", "bridge", "uid-1"),
            ics_occurrence("c", "bridge", "uid-1", "20260312T170000Z"),
            google_occurrence("c", "picard@enterprise.test", "abc_20260312T170000Z"),
            calendar("c", "bridge"),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "c",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at
                ),
            );
        }
    }

    #[test]
    fn a_series_its_occurrences_and_other_calendars_stay_apart() {
        let series = event("c", "bridge", "uid-1");
        assert_ne!(series.uuid, event("c", "ops", "uid-1").uuid);
        assert_ne!(
            series.uuid,
            ics_occurrence("c", "bridge", "uid-1", "20260312").uuid
        );
        assert_ne!(
            ics_occurrence("c", "bridge", "uid-1", "20260312").uuid,
            ics_occurrence("c", "bridge", "uid-1", "20260319").uuid
        );
        // Google calendar ids carry `#`, which the key escapes.
        let holiday = event("c", "en.usa#holiday@group.v.calendar.google.com", "x");
        assert_ne!(
            holiday.uuid,
            event("c", "en.usa", "holiday@group.v.calendar.google.com#x").uuid
        );
    }
}
