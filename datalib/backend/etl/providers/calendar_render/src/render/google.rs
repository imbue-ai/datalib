//! Google Calendar events into the shared model. Google lists a series
//! once, with its rule in `recurrence`, and each changed or cancelled
//! occurrence as its own resource naming the series in
//! `recurringEventId` — so the grouping iCalendar does in one object is
//! done here across rows.

use std::collections::HashMap;

use datalib_etl_calendar::ical;
use datalib_etl_calendar::ingest::db::LoadedGoogleEvent;
use datalib_etl_calendar_common::{
    Attendee, EventLink, EventShape, EventTime, NormalizedEvent, OccurrenceRef, Person, SeriesRef,
};
use datalib_etl_render::inputs::Input;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;

use super::ids;
use super::render::CalendarInfo;

/// Every event of one calendar.
pub fn normalize(
    source_id: &str,
    events: &[&LoadedGoogleEvent],
    cal: &CalendarInfo,
) -> Vec<NormalizedEvent> {
    let mut by_series: HashMap<&str, Vec<&LoadedGoogleEvent>> = HashMap::new();
    for e in events {
        if let Some(series) = str_of(&e.event, "recurringEventId") {
            by_series.entry(series).or_default().push(e);
        }
    }
    let masters: HashMap<&str, &LoadedGoogleEvent> = events
        .iter()
        .filter(|e| str_of(&e.event, "recurringEventId").is_none())
        .filter_map(|e| Some((str_of(&e.event, "id")?, *e)))
        .collect();

    let mut out = Vec::new();
    for e in events {
        let Some(id) = str_of(&e.event, "id") else {
            continue;
        };
        let cancelled = str_of(&e.event, "status") == Some("cancelled");
        match str_of(&e.event, "recurringEventId") {
            None => {
                let occurrences = by_series.get(id).map(Vec::as_slice).unwrap_or_default();
                out.push(master(source_id, e, id, occurrences, cal));
            }
            Some(series_id) if !cancelled => {
                let series = masters.get(series_id).copied();
                out.push(occurrence(source_id, e, id, series, cal));
            }
            // A cancelled occurrence is a line on its series.
            Some(_) => {}
        }
    }
    out
}

fn master(
    source_id: &str,
    e: &LoadedGoogleEvent,
    id: &str,
    occurrences: &[&LoadedGoogleEvent],
    cal: &CalendarInfo,
) -> NormalizedEvent {
    let identity = ids::event(source_id, &e.calendar_id, id);
    let (rules, rdates, exdates) = recurrence(&e.event);
    let mut inputs = vec![
        Input::new("google_events", &e.id),
        Input::new("calendars", &e.calendar_id),
    ];
    let shape = if rules.is_empty() && rdates.is_empty() {
        EventShape::Single
    } else {
        let mut cancelled = exdates;
        let mut changed = Vec::new();
        let tz = cal.time_zone.as_deref();
        for o in occurrences {
            inputs.push(Input::new("google_events", &o.id));
            let Some(original) = time_at(&o.event, "originalStartTime") else {
                continue;
            };
            if str_of(&o.event, "status") == Some("cancelled") {
                cancelled.push(original);
            } else if let Some(oid) = str_of(&o.event, "id") {
                changed.push(OccurrenceRef {
                    uuid: ids::google_occurrence(source_id, &o.calendar_id, oid).uuid,
                    original_start: original,
                    start: time_at(&o.event, "start"),
                    title: text(&o.event, "summary"),
                });
            }
        }
        cancelled.sort_by_key(|t| t.key(tz));
        cancelled.dedup_by_key(|t| t.key(tz));
        changed.sort_by_key(|c| c.original_start.key(tz));
        EventShape::Series {
            rules,
            rdates,
            cancelled,
            changed,
        }
    };
    event(e, shape, &identity, cal, inputs)
}

