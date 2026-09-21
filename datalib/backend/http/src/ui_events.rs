//! `POST /api/ui/events`: what a page of the app reports about itself,
//! written as the lines of its own `ui` process (`docs/dev/logging.md`).
//! The page cannot reach the store, so it posts batches here; the
//! process row is asserted with every batch and closed when the page
//! says it is going.

use app_schema::runs::{LogLevel, LogRow, Process, ProcessRow};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

/// The header a page sends on every request, carrying its process id.
pub const PAGE_HEADER: &str = "x-datalib-page";

/// More than this in one batch is a page that has stopped batching.
const MAX_EVENTS: usize = 500;
/// A name is a word: `navigate`, `error`, `search`.
const MAX_NAME: usize = 64;
/// A stack trace fits; a payload does not.
const MAX_FIELDS_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub struct PageEvents {
    pub page: Page,
    #[serde(default)]
    pub events: Vec<PageEvent>,
    /// The page is going away; this batch closes its process.
    #[serde(default)]
    pub closing: bool,
}

#[derive(Debug, Deserialize)]
pub struct Page {
    /// A UUID the page minted when it loaded.
    pub process_id: String,
    /// When it loaded, as the page's clock said it, with offset.
    pub started_at: String,
}

#[derive(Debug, Deserialize)]
pub struct PageEvent {
    /// When it happened, as the page's clock said it, with offset.
    pub at: String,
    /// The event's kind; the row's target is `ui.<name>`.
    pub name: String,
    /// A [`LogLevel`] word; `info` when absent or unknown.
    #[serde(default)]
    pub level: Option<String>,
    /// The human line; the name when absent.
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub fields: Option<serde_json::Map<String, serde_json::Value>>,
}

pub async fn post_events(
    Json(batch): Json<PageEvents>,
) -> Result<StatusCode, (StatusCode, String)> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string());
    if uuid::Uuid::parse_str(&batch.page.process_id).is_err() {
        return Err(bad("page.process_id is not a UUID"));
    }
    let started = datalib_time::parse_strict(&batch.page.started_at)
        .map_err(|_| bad("page.started_at is not an offset-bearing timestamp"))?;
    if batch.events.len() > MAX_EVENTS {
        return Err(bad("too many events in one batch"));
    }
    let Some(writer) = crate::logging::writer() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the run store is not open in this server".to_string(),
        ));
    };

    let mut rows = Vec::with_capacity(batch.events.len());
    for e in &batch.events {
        rows.push(row_of(&batch.page.process_id, e)?);
    }

    let (started_at_utc, tz_offset) = started.to_utc_and_offset();
    let finished_at_utc = batch.closing.then(|| datalib_runs::store::now_split().0);
    writer.process(ProcessRow {
        process_id: batch.page.process_id.clone(),
        process: Process::Ui.as_str().into(),
        started_at_utc,
        finished_at_utc,
        tz_offset: Some(tz_offset),
        // The bundle is embedded in this binary, so the page's commit
        // is the server's.
        git_hash: datalib_runs::git_hash(),
        ..Default::default()
    });
    for row in rows {
        writer.log(row);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn row_of(process_id: &str, e: &PageEvent) -> Result<LogRow, (StatusCode, String)> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    let name_ok = !e.name.is_empty()
        && e.name.len() <= MAX_NAME
        && e.name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !name_ok {
        return Err(bad(format!("event name {:?} is not a word", e.name)));
    }
    let fields = match &e.fields {
        Some(f) if !f.is_empty() => {
            let text = serde_json::Value::Object(f.clone()).to_string();
            if text.len() > MAX_FIELDS_BYTES {
                return Err(bad(format!("event {} carries too much", e.name)));
            }
            Some(text)
        }
        _ => None,
    };
    // A page whose clock reads wrong is still a page that did something;
    // the server's arrival stamp is the better of two bad answers.
    let (ts_utc, tz_offset) = match datalib_time::parse_strict(&e.at) {
        Ok(t) => {
            let (utc, off) = t.to_utc_and_offset();
            (utc, Some(off))
        }
        Err(_) => datalib_runs::store::now_split(),
    };
    let level = e
        .level
        .as_deref()
        .and_then(LogLevel::parse)
        .unwrap_or(LogLevel::Info);
    Ok(LogRow {
        process_id: process_id.to_string(),
        ts_utc,
        tz_offset,
        level: level.as_str().into(),
        target: Some(format!("ui.{}", e.name)),
        msg: e.msg.clone().unwrap_or_else(|| e.name.clone()),
        fields,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str) -> PageEvent {
        PageEvent {
            at: "2026-09-21T10:00:00.250-07:00".into(),
            name: name.into(),
            level: None,
            msg: None,
            fields: None,
        }
    }

    #[test]
    fn an_event_is_a_row_under_the_page_with_its_own_clock() {
        let mut e = event("navigate");
        e.msg = Some("/cards".into());
        e.level = Some("warn".into());
        e.fields = Some(
            serde_json::json!({"from": "/"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let row = row_of("p-1", &e).unwrap();
        assert_eq!(row.process_id, "p-1");
        assert_eq!(row.ts_utc, "2026-09-21T17:00:00.250000+00:00");
        assert_eq!(row.tz_offset.as_deref(), Some("-07:00"));
        assert_eq!(row.level, "warn");
        assert_eq!(row.target.as_deref(), Some("ui.navigate"));
        assert_eq!(row.msg, "/cards");
        assert_eq!(row.fields.as_deref(), Some(r#"{"from":"/"}"#));
        assert!(row.run_id.is_none());
    }

    #[test]
    fn a_bare_event_reads_as_its_name_at_info() {
        let row = row_of("p-1", &event("page_load")).unwrap();
        assert_eq!(row.level, "info");
        assert_eq!(row.msg, "page_load");
        assert!(row.fields.is_none());
    }

    #[test]
    fn a_name_that_is_not_a_word_is_refused() {
        for name in ["", "Navigate", "ui.navigate", "a b", &"x".repeat(65)] {
            assert!(row_of("p-1", &event(name)).is_err(), "{name:?}");
        }
        assert!(row_of("p-1", &event("open_document_2")).is_ok());
    }

    #[test]
    fn an_unknown_level_is_info_not_a_guess() {
        let mut e = event("x");
        e.level = Some("fatal".into());
        assert_eq!(row_of("p-1", &e).unwrap().level, "info");
    }
}
