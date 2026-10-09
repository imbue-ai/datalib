//! The raw store's events into [`NormalizedEvent`]s, handed to the
//! shared calendar renderer.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_calendar::ingest::db::{LoadedAccount, LoadedGoogleEvent};
use datalib_etl_calendar_common::{
    render_all as cc_render_all, CalendarRenderProfile, NormalizedEvent,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{keys_reading, Bucket, Buckets, Input, RawRange};
use datalib_schema::providers::Provider;

use super::parse::Parsed;
use super::{google, ics, ids};

/// Bump when the rendered layout changes enough that every event needs
/// re-rendering: 2 when a Google time started showing in its own zone;
/// 6 so a full walk sweeps the occurrences earlier versions kept after a
/// "this and following" edit moved them to a new UID.
pub const RENDER_VERSION: u32 = 6;

/// A calendar as its events need it.
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    pub uuid: String,
    pub label: String,
    pub time_zone: Option<String>,
}

/// What a pass looked at: the buckets it built, each with its rows, and
/// the ones it was asked for that no event in the store mints any more.
#[derive(Debug, Default)]
pub struct Rendered {
    pub buckets: Buckets,
    pub gone: Vec<String>,
}

pub fn render_all(
    parsed: &Parsed,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    range: RawRange<'_>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Rendered> {
    let profile = CalendarRenderProfile {
        provider: Provider::Calendar,
        source_label: source_label(parsed.account.as_ref()).to_string(),
        account: parsed.account.as_ref().and_then(|a| a.login.clone()),
        render_version: RENDER_VERSION,
    };
    let mut events = normalize_all(parsed, source_id);
    // Every document shows the account's login and service, so every
    // document reads its row.
    if let Some(a) = &parsed.account {
        for e in &mut events {
            e.inputs.push(Input::new("accounts", &a.id));
        }
    }

    // What to render: the documents the driver found stale, plus every
    // document a changed row feeds — a series reads its occurrences'
    // rows, and a CalDAV object is a series and its occurrences at once.
    let forward = parsed.changed.as_ref().map(|changed| {
        keys_reading(
            changed,
            events
                .iter()
                .map(|e| (e.event_uuid.as_str(), e.inputs.as_slice())),
        )
    });
    let render = range.narrow(forward.as_ref());
    let gone = gone_keys(render.as_ref(), &events, parsed);
    let mut buckets: Buckets = render
        .iter()
        .flatten()
        .filter(|key| gone.binary_search(key).is_err())
        .map(|key| Bucket {
            key: key.clone(),
            inputs: Vec::new(),
        })
        .collect();
    if let Some(render) = &render {
        events.retain(|e| render.contains(&e.event_uuid));
    }
    let summary = cc_render_all(
        &profile,
        &events,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
    )?;
    buckets.extend(summary.buckets);
    Ok(Rendered { buckets, gone })
}

/// The keys asked for that no event mints. Every event row is read on
/// every pass, so such a key is gone upstream even though its row is
/// still there: a "this and following" edit moves an occurrence to a
/// new UID and leaves the old series' row in place, cut short. Nothing
/// is gone while a row would not read; it may be the missing event.
fn gone_keys(
    render: Option<&HashSet<String>>,
    events: &[NormalizedEvent],
    parsed: &Parsed,
) -> Vec<String> {
    let Some(render) = render.filter(|_| parsed.unparsed.is_empty()) else {
        return Vec::new();
    };
    let minted: HashSet<&str> = events.iter().map(|e| e.event_uuid.as_str()).collect();
    let mut gone: Vec<String> = render
        .iter()
        .filter(|key| !minted.contains(key.as_str()))
        .cloned()
        .collect();
    gone.sort();
    gone
}

/// Every event in the store, in a stable order.
pub fn normalize_all(parsed: &Parsed, source_id: &str) -> Vec<NormalizedEvent> {
    let calendars: HashMap<&str, CalendarInfo> = parsed
        .calendars
        .iter()
        .map(|c| {
            (
                c.id.as_str(),
                CalendarInfo {
                    uuid: ids::calendar(source_id, &c.id).uuid,
                    label: c.display_name.clone().unwrap_or_else(|| c.id.clone()),
                    time_zone: c.time_zone.clone(),
                },
            )
        })
        .collect();
    let info = |id: &str| {
        calendars.get(id).cloned().unwrap_or_else(|| CalendarInfo {
            uuid: ids::calendar(source_id, id).uuid,
            label: id.to_string(),
            time_zone: None,
        })
    };

    let mut out: Vec<NormalizedEvent> = Vec::new();
    for obj in &parsed.ics {
        out.extend(ics::normalize(source_id, obj, &info(&obj.calendar_id)));
    }
    let mut google_by_calendar: HashMap<&str, Vec<&LoadedGoogleEvent>> = HashMap::new();
    for e in &parsed.google {
        google_by_calendar
            .entry(&e.calendar_id)
            .or_default()
            .push(e);
    }
    let mut calendar_ids: Vec<&str> = google_by_calendar.keys().copied().collect();
    calendar_ids.sort_unstable();
    for id in calendar_ids {
        out.extend(google::normalize(
            source_id,
            &google_by_calendar[id],
            &info(id),
        ));
    }
    out
}

