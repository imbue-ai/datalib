//! The `render_occurrences` step's rows: every occurrence of every event
//! in a window of time, one row each, the way Google's
//! `events.list?singleEvents=true` lists them. A series is run forward
//! (`datalib_etl_calendar_common::occurrences`), the dates it no longer
//! has are taken out, and each moved or edited occurrence stands in for
//! the date it replaced. `README.md` beside this file has the columns.

pub mod store;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use datalib_etl_calendar::ical::{self, Component};
use datalib_etl_calendar::ingest::db::{LoadedGoogleEvent, LoadedIcsObject};
use datalib_etl_calendar_common::occurrences::{self, Occurrence, Series, Window};
use datalib_etl_calendar_common::EventTime;
use datalib_etl_render::inputs::RawRange;
use serde_json::{json, Value};

use crate::render::parse::{self, Parsed};
use crate::render::{google, ics};

/// One occurrence, as `occurrences` stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceRow {
    pub calendar_id: String,
    /// Google's id for the occurrence; for an iCalendar event, its `UID`,
    /// and for an occurrence of a series `<UID>_<when the rule put it>`.
    pub instance_id: String,
    /// The series it is an occurrence of; `None` for a one-off event.
    pub series_id: Option<String>,
    /// Where the rule put it, before any move.
    pub original_start_time: Option<String>,
    pub start_time: String,
    pub end_time: Option<String>,
    pub start_utc: String,
    pub end_utc: String,
    pub all_day: bool,
    pub time_zone: Option<String>,
    pub status: String,
    pub title: Option<String>,
    /// The raw store's row it was read from.
    pub event_row: String,
    pub payload: String,
}

impl OccurrenceRow {
    pub fn id(&self) -> String {
        datalib_etl_calendar::ingest::schema_raw::event_pk(&self.calendar_id, &self.instance_id)
    }
}

/// An event whose occurrences could not be listed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlisted {
    pub calendar_id: String,
    pub event_row: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Listed {
    pub rows: Vec<OccurrenceRow>,
    pub unlisted: Vec<Unlisted>,
}

/// Every occurrence the raw store at `raw_path` holds in `window`, read
/// at its head; `None` when there is no raw store yet.
pub fn read(raw_path: &Path, window: &Window) -> Result<Option<Listed>> {
    let db = datalib_etl_calendar::ingest::db_path_for(raw_path);
    let range = RawRange {
        cursor: None,
        pin: None,
        stale: None,
    };
    Ok(parse::parse(&db, range)?.map(|parsed| list(&parsed, window)))
}

/// Every occurrence `parsed` holds in `window`, in order of start.
pub fn list(parsed: &Parsed, window: &Window) -> Listed {
    let zones: HashMap<&str, Option<&str>> = parsed
        .calendars
        .iter()
        .map(|c| (c.id.as_str(), c.time_zone.as_deref()))
        .collect();
    let zone_of = |calendar: &str| zones.get(calendar).copied().flatten();
    let mut out = Listed::default();
    for u in &parsed.unparsed {
        out.unlisted.push(Unlisted {
            calendar_id: u.id.split('#').next().unwrap_or_default().to_string(),
            event_row: u.id.clone(),
            reason: "the raw row would not read".into(),
        });
    }
    let mut by_calendar: BTreeMap<&str, Vec<&LoadedGoogleEvent>> = BTreeMap::new();
    for e in &parsed.google {
        by_calendar
            .entry(e.calendar_id.as_str())
            .or_default()
            .push(e);
    }
    for (calendar, events) in by_calendar {
        google_calendar(&events, zone_of(calendar), window, &mut out);
    }
    for obj in &parsed.ics {
        ics_object(obj, zone_of(&obj.calendar_id), window, &mut out);
    }
    out.rows.sort_by(|a, b| {
        (&a.start_utc, &a.calendar_id, &a.instance_id).cmp(&(
            &b.start_utc,
            &b.calendar_id,
            &b.instance_id,
        ))
    });
    out.rows.dedup_by(|a, b| a.id() == b.id());
    out
}

