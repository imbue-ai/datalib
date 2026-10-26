//! Calendly scheduled events into the shared model. A Calendly event
//! happens once, at a UTC instant; its hosts are `event_memberships`,
//! its invitees a list of their own (`calendly_invitees`), each with
//! what they answered on the booking form, and anyone an invitee
//! brought along is in `event_guests`.

use datalib_etl_calendar::ingest::db::LoadedCalendlyEvent;
use datalib_etl_calendar_common::{
    Attendee, EventLink, EventShape, EventTime, GuestAnswers, NormalizedEvent, Person,
    QuestionAnswer,
};
use datalib_etl_render::inputs::Input;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;

use super::ids;
use super::render::CalendarInfo;

/// One document per event. `owner` is the signed-in user's URI, the host
/// the page names as organizer when the event has several.
pub fn normalize(
    source_id: &str,
    e: &LoadedCalendlyEvent,
    cal: &CalendarInfo,
    owner: Option<&str>,
) -> Option<NormalizedEvent> {
    let v = &e.event;
    let uuid =
        str_of(v, "uri").and_then(datalib_etl_calendar::ingest::schema_raw::calendly_uuid)?;
    let identity = ids::event(source_id, &e.calendar_id, uuid);
    let tz = cal.time_zone.as_deref();
    let at = |key: &str| str_of(v, key).and_then(|t| EventTime::from_google(None, Some(t), tz));
    let mut problems = Vec::new();
    let start = at("start_time");
    if start.is_none() {
        let sample = v.get("start_time").map(Value::to_string);
        problems.push(Problem::field(
            "start_time",
            Reason::CoercionFailed,
            sample.as_deref().unwrap_or("missing"),
        ));
    }
    let (organizer, mut attendees) = hosts(v, owner);
    let invitees = e.invitees.as_deref().unwrap_or_default();
    attendees.extend(invitees.iter().filter_map(invitee));
    attendees.extend(guests(v, &attendees));
    let (location, links) = place(v);
    Some(NormalizedEvent {
        event_uuid: identity.uuid.clone(),
        shape: EventShape::Single,
        calendar_uuid: cal.uuid.clone(),
        calendar_label: cal.label.clone(),
        calendar_time_zone: cal.time_zone.clone(),
        upstream_id: identity.natural_key.clone(),
        upstream_entity_kind: identity.entity_kind,
        title: text(v, "name"),
        start,
        end: at("end_time"),
        status: str_of(v, "status").map(status),
        busy: Some(true),
        location,
        description: description(v),
        organizer,
        attendees,
        links,
        answers: invitees.iter().filter_map(answers).collect(),
        // Calendly documents no page of its own for one scheduled event.
        source_url: None,
        created: str_of(v, "created_at").and_then(datalib_time::coerce_record_stamp),
        modified_at: str_of(v, "updated_at").and_then(datalib_time::coerce_record_stamp),
        inputs: vec![
            Input::new("calendly_events", &e.id),
            Input::new("calendly_invitees", &e.id),
            Input::new("calendars", &e.calendar_id),
        ],
        problems,
    })
}