/// The grid's Source column: the service a person recognizes, where the
/// store says which one it is.
fn source_label(account: Option<&LoadedAccount>) -> &'static str {
    let Some(a) = account else {
        return "Calendar";
    };
    let url = a.server_url.as_deref().unwrap_or("");
    match a.method.as_str() {
        "google" => "Google Calendar",
        "caldav" if url.contains("fastmail.com") => "Fastmail Calendar",
        "caldav" if url.contains("icloud.com") => "iCloud Calendar",
        _ => "Calendar",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use datalib_etl_calendar::ical;
    use datalib_etl_calendar::ingest::db::{LoadedCalendar, LoadedIcsObject};
    use datalib_etl_calendar_common::EventShape;
    use datalib_etl_render::processor::Unparsed;

    /// The checked-in TNG `.ics` files, split the way the `ics` method
    /// stores them.
    fn tng() -> Parsed {
        let mut parsed = Parsed::default();
        for name in ["Bridge", "Engineering"] {
            let path =
                format!(
                "{}/datalib/backend/etl/providers/calendar/tests/fixtures/calendar_tng/{name}.ics",
                std::env::var("TEST_SRCDIR").map(|d| format!("{d}/_main")).unwrap_or_else(|_| {
                    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../..").to_string()
                })
            );
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let split = ical::split_file(&text);
            parsed.calendars.push(LoadedCalendar {
                id: name.to_string(),
                display_name: split.calendar_name.clone(),
                time_zone: split.time_zone.clone(),
            });
            for e in split.events {
                parsed.ics.push(LoadedIcsObject {
                    id: format!("{name}#{}", e.uid),
                    calendar_id: name.to_string(),
                    uid: e.uid,
                    ics: e.ics,
                });
            }
        }
        parsed
    }

    /// Nine events in the fixture become ten documents: the briefing's
    /// moved occurrence has its own, its cancelled one does not, and the
    /// lecture invitation stands alone.
    #[test]
    fn the_tng_calendars_render_one_document_per_event_and_changed_occurrence() {
        let events = normalize_all(&tng(), "tng_calendar");
        let titles: Vec<(&str, &str)> = events
            .iter()
            .map(|e| {
                let shape = match e.shape {
                    EventShape::Single => "single",
                    EventShape::Series { .. } => "series",
                    EventShape::Occurrence { .. } => "occurrence",
                };
                (shape, e.title.as_deref().unwrap_or(""))
            })
            .collect();
        assert_eq!(
            titles,
            vec![
                ("series", "Senior staff briefing"),
                ("occurrence", "Senior staff briefing — Borg incursion"),
                ("single", "Reception for the Klingon delegation"),
                ("single", "Shore leave on Risa"),
                ("series", "Officers' poker night"),
                (
                    "occurrence",
                    "Starfleet Academy guest lecture: first contact protocols"
                ),
                ("series", "Warp core maintenance"),
                ("single", "Dilithium shipment arrives"),
                ("series", "Data's activation day"),
            ]
        );
        let uuids: HashSet<&str> = events.iter().map(|e| e.event_uuid.as_str()).collect();
        assert_eq!(uuids.len(), events.len(), "every document has its own id");
    }

    /// A key asked for that no event mints is gone, unless a row would
    /// not read: that row may be the event, and its documents stay.
    #[test]
    fn a_key_no_event_mints_is_gone_only_when_every_row_read() {
        let mut parsed = tng();
        let events = normalize_all(&parsed, "tng_calendar");
        let asked: HashSet<String> =
            [events[0].event_uuid.clone(), "moved-away".to_string()].into();
        assert_eq!(gone_keys(Some(&asked), &events, &parsed), ["moved-away"]);
        assert!(gone_keys(None, &events, &parsed).is_empty());

        parsed.unparsed.push(Unparsed::new(
            "ics_objects",
            "Bridge#garbled",
            "not iCalendar",
        ));
        assert!(gone_keys(Some(&asked), &events, &parsed).is_empty());
    }
}