/// Google's id for an occurrence of `series_id` the rule put at
/// `original`: `<id>_20261022T160000Z`, or `<id>_20261224` for an
/// all-day one. A series split by "this and following" is
/// `<id>_R20261001T160000`, and its occurrences drop the `_R…`.
pub fn google_instance_id(series_id: &str, original: &EventTime) -> String {
    format!("{}_{}", unsplit(series_id), original.key(None))
}

fn unsplit(series_id: &str) -> &str {
    let Some((base, stamp)) = series_id.rsplit_once("_R") else {
        return series_id;
    };
    let (date, time) = stamp.split_once('T').unwrap_or((stamp, ""));
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if digits(date, 8) && (time.is_empty() || digits(time, 6)) {
        base
    } else {
        series_id
    }
}

fn google_calendar(
    events: &[&LoadedGoogleEvent],
    zone: Option<&str>,
    window: &Window,
    out: &mut Listed,
) {
    let mut changed: HashMap<&str, Vec<&LoadedGoogleEvent>> = HashMap::new();
    for e in events {
        if let Some(series) = google::str_of(&e.event, "recurringEventId") {
            changed.entry(series).or_default().push(e);
        }
    }
    for e in events {
        let v = &e.event;
        if google::str_of(v, "status") == Some("cancelled") {
            continue;
        }
        let Some(id) = google::str_of(v, "id") else {
            continue;
        };
        let unlisted = |reason: String| Unlisted {
            calendar_id: e.calendar_id.clone(),
            event_row: e.id.clone(),
            reason,
        };
        let Some(start) = google::time_at(v, "start") else {
            out.unlisted.push(unlisted("no start".into()));
            continue;
        };
        let end = google::time_at(v, "end");
        let row = |occ: &Occurrence, instance: Option<(String, &str, &EventTime)>| {
            let payload = match &instance {
                Some((instance_id, series_id, original)) => {
                    google_instance(v, instance_id, series_id, original, occ)
                }
                None => v.clone(),
            };
            let original = match &instance {
                Some((_, _, original)) => Some(time_text(original, zone)),
                None => google::time_at(v, "originalStartTime").map(|t| time_text(&t, zone)),
            };
            occurrence_row(
                &e.calendar_id,
                instance.as_ref().map_or(id, |(i, _, _)| i.as_str()),
                instance
                    .as_ref()
                    .map(|(_, s, _)| *s)
                    .or(google::str_of(v, "recurringEventId")),
                original,
                occ,
                zone,
                google::str_of(v, "status"),
                google::text(v, "summary"),
                &e.id,
                payload.to_string(),
            )
        };
        let (rules, rdates, exdates) = google::recurrence(v);
        let recurs = !(rules.is_empty() && rdates.is_empty());
        let mut removed = exdates;
        removed.extend(
            changed
                .get(id)
                .into_iter()
                .flatten()
                .filter_map(|c| google::time_at(&c.event, "originalStartTime")),
        );
        let series = Series {
            start: &start,
            end: end.as_ref(),
            rules: &rules,
            rdates: &rdates,
            removed: if recurs { &removed } else { &[] },
        };
        match occurrences::expand(&series, window, zone) {
            Ok(occs) => {
                for occ in &occs {
                    let instance =
                        recurs.then(|| (google_instance_id(id, &occ.start), id, &occ.start));
                    out.rows.push(row(occ, instance));
                }
            }
            Err(why) => out.unlisted.push(unlisted(why.to_string())),
        }
    }
}

/// What `singleEvents=true` returns for an occurrence the rule gives:
/// the series' resource, without its rule, under the occurrence's id
/// and times.
fn google_instance(
    series: &Value,
    instance_id: &str,
    series_id: &str,
    original: &EventTime,
    occ: &Occurrence,
) -> Value {
    let zone_of = |key: &str| series.pointer(&format!("/{key}/timeZone")).cloned();
    let mut v = series.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("recurrence");
        o.insert("id".into(), json!(instance_id));
        o.insert("recurringEventId".into(), json!(series_id));
        o.insert(
            "originalStartTime".into(),
            google_time(original, zone_of("start")),
        );
        o.insert("start".into(), google_time(&occ.start, zone_of("start")));
        match &occ.end {
            Some(end) => o.insert("end".into(), google_time(end, zone_of("end"))),
            None => o.remove("end"),
        };
    }
    v
}