/// `canceled` is Calendly's spelling; the page says `cancelled`, as it
/// does for every other calendar.
fn status(s: &str) -> String {
    match s {
        "active" => "confirmed".into(),
        "canceled" => "cancelled".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// The organizer, and the other hosts as attendees who said yes. The
/// organizer is the signed-in user when they host, else the first host.
fn hosts(v: &Value, owner: Option<&str>) -> (Option<Person>, Vec<Attendee>) {
    let members: Vec<&Value> = v
        .get("event_memberships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let lead = members
        .iter()
        .position(|m| owner.is_some() && str_of(m, "user") == owner)
        .unwrap_or(0);
    let person = |m: &Value| {
        let p = Person {
            name: text(m, "user_name"),
            email: text(m, "user_email"),
        };
        (p.name.is_some() || p.email.is_some()).then_some(p)
    };
    let organizer = members.get(lead).and_then(|m| person(m));
    let others = members
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != lead)
        .filter_map(|(_, m)| person(m))
        .map(|p| Attendee {
            person: p,
            response: Some("accepted".into()),
            optional: false,
            resource: false,
        })
        .collect();
    (organizer, others)
}

/// An invitee who booked is coming; one who cancelled is not.
fn invitee(i: &Value) -> Option<Attendee> {
    let person = invitee_person(i)?;
    Some(Attendee {
        person,
        response: match str_of(i, "status") {
            Some("active") => Some("accepted".into()),
            Some("canceled") => Some("declined".into()),
            _ => None,
        },
        optional: false,
        resource: false,
    })
}

fn invitee_person(i: &Value) -> Option<Person> {
    let name = text(i, "name").or_else(|| {
        let parts: Vec<String> = ["first_name", "last_name"]
            .iter()
            .filter_map(|k| text(i, k))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    });
    let email = text(i, "email");
    (name.is_some() || email.is_some()).then_some(Person { name, email })
}

/// The people an invitee added, as an address each, unless they are on
/// the list already.
fn guests(v: &Value, already: &[Attendee]) -> Vec<Attendee> {
    v.get("event_guests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| text(g, "email"))
        .filter(|email| {
            !already.iter().any(|a| {
                a.person
                    .email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
        })
        .map(|email| Attendee {
            person: Person {
                name: None,
                email: Some(email),
            },
            response: None,
            optional: false,
            resource: false,
        })
        .collect()
}

fn answers(i: &Value) -> Option<GuestAnswers> {
    let mut qas: Vec<(i64, QuestionAnswer)> = i
        .get("questions_and_answers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|qa| {
            Some((
                qa.get("position")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX),
                QuestionAnswer {
                    question: text(qa, "question")?,
                    answer: str_of(qa, "answer").unwrap_or_default().to_string(),
                },
            ))
        })
        .collect();
    if qas.is_empty() {
        return None;
    }
    qas.sort_by_key(|(position, _)| *position);
    Some(GuestAnswers {
        person: invitee_person(i).unwrap_or(Person {
            name: None,
            email: None,
        }),
        answers: qas.into_iter().map(|(_, qa)| qa).collect(),
    })
}

/// Where the meeting is, as text, and the link to join it. A call's
/// `location` is a phone number; a video call's is its link.
fn place(v: &Value) -> (Option<String>, Vec<EventLink>) {
    let Some(loc) = v.get("location").filter(|l| l.is_object()) else {
        return (None, Vec::new());
    };
    let kind = str_of(loc, "type").unwrap_or("");
    let said = text(loc, "location");
    let location = match kind {
        "inbound_call" | "outbound_call" => said.map(|n| format!("Phone call: {n}")),
        _ => said,
    };
    let label = match kind {
        "zoom" => "Zoom",
        "google_conference" => "Google Meet",
        "microsoft_teams_conference" => "Microsoft Teams",
        "webex_conference" => "Webex",
        "gotomeeting" => "GoTo Meeting",
        _ => "Video call",
    };
    let links = text(loc, "join_url")
        .map(|url| EventLink {
            label: label.to_string(),
            url,
        })
        .into_iter()
        .collect();
    (location, links)
}

/// The host's notes, then who cancelled and why.
fn description(v: &Value) -> Option<String> {
    let mut parts: Vec<String> = text(v, "meeting_notes_plain").into_iter().collect();
    if let Some(c) = v.get("cancellation").filter(|c| c.is_object()) {
        let who = text(c, "canceled_by");
        let why = text(c, "reason");
        parts.push(match (who, why) {
            (Some(who), Some(why)) => format!("Cancelled by {who}: {why}"),
            (Some(who), None) => format!("Cancelled by {who}."),
            (None, Some(why)) => format!("Cancelled: {why}"),
            (None, None) => "Cancelled.".into(),
        });
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: &str = "https://api.calendly.com/users/PICARD";

    fn cal() -> CalendarInfo {
        CalendarInfo {
            uuid: "cal".into(),
            label: "Calendly".into(),
            time_zone: Some("America/Los_Angeles".into()),
        }
    }

    /// The shape Calendly's API reference gives for a scheduled event
    /// and its invitees; see `INGEST.md`.
    fn counseling() -> LoadedCalendlyEvent {
        LoadedCalendlyEvent {
            id: "PICARD#EV1".into(),
            calendar_id: "PICARD".into(),
            event: json!({
                "uri": "https://api.calendly.com/scheduled_events/EV1",
                "name": "Counseling session",
                "status": "active",
                "start_time": "2026-11-02T17:00:00.000000Z",
                "end_time": "2026-11-02T17:30:00.000000Z",
                "event_type": "https://api.calendly.com/event_types/ET1",
                "location": {"type": "zoom", "status": "pushed",
                             "join_url": "https://zoom.example/j/1701", "data": null},
                "invitees_counter": {"total": 2, "active": 1, "limit": 2},
                "created_at": "2026-10-20T09:00:00.000000Z",
                "updated_at": "2026-10-21T09:00:00.000000Z",
                "meeting_notes_plain": "Bring the tea.",
                "event_memberships": [
                    {"user": "https://api.calendly.com/users/RIKER",
                     "user_email": "riker@enterprise.test", "user_name": "William Riker"},
                    {"user": OWNER, "user_email": "troi@enterprise.test", "user_name": "Deanna Troi"}
                ],
                "event_guests": [
                    {"email": "barclay@enterprise.test", "created_at": "2026-10-20T09:00:00Z",
                     "updated_at": "2026-10-20T09:00:00Z"},
                    {"email": "data@enterprise.test", "created_at": "2026-10-20T09:00:00Z",
                     "updated_at": "2026-10-20T09:00:00Z"}
                ],
                "calendar_event": null
            }),
            invitees: Some(vec![
                json!({"uri": "https://api.calendly.com/scheduled_events/EV1/invitees/IN1",
                "email": "barclay@enterprise.test", "name": "Reginald Barclay",
                "first_name": null, "last_name": null, "status": "active",
                "questions_and_answers": [
                    {"question": "Anything else?", "answer": "No.", "position": 1},
                    {"question": "What would you like to discuss?",
                     "answer": "The holodeck.", "position": 0}
                ]}),
                json!({"uri": "https://api.calendly.com/scheduled_events/EV1/invitees/IN2",
                       "email": "obrien@enterprise.test", "name": "",
                       "first_name": "Miles", "last_name": "O'Brien", "status": "canceled",
                       "questions_and_answers": []}),
            ]),
        }
    }

    #[test]
    fn an_event_reads_as_a_meeting_with_its_people_and_their_answers() {
        let e = normalize("tng_calendly", &counseling(), &cal(), Some(OWNER)).unwrap();
        assert_eq!(e.title.as_deref(), Some("Counseling session"));
        assert_eq!(e.status.as_deref(), Some("confirmed"));
        assert_eq!(
            e.organizer.as_ref().and_then(|o| o.name.as_deref()),
            Some("Deanna Troi"),
            "the signed-in user hosts"
        );
        let who: Vec<(&str, Option<&str>)> = e
            .attendees
            .iter()
            .map(|a| (a.person.label().unwrap_or(""), a.response.as_deref()))
            .collect();
        assert_eq!(
            who,
            vec![
                ("William Riker", Some("accepted")),
                ("Reginald Barclay", Some("accepted")),
                ("Miles O'Brien", Some("declined")),
                ("data@enterprise.test", None),
            ],
            "a guest already invited is not listed twice"
        );
        assert_eq!(e.answers.len(), 1, "only who answered something");
        let questions: Vec<&str> = e.answers[0]
            .answers
            .iter()
            .map(|qa| qa.question.as_str())
            .collect();
        assert_eq!(
            questions,
            vec!["What would you like to discuss?", "Anything else?"]
        );
        assert_eq!(e.links[0].label, "Zoom");
        assert_eq!(e.location, None);
        assert_eq!(e.description.as_deref(), Some("Bring the tea."));
        assert_eq!(
            e.start
                .as_ref()
                .unwrap()
                .instant(None)
                .unwrap()
                .at
                .to_rfc3339(),
            "2026-11-02T09:00:00-08:00",
            "the UTC start, on the host's clock"
        );
        assert_eq!(e.inputs.len(), 3);
    }

    #[test]
    fn a_cancelled_event_says_who_cancelled_and_why() {
        let mut row = counseling();
        row.event["status"] = json!("canceled");
        row.event["cancellation"] = json!({"canceled_by": "Reginald Barclay",
            "reason": "Holodeck malfunction", "canceler_type": "invitee",
            "created_at": "2026-10-22T09:00:00Z"});
        row.event["location"] = json!({"type": "outbound_call", "location": "+1 555 0101"});
        row.invitees = None;
        let e = normalize("tng_calendly", &row, &cal(), Some(OWNER)).unwrap();
        assert_eq!(e.status.as_deref(), Some("cancelled"));
        assert_eq!(
            e.description.as_deref(),
            Some("Bring the tea.\n\nCancelled by Reginald Barclay: Holodeck malfunction")
        );
        assert_eq!(e.location.as_deref(), Some("Phone call: +1 555 0101"));
        assert!(e.links.is_empty());
        assert!(e.answers.is_empty(), "no invitees fetched yet");
    }
}