fn occurrence(
    source_id: &str,
    e: &LoadedGoogleEvent,
    id: &str,
    series: Option<&LoadedGoogleEvent>,
    cal: &CalendarInfo,
) -> NormalizedEvent {
    let identity = ids::google_occurrence(source_id, &e.calendar_id, id);
    let mut inputs = vec![
        Input::new("google_events", &e.id),
        Input::new("calendars", &e.calendar_id),
    ];
    let series = series.and_then(|s| {
        inputs.push(Input::new("google_events", &s.id));
        Some(SeriesRef {
            uuid: ids::event(source_id, &s.calendar_id, str_of(&s.event, "id")?).uuid,
            title: text(&s.event, "summary"),
        })
    });
    let original = time_at(&e.event, "originalStartTime");
    let mut ev = match original {
        Some(original_start) => event(
            e,
            EventShape::Occurrence {
                series,
                original_start,
            },
            &identity,
            cal,
            inputs,
        ),
        // No original start: all that is left to say is the event.
        None => event(e, EventShape::Single, &identity, cal, inputs),
    };
    if ev.title.is_none() {
        ev.title = series_title(&ev);
    }
    ev
}

fn series_title(e: &NormalizedEvent) -> Option<String> {
    match &e.shape {
        EventShape::Occurrence {
            series: Some(s), ..
        } => s.title.clone(),
        _ => None,
    }
}

fn event(
    e: &LoadedGoogleEvent,
    shape: EventShape,
    id: &datalib_id::Identity,
    cal: &CalendarInfo,
    inputs: Vec<Input>,
) -> NormalizedEvent {
    let v = &e.event;
    let mut problems = Vec::new();
    let start = time_at(v, "start");
    if start.is_none() && v.get("start").is_some() {
        problems.push(Problem::field(
            "start",
            Reason::CoercionFailed,
            &v["start"].to_string(),
        ));
    }
    NormalizedEvent {
        event_uuid: id.uuid.clone(),
        shape,
        calendar_uuid: cal.uuid.clone(),
        calendar_label: cal.label.clone(),
        calendar_time_zone: cal.time_zone.clone(),
        upstream_id: id.natural_key.clone(),
        upstream_entity_kind: id.entity_kind,
        title: text(v, "summary"),
        start,
        end: time_at(v, "end"),
        status: str_of(v, "status").map(str::to_ascii_lowercase),
        busy: Some(str_of(v, "transparency") != Some("transparent")),
        location: text(v, "location"),
        description: str_of(v, "description")
            .map(html_to_text)
            .filter(|d| !d.trim().is_empty()),
        organizer: v.get("organizer").and_then(person),
        attendees: v
            .get("attendees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(attendee)
            .collect(),
        links: links(v),
        source_url: str_of(v, "htmlLink").map(str::to_string),
        created: str_of(v, "created").and_then(datalib_time::coerce_record_stamp),
        modified_at: str_of(v, "updated").and_then(datalib_time::coerce_record_stamp),
        inputs,
        problems,
    }
}

/// `recurrence` holds iCalendar lines: `RRULE:…`, `EXDATE;TZID=…:…`,
/// `RDATE…`.
fn recurrence(v: &Value) -> (Vec<String>, Vec<EventTime>, Vec<EventTime>) {
    let mut rules = Vec::new();
    let mut rdates = Vec::new();
    let mut exdates = Vec::new();
    for line in v
        .get("recurrence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let Some(p) = ical::parse_line(line) else {
            continue;
        };
        let times = || {
            p.value
                .split(',')
                .filter_map(|one| EventTime::from_ical(one, p.param("TZID")))
                .collect::<Vec<_>>()
        };
        match p.name.as_str() {
            "RRULE" => rules.push(p.value.clone()),
            "RDATE" => rdates.extend(times()),
            "EXDATE" => exdates.extend(times()),
            _ => {}
        }
    }
    (rules, rdates, exdates)
}

fn time_at(v: &Value, key: &str) -> Option<EventTime> {
    let t = v.get(key)?;
    EventTime::from_google(
        str_of(t, "date"),
        str_of(t, "dateTime"),
        str_of(t, "timeZone"),
    )
}

fn person(p: &Value) -> Option<Person> {
    let name = text(p, "displayName");
    let email = text(p, "email");
    (name.is_some() || email.is_some()).then_some(Person { name, email })
}

