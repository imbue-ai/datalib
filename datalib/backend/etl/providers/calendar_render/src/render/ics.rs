//! One iCalendar object — a CalDAV resource, or one `UID` of an `.ics`
//! file — into events. The object holds the event, or a series and its
//! changed occurrences (`VEVENT`s sharing the `UID`, each with a
//! `RECURRENCE-ID`). A series is one document; each changed occurrence
//! is another; a cancelled occurrence is a line on the series.

use datalib_etl_calendar::ical::{self, Component, Property};
use datalib_etl_calendar::ingest::db::LoadedIcsObject;
use datalib_etl_calendar_common::{
    Attendee, EventLink, EventShape, EventTime, NormalizedEvent, OccurrenceRef, Person, SeriesRef,
};
use datalib_etl_render::inputs::Input;
use datalib_schema::problems::{Problem, Reason};

use super::ids;
use super::render::CalendarInfo;

pub fn normalize(
    source_id: &str,
    obj: &LoadedIcsObject,
    cal: &CalendarInfo,
) -> Vec<NormalizedEvent> {
    let vevents: Vec<Component> = ical::parse(&obj.ics)
        .into_iter()
        .flat_map(|c| c.children_named("VEVENT").cloned().collect::<Vec<_>>())
        .collect();
    let inputs = vec![
        Input::new("ics_objects", &obj.id),
        Input::new("calendars", &obj.calendar_id),
    ];
    let tz = cal.time_zone.as_deref();
    let master = vevents.iter().find(|v| v.prop("RECURRENCE-ID").is_none());
    let overrides: Vec<(&Component, EventTime)> = vevents
        .iter()
        .filter_map(|v| Some((v, time_of(v.prop("RECURRENCE-ID")?)?)))
        .collect();

    let series_id = ids::event(source_id, &obj.calendar_id, &obj.uid);
    let occurrence_id =
        |rid: &EventTime| ids::ics_occurrence(source_id, &obj.calendar_id, &obj.uid, &rid.key(tz));
    let is_cancelled = |v: &Component| status(v).as_deref() == Some("cancelled");

    let mut out = Vec::new();
    if let Some(m) = master {
        let rules: Vec<String> = m.props_named("RRULE").map(|p| p.value.clone()).collect();
        let rdates = times_in(m, "RDATE");
        let shape = if rules.is_empty() && rdates.is_empty() {
            EventShape::Single
        } else {
            let mut cancelled = times_in(m, "EXDATE");
            let mut changed = Vec::new();
            for (v, rid) in &overrides {
                if is_cancelled(v) {
                    cancelled.push(rid.clone());
                } else {
                    changed.push(OccurrenceRef {
                        uuid: occurrence_id(rid).uuid,
                        original_start: rid.clone(),
                        start: v.prop("DTSTART").and_then(time_of),
                        title: v.text("SUMMARY"),
                    });
                }
            }
            cancelled.sort_by_key(|t| t.key(tz));
            cancelled.dedup_by_key(|t| t.key(tz));
            EventShape::Series {
                rules,
                rdates,
                cancelled,
                changed,
            }
        };
        out.push(event(m, shape, &series_id, cal, inputs.clone()));
    }

    let series = master.map(|m| SeriesRef {
        uuid: series_id.uuid.clone(),
        title: m.text("SUMMARY"),
    });
    for (v, rid) in &overrides {
        // A cancelled occurrence of a series we hold is a line on it; of
        // one we do not, there is nothing to say.
        if is_cancelled(v) {
            continue;
        }
        let id = occurrence_id(rid);
        out.push(event(
            v,
            EventShape::Occurrence {
                series: series.clone(),
                original_start: rid.clone(),
            },
            &id,
            cal,
            inputs.clone(),
        ));
    }
    out
}

fn event(
    v: &Component,
    shape: EventShape,
    id: &datalib_id::Identity,
    cal: &CalendarInfo,
    inputs: Vec<Input>,
) -> NormalizedEvent {
    let mut problems = Vec::new();
    let start = match v.prop("DTSTART") {
        Some(p) => {
            let t = time_of(p);
            if t.is_none() {
                problems.push(Problem::field("DTSTART", Reason::CoercionFailed, &p.value));
            }
            t
        }
        None => None,
    };
    let end = v
        .prop("DTEND")
        .and_then(time_of)
        .or_else(|| end_from_duration(start.as_ref(), v.prop("DURATION")?));
    NormalizedEvent {
        event_uuid: id.uuid.clone(),
        shape,
        calendar_uuid: cal.uuid.clone(),
        calendar_label: cal.label.clone(),
        calendar_time_zone: cal.time_zone.clone(),
        upstream_id: id.natural_key.clone(),
        upstream_entity_kind: id.entity_kind,
        title: v.text("SUMMARY"),
        start,
        end,
        status: status(v),
        busy: v
            .prop("TRANSP")
            .map(|t| !t.value.trim().eq_ignore_ascii_case("TRANSPARENT")),
        location: v.text("LOCATION"),
        description: v.text("DESCRIPTION"),
        organizer: v.prop("ORGANIZER").map(person),
        attendees: v.props_named("ATTENDEE").map(attendee).collect(),
        links: links(v),
        source_url: None,
        created: v
            .prop("CREATED")
            .and_then(|p| datalib_time::coerce_record_stamp(p.value.trim())),
        modified_at: v
            .prop("LAST-MODIFIED")
            .or_else(|| v.prop("DTSTAMP"))
            .and_then(|p| datalib_time::coerce_record_stamp(p.value.trim())),
        inputs,
        problems,
    }
}

