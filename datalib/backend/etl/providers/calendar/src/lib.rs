//! The download half of the `calendar` provider: calendars from Google,
//! any CalDAV server (Fastmail, iCloud, Nextcloud, …) or `.ics` files,
//! into a doltlite raw store. Rendering lives in
//! `datalib_etl_calendar_render`. `INGEST.md` has what each upstream
//! was measured to do.

pub mod ical;
pub mod ingest;
pub mod probe;
pub mod processor;