fn google_time(t: &EventTime, zone: Option<Value>) -> Value {
    let mut v = match t {
        EventTime::Date(d) => json!({ "date": d.format("%Y-%m-%d").to_string() }),
        EventTime::At { .. } => json!({ "dateTime": time_text(t, None) }),
    };
    if let (Some(zone), EventTime::At { .. }) = (zone, t) {
        v["timeZone"] = zone;
    }
    v
}

fn ics_object(obj: &LoadedIcsObject, zone: Option<&str>, window: &Window, out: &mut Listed) {
    let vevents: Vec<Component> = ical::parse(&obj.ics)
        .into_iter()
        .flat_map(|c| c.children_named("VEVENT").cloned().collect::<Vec<_>>())
        .collect();
    let master = vevents.iter().find(|v| v.prop("RECURRENCE-ID").is_none());
    let overrides: Vec<(&Component, EventTime)> = vevents
        .iter()
        .filter_map(|v| Some((v, ics::time_of(v.prop("RECURRENCE-ID")?)?)))
        .collect();
    let cancelled = |v: &Component| ics::status(v).as_deref() == Some("cancelled");
    let unlisted = |reason: String| Unlisted {
        calendar_id: obj.calendar_id.clone(),
        event_row: obj.id.clone(),
        reason,
    };
    let start_end = |v: &Component| {
        let start = v.prop("DTSTART").and_then(ics::time_of);
        let end = v
            .prop("DTEND")
            .and_then(ics::time_of)
            .or_else(|| ics::end_from_duration(start.as_ref(), v.prop("DURATION")?));
        (start, end)
    };
    let row = |v: &Component,
               instance_id: &str,
               series: bool,
               original: Option<&EventTime>,
               occ: &Occurrence| {
        occurrence_row(
            &obj.calendar_id,
            instance_id,
            series.then_some(obj.uid.as_str()),
            original.map(|t| time_text(t, zone)),
            occ,
            zone,
            ics::status(v).as_deref(),
            v.text("SUMMARY").or_else(|| master?.text("SUMMARY")),
            &obj.id,
            json!({ "ics": vevent_text(v) }).to_string(),
        )
    };
    let master_times = master.map(start_end);

    if let Some(m) = master.filter(|m| !cancelled(m)) {
        let (Some(start), end) = start_end(m) else {
            out.unlisted.push(unlisted("no DTSTART".into()));
            return;
        };
        let rules: Vec<String> = m.props_named("RRULE").map(|p| p.value.clone()).collect();
        let rdates = ics::times_in(m, "RDATE");
        let recurs = !(rules.is_empty() && rdates.is_empty());
        let mut removed = ics::times_in(m, "EXDATE");
        removed.extend(overrides.iter().map(|(_, rid)| rid.clone()));
        let series = Series {
            start: &start,
            end: end.as_ref(),
            rules: &rules,
            rdates: &rdates,
            removed: if recurs { &removed } else { &[] },
        };
        match occurrences::expand(&series, window, zone) {
            Ok(occs) => {
                for occ in &occs {
                    let id = match recurs {
                        true => format!("{}_{}", obj.uid, occ.start.key(zone)),
                        false => obj.uid.clone(),
                    };
                    let original = recurs.then_some(&occ.start);
                    out.rows.push(row(m, &id, recurs, original, occ));
                }
            }
            Err(why) => out.unlisted.push(unlisted(why.to_string())),
        }
    }

    for (v, rid) in &overrides {
        if cancelled(v) {
            continue;
        }
        let (start, end) = start_end(v);
        let start = start.unwrap_or_else(|| rid.clone());
        // A changed occurrence that does not say how long it is keeps
        // the series' length.
        let end = end.or_else(|| match &master_times {
            Some((Some(ms), Some(me))) => same_length(&start, ms, me),
            _ => None,
        });
        let one = Series {
            start: &start,
            end: end.as_ref(),
            rules: &[],
            rdates: &[],
            removed: &[],
        };
        match occurrences::expand(&one, window, zone) {
            Ok(occs) => {
                let id = format!("{}_{}", obj.uid, rid.key(zone));
                for occ in &occs {
                    out.rows.push(row(v, &id, true, Some(rid), occ));
                }
            }
            Err(why) => out.unlisted.push(unlisted(why.to_string())),
        }
    }
}