fn status(v: &Component) -> Option<String> {
    v.prop("STATUS")
        .map(|s| s.value.trim().to_ascii_lowercase())
}

fn time_of(p: &Property) -> Option<EventTime> {
    EventTime::from_ical(&p.value, p.param("TZID"))
}

/// Every time in a list-valued property (`EXDATE`, `RDATE`), which may
/// repeat and may hold several values; a `PERIOD` is not a start.
fn times_in(v: &Component, name: &str) -> Vec<EventTime> {
    v.props_named(name)
        .filter(|p| {
            !p.param("VALUE")
                .is_some_and(|t| t.eq_ignore_ascii_case("PERIOD"))
        })
        .flat_map(|p| {
            p.value
                .split(',')
                .filter_map(|one| EventTime::from_ical(one, p.param("TZID")))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `DTSTART` plus an RFC 5545 duration (`PT1H30M`, `P1D`, `P2W`).
fn end_from_duration(start: Option<&EventTime>, dur: &Property) -> Option<EventTime> {
    let d = dur.value.trim();
    let (neg, d) = match d.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, d.strip_prefix('+').unwrap_or(d)),
    };
    let d = d.strip_prefix('P')?;
    let (date_part, time_part) = d.split_once('T').unwrap_or((d, ""));
    let mut secs: i64 = 0;
    let mut take = |part: &str, units: &[(char, i64)]| -> Option<()> {
        let mut n = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                n.push(c);
            } else {
                let mult = units.iter().find(|(u, _)| *u == c)?.1;
                secs += n.parse::<i64>().ok()? * mult;
                n.clear();
            }
        }
        n.is_empty().then_some(())
    };
    take(date_part, &[('W', 604_800), ('D', 86_400)])?;
    take(time_part, &[('H', 3_600), ('M', 60), ('S', 1)])?;
    let delta = chrono::Duration::seconds(if neg { -secs } else { secs });
    match start? {
        EventTime::Date(day) => Some(EventTime::Date(*day + delta)),
        EventTime::At { local, zone } => Some(EventTime::At {
            local: *local + delta,
            zone: zone.clone(),
        }),
    }
}

fn person(p: &Property) -> Person {
    let email = p
        .value
        .trim()
        .strip_prefix("mailto:")
        .or_else(|| p.value.trim().strip_prefix("MAILTO:"))
        .map(str::to_string)
        .filter(|e| !e.is_empty());
    Person {
        name: p.param("CN").map(str::to_string).filter(|n| !n.is_empty()),
        email,
    }
}

fn attendee(p: &Property) -> Attendee {
    Attendee {
        person: person(p),
        response: p.param("PARTSTAT").map(str::to_ascii_lowercase),
        optional: p.param("ROLE").is_some_and(|r| {
            r.eq_ignore_ascii_case("OPT-PARTICIPANT") || r.eq_ignore_ascii_case("NON-PARTICIPANT")
        }),
        resource: p
            .param("CUTYPE")
            .is_some_and(|c| c.eq_ignore_ascii_case("ROOM") || c.eq_ignore_ascii_case("RESOURCE")),
    }
}

