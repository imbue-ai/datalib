//! `datalib-etl-calendar-common` — one markdown layout and one grid row
//! per calendar event, for every calendar provider. A provider turns
//! what its upstream sent into [`NormalizedEvent`]s; this crate does
//! the rest. `README.md` has the model, recurring events included.

pub mod render;
pub mod rrule;
pub mod types;
pub mod when;

pub use render::{render_all, CalendarRenderProfile, RenderSummary};
pub use types::{
    Attendee, EventLink, EventShape, NormalizedEvent, OccurrenceRef, Person, SeriesRef,
};
pub use when::EventTime;
