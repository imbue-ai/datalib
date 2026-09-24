//! The provider-agnostic event model [`crate::render`] consumes.

use crate::when::EventTime;

/// One document's worth of event: a one-off event, a recurring series,
/// or one changed occurrence of a series. Built by the provider's
/// render stage; the renderer never reaches back into provider rows.
#[derive(Debug, Clone)]
pub struct NormalizedEvent {
    /// The document's `markdown_uuid` and its grid row's `uuid`.
    pub event_uuid: String,
    pub shape: EventShape,
    /// The calendar's datalib id, and its name as the service shows it.
    pub calendar_uuid: String,
    pub calendar_label: String,
    /// The zone a floating time on this calendar is in, where known.
    pub calendar_time_zone: Option<String>,
    /// `grid_rows.upstream_id` / `upstream_entity_kind`: exactly what
    /// the provider minted `event_uuid` from.
    pub upstream_id: String,
    pub upstream_entity_kind: &'static str,
    pub title: Option<String>,
    pub start: Option<EventTime>,
    pub end: Option<EventTime>,
    /// `confirmed`, `tentative` or `cancelled`, lower-cased.
    pub status: Option<String>,
    /// False for an event marked free (`TRANSP:TRANSPARENT`, Google's
    /// `transparency: transparent`).
    pub busy: Option<bool>,
    pub location: Option<String>,
    /// Plain text; a provider whose upstream sends HTML converts it.
    pub description: Option<String>,
    pub organizer: Option<Person>,
    pub attendees: Vec<Attendee>,
    /// A video call, an attachment, a link the event carries.
    pub links: Vec<EventLink>,
    /// The event's page on the service, where it has one.
    pub source_url: Option<String>,
    /// When the event was put on the calendar, as the source wrote it.
    /// Shown in the document; the grid's `created_at` is the start.
    pub created: Option<String>,
    /// When the event last changed, RFC 3339 with its offset.
    pub modified_at: Option<String>,
    /// Every raw row this document was built from, for the render
    /// driver's incrementality.
    pub inputs: Vec<datalib_etl_render::inputs::Input>,
    /// What the provider could not read of the event while normalizing
    /// it; each becomes a problem row on the document.
    pub problems: Vec<datalib_schema::problems::Problem>,
}

#[derive(Debug, Clone)]
pub enum EventShape {
    /// Happens once.
    Single,
    /// Repeats. Its occurrences are not documents of their own; the
    /// rule, and the occurrences that differ from it, are on this one.
    Series {
        /// `RRULE` values, as written (`FREQ=WEEKLY;BYDAY=MO`).
        rules: Vec<String>,
        /// Extra dates the rule does not produce (`RDATE`).
        rdates: Vec<EventTime>,
        /// Occurrences the series no longer has: `EXDATE`s, and
        /// occurrences cancelled one at a time.
        cancelled: Vec<EventTime>,
        /// Occurrences that were moved or edited, each its own document.
        changed: Vec<OccurrenceRef>,
    },
    /// One occurrence of a series, changed from what the rule says.
    Occurrence {
        /// `None` when the series itself is not on this calendar: an
        /// invitation to one date of someone else's series.
        series: Option<SeriesRef>,
        /// The start the rule gave it (`RECURRENCE-ID`).
        original_start: EventTime,
    },
}

/// A changed occurrence, as its series lists it.
#[derive(Debug, Clone)]
pub struct OccurrenceRef {
    pub uuid: String,
    pub original_start: EventTime,
    pub start: Option<EventTime>,
    pub title: Option<String>,
}

/// The series a changed occurrence belongs to.
#[derive(Debug, Clone)]
pub struct SeriesRef {
    pub uuid: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl Person {
    /// The name, else the address; `None` when neither is known.
    pub fn label(&self) -> Option<&str> {
        self.name.as_deref().or(self.email.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    pub person: Person,
    /// `accepted`, `declined`, `tentative`, `needs-action`, lower-cased.
    pub response: Option<String>,
    /// True for an attendee whose presence is optional.
    pub optional: bool,
    /// A room or other resource rather than a person.
    pub resource: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLink {
    pub label: String,
    pub url: String,
}