/// Attachments by link, the event's `URL`, and a video call where the
/// event names one (RFC 7986 `CONFERENCE`, Google's export extension).
/// An attachment inlined as binary is not a link.
fn links(v: &Component) -> Vec<EventLink> {
    let is_url = |s: &str| s.starts_with("https://") || s.starts_with("http://");
    let mut out = Vec::new();
    for p in v
        .props_named("CONFERENCE")
        .chain(v.props_named("X-GOOGLE-CONFERENCE"))
    {
        if is_url(p.value.trim()) {
            out.push(EventLink {
                label: p.param("LABEL").unwrap_or("Video call").to_string(),
                url: p.value.trim().to_string(),
            });
        }
    }
    for p in v.props_named("URL") {
        if is_url(p.value.trim()) {
            out.push(EventLink {
                label: "Link".into(),
                url: p.value.trim().to_string(),
            });
        }
    }
    for p in v.props_named("ATTACH") {
        if is_url(p.value.trim()) {
            out.push(EventLink {
                label: p
                    .param("FILENAME")
                    .or_else(|| p.param("X-FILENAME"))
                    .unwrap_or("Attachment")
                    .to_string(),
                url: p.value.trim().to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal() -> CalendarInfo {
        CalendarInfo {
            uuid: "cal".into(),
            label: "Bridge".into(),
            time_zone: Some("America/Los_Angeles".into()),
        }
    }

    fn obj(ics: &str) -> LoadedIcsObject {
        LoadedIcsObject {
            id: "bridge#staff".into(),
            calendar_id: "bridge".into(),
            uid: "staff".into(),
            ics: ics.into(),
        }
    }

    const SERIES: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:staff\r\nSUMMARY:Briefing\r\nDTSTART;TZID=America/Los_Angeles:20260105T090000\r\nDURATION:PT1H30M\r\nRRULE:FREQ=WEEKLY;BYDAY=MO\r\nEXDATE;TZID=America/Los_Angeles:20260119T090000,20260126T090000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:staff\r\nRECURRENCE-ID:20260209T170000Z\r\nSUMMARY:Briefing (moved)\r\nDTSTART;TZID=America/Los_Angeles:20260209T110000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:staff\r\nRECURRENCE-ID;TZID=America/Los_Angeles:20260216T090000\r\nSTATUS:CANCELLED\r\nDTSTART;TZID=America/Los_Angeles:20260216T090000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn a_series_with_a_moved_and_a_cancelled_occurrence_is_two_documents() {
        let events = normalize("tng_calendar", &obj(SERIES), &cal());
        assert_eq!(events.len(), 2, "the series and its one moved occurrence");
        let EventShape::Series {
            cancelled, changed, ..
        } = &events[0].shape
        else {
            panic!("series first");
        };
        // Two EXDATEs in one property and one cancelled override.
        assert_eq!(cancelled.len(), 3);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].uuid, events[1].event_uuid);
        assert_eq!(
            events[0].end,
            EventTime::from_ical("20260105T103000", Some("America/Los_Angeles"))
        );

        let EventShape::Occurrence {
            series,
            original_start,
        } = &events[1].shape
        else {
            panic!("occurrence second");
        };
        assert_eq!(series.as_ref().unwrap().uuid, events[0].event_uuid);
        assert_eq!(original_start.key(None), "20260209T170000Z");
        assert_eq!(events[1].upstream_entity_kind, ids::KIND_OCCURRENCE);
    }

    /// A `RECURRENCE-ID` written in UTC and one written in the series'
    /// zone name the same occurrence, and mint the same id.
    #[test]
    fn an_occurrence_id_does_not_depend_on_how_its_recurrence_id_was_spelled() {
        let utc = normalize("s", &obj(SERIES), &cal());
        let zoned = SERIES.replace(
            "RECURRENCE-ID:20260209T170000Z",
            "RECURRENCE-ID;TZID=America/Los_Angeles:20260209T090000",
        );
        let zoned = normalize("s", &obj(&zoned), &cal());
        assert_eq!(utc[1].event_uuid, zoned[1].event_uuid);
    }

    #[test]
    fn an_invitation_to_one_occurrence_stands_alone() {
        let orphan = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:lecture\r\nRECURRENCE-ID;TZID=America/Los_Angeles:20261005T140000\r\nSUMMARY:Guest lecture\r\nDTSTART;TZID=America/Los_Angeles:20261005T140000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = normalize("s", &obj(orphan), &cal());
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].shape,
            EventShape::Occurrence { series: None, .. }
        ));
    }

    #[test]
    fn people_links_and_free_time_are_read() {
        let ev = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20260713\r\nTRANSP:TRANSPARENT\r\n\
ORGANIZER;CN=Deanna Troi:mailto:troi@enterprise.test\r\n\
ATTENDEE;CN=Guinan;PARTSTAT=DECLINED;ROLE=OPT-PARTICIPANT:mailto:guinan@enterprise.test\r\n\
ATTENDEE;CUTYPE=ROOM;CN=Ten Forward:mailto:ten-forward@enterprise.test\r\n\
ATTACH;FILENAME=Menu.pdf:https://files.enterprise.test/menu.pdf\r\nATTACH;ENCODING=BASE64;VALUE=BINARY:AAAA\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let e = &normalize("s", &obj(ev), &cal())[0];
        assert!(matches!(e.shape, EventShape::Single));
        assert_eq!(e.busy, Some(false));
        assert_eq!(
            e.organizer.as_ref().unwrap().name.as_deref(),
            Some("Deanna Troi")
        );
        assert!(e.attendees[0].optional);
        assert_eq!(e.attendees[0].response.as_deref(), Some("declined"));
        assert!(e.attendees[1].resource);
        assert_eq!(e.links.len(), 1);
        assert_eq!(e.links[0].label, "Menu.pdf");
    }

    #[test]
    fn an_unreadable_start_is_a_problem_not_a_guess() {
        let ev = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:soon\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let e = &normalize("s", &obj(ev), &cal())[0];
        assert_eq!(e.start, None);
        assert_eq!(e.problems.len(), 1);
    }
}