fn same_length(start: &EventTime, from: &EventTime, to: &EventTime) -> Option<EventTime> {
    match (start, from, to) {
        (EventTime::Date(s), EventTime::Date(f), EventTime::Date(t)) => {
            Some(EventTime::Date(*s + (*t - *f)))
        }
        (
            EventTime::At { local, zone },
            EventTime::At { local: f, .. },
            EventTime::At { local: t, .. },
        ) => Some(EventTime::At {
            local: *local + (*t - *f),
            zone: zone.clone(),
        }),
        _ => None,
    }
}

/// One `VEVENT` as content lines, for the row's payload.
fn vevent_text(c: &Component) -> String {
    fn write(c: &Component, out: &mut Vec<String>) {
        out.push(format!("BEGIN:{}", c.name));
        for p in &c.props {
            let mut line = String::new();
            if let Some(g) = &p.group {
                line.push_str(g);
                line.push('.');
            }
            line.push_str(&p.name);
            for (k, v) in &p.params {
                let quoted = v.contains([':', ';', ',']);
                match quoted {
                    true => line.push_str(&format!(";{k}=\"{v}\"")),
                    false => line.push_str(&format!(";{k}={v}")),
                }
            }
            line.push(':');
            line.push_str(&p.value);
            out.push(line);
        }
        for child in &c.children {
            write(child, out);
        }
        out.push(format!("END:{}", c.name));
    }
    let mut lines = Vec::new();
    write(c, &mut lines);
    lines.push(String::new());
    lines.join("\r\n")
}

#[allow(clippy::too_many_arguments)]
fn occurrence_row(
    calendar_id: &str,
    instance_id: &str,
    series_id: Option<&str>,
    original_start_time: Option<String>,
    occ: &Occurrence,
    zone: Option<&str>,
    status: Option<&str>,
    title: Option<String>,
    event_row: &str,
    payload: String,
) -> OccurrenceRow {
    let (s, e) = occurrences::span(&occ.start, occ.end.as_ref(), zone)
        .expect("expand only returns occurrences whose span it read");
    OccurrenceRow {
        calendar_id: calendar_id.to_string(),
        instance_id: instance_id.to_string(),
        series_id: series_id.map(str::to_string),
        original_start_time,
        start_time: time_text(&occ.start, zone),
        end_time: occ.end.as_ref().map(|t| time_text(t, zone)),
        start_utc: utc_text(s),
        end_utc: utc_text(e),
        all_day: occ.start.is_all_day(),
        time_zone: occ
            .start
            .zone_name()
            .or(match occ.start {
                EventTime::Date(_) => zone,
                EventTime::At { .. } => None,
            })
            .map(str::to_string),
        // RFC 5545 has no default; an event that does not say is on.
        status: status
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "confirmed".into()),
        title,
        event_row: event_row.to_string(),
        payload,
    }
}

/// A time as the event has it: a date for an all-day one, else the
/// instant with the offset its own zone has then.
fn time_text(t: &EventTime, zone: Option<&str>) -> String {
    match t {
        EventTime::Date(d) => d.format("%Y-%m-%d").to_string(),
        EventTime::At { local, .. } => match t.instant(zone) {
            Ok(i) => i.at.to_rfc3339_opts(SecondsFormat::Secs, false),
            Err(_) => local.format("%Y-%m-%dT%H:%M:%S").to_string(),
        },
    }
}

fn utc_text(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests;