fn attendee(a: &Value) -> Option<Attendee> {
    Some(Attendee {
        person: person(a)?,
        response: str_of(a, "responseStatus").map(|r| match r {
            "needsAction" => "needs-action".to_string(),
            other => other.to_ascii_lowercase(),
        }),
        optional: a.get("optional").and_then(Value::as_bool) == Some(true),
        resource: a.get("resource").and_then(Value::as_bool) == Some(true),
    })
}

fn links(v: &Value) -> Vec<EventLink> {
    let mut out: Vec<EventLink> = Vec::new();
    let solution = v
        .pointer("/conferenceData/conferenceSolution/name")
        .and_then(Value::as_str)
        .unwrap_or("Video call");
    for ep in v
        .pointer("/conferenceData/entryPoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if str_of(ep, "entryPointType") == Some("video") {
            if let Some(uri) = str_of(ep, "uri") {
                out.push(EventLink {
                    label: solution.to_string(),
                    url: uri.to_string(),
                });
            }
        }
    }
    if let Some(meet) = str_of(v, "hangoutLink") {
        if !out.iter().any(|l| l.url == meet) {
            out.push(EventLink {
                label: "Google Meet".into(),
                url: meet.to_string(),
            });
        }
    }
    for a in v
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = str_of(a, "fileUrl") {
            out.push(EventLink {
                label: str_of(a, "title").unwrap_or("Attachment").to_string(),
                url: url.to_string(),
            });
        }
    }
    out
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn text(v: &Value, key: &str) -> Option<String> {
    str_of(v, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Google stores a description as the HTML its editor wrote. Line
/// breaks and list items become lines; a link keeps its address where
/// the text did not already show it; every other tag goes.
pub fn html_to_text(html: &str) -> String {
    if !html.contains('<') && !html.contains('&') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut href: Option<String> = None;
    let mut link_text = String::new();
    while let Some(i) = rest.find('<') {
        let text = decode_entities(&rest[..i]);
        out.push_str(&text);
        if href.is_some() {
            link_text.push_str(&text);
        }
        let Some(j) = rest[i..].find('>') else {
            out.push_str(&decode_entities(&rest[i..]));
            rest = "";
            break;
        };
        let tag = &rest[i + 1..i + j];
        rest = &rest[i + j + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');
        match (name.as_str(), closing) {
            ("br", _) | ("p" | "div" | "tr" | "ul" | "ol", true) => out.push('\n'),
            ("li", false) => out.push_str("\n- "),
            ("a", false) => {
                href = attr(tag, "href");
                link_text.clear();
            }
            ("a", true) => {
                if let Some(h) = href.take() {
                    if !link_text.contains(h.trim_start_matches("mailto:")) {
                        out.push_str(&format!(" ({h})"));
                    }
                }
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let mut joined = lines.join("\n");
    while joined.contains("\n\n\n") {
        joined = joined.replace("\n\n\n", "\n\n");
    }
    joined.trim().to_string()
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let (quote, body) = match rest.chars().next()? {
        q @ ('"' | '\'') => (q, &rest[1..]),
        _ => (' ', rest),
    };
    let end = body.find(quote).unwrap_or(body.len());
    Some(decode_entities(&body[..end]))
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cal() -> CalendarInfo {
        CalendarInfo {
            uuid: "cal".into(),
            label: "picard@enterprise.test".into(),
            time_zone: Some("America/Los_Angeles".into()),
        }
    }

    fn row(event: Value) -> LoadedGoogleEvent {
        LoadedGoogleEvent {
            id: format!("picard@enterprise.test#{}", event["id"].as_str().unwrap()),
            calendar_id: "picard@enterprise.test".into(),
            event,
        }
    }

    /// The shapes Google's documentation gives for `singleEvents=false`:
    /// the series with its rule, a moved occurrence, and a cancelled
    /// one reduced to its id, status and original start.
    fn staff_series() -> Vec<LoadedGoogleEvent> {
        vec![
            row(json!({
                "id": "staff01", "status": "confirmed", "summary": "Senior staff briefing",
                "htmlLink": "https://www.google.com/calendar/event?eid=c3RhZmYwMQ",
                "created": "2026-01-05T18:00:00.000Z", "updated": "2026-09-02T16:00:00.000Z",
                "start": {"dateTime": "2026-01-05T09:00:00-08:00", "timeZone": "America/Los_Angeles"},
                "end": {"dateTime": "2026-01-05T10:00:00-08:00", "timeZone": "America/Los_Angeles"},
                "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=20261231T170000Z",
                               "EXDATE;TZID=America/Los_Angeles:20260209T090000"],
                "organizer": {"email": "picard@enterprise.test", "displayName": "Jean-Luc Picard", "self": true},
                "attendees": [
                    {"email": "riker@enterprise.test", "displayName": "William Riker", "responseStatus": "accepted"},
                    {"email": "laforge@enterprise.test", "responseStatus": "needsAction", "optional": true}
                ],
                "conferenceData": {"entryPoints": [{"entryPointType": "video", "uri": "https://meet.google.com/ncc-1701-d"}],
                                   "conferenceSolution": {"name": "Google Meet"}},
                "hangoutLink": "https://meet.google.com/ncc-1701-d"
            })),
            row(json!({
                "id": "staff01_20260312T170000Z", "status": "confirmed", "recurringEventId": "staff01",
                "summary": "Senior staff briefing — Borg incursion",
                "originalStartTime": {"dateTime": "2026-03-12T09:00:00-08:00", "timeZone": "America/Los_Angeles"},
                "start": {"dateTime": "2026-03-12T11:00:00-07:00", "timeZone": "America/Los_Angeles"},
                "end": {"dateTime": "2026-03-12T12:30:00-07:00", "timeZone": "America/Los_Angeles"}
            })),
            row(json!({
                "id": "staff01_20260402T160000Z", "status": "cancelled", "recurringEventId": "staff01",
                "originalStartTime": {"dateTime": "2026-04-02T09:00:00-07:00", "timeZone": "America/Los_Angeles"}
            })),
        ]
    }

    #[test]
    fn a_series_gathers_its_occurrences_from_their_own_rows() {
        let rows = staff_series();
        let refs: Vec<&LoadedGoogleEvent> = rows.iter().collect();
        let events = normalize("tng_google", &refs, &cal());
        assert_eq!(events.len(), 2, "the series and the moved occurrence");
        let series = &events[0];
        let EventShape::Series {
            rules,
            cancelled,
            changed,
            ..
        } = &series.shape
        else {
            panic!("series");
        };
        assert_eq!(rules, &["FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=20261231T170000Z"]);
        assert_eq!(cancelled.len(), 2, "the EXDATE and the cancelled row");
        assert_eq!(changed[0].uuid, events[1].event_uuid);
        assert_eq!(
            series.inputs.len(),
            4,
            "its row, its calendar, both occurrences"
        );
        assert_eq!(series.links.len(), 1, "the Meet link once");
        assert_eq!(
            series.attendees[1].response.as_deref(),
            Some("needs-action")
        );
        assert!(series.attendees[1].optional);

        let EventShape::Occurrence { series: of, .. } = &events[1].shape else {
            panic!("occurrence");
        };
        assert_eq!(of.as_ref().unwrap().uuid, series.event_uuid);
        assert_eq!(
            events[1]
                .start
                .as_ref()
                .unwrap()
                .instant(None)
                .unwrap()
                .at
                .to_rfc3339(),
            "2026-03-12T11:00:00-07:00"
        );
    }

    #[test]
    fn descriptions_lose_their_markup_but_not_their_lines_or_links() {
        assert_eq!(
            html_to_text("Agenda:<br><ul><li>Saucer <b>separation</b></li><li>Drill</li></ul><a href=\"https://memory-alpha.test/x\">notes</a> &amp; more"),
            "Agenda:\n\n- Saucer separation\n- Drill\nnotes (https://memory-alpha.test/x) & more"
        );
        assert_eq!(
            html_to_text("<a href=\"https://meet.test/a\">https://meet.test/a</a>"),
            "https://meet.test/a"
        );
        assert_eq!(html_to_text("plain\ntext"), "plain\ntext");
    }
}
