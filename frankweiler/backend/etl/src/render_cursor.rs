//! Per-source render cursor stored as a small JSON file at the root of
//! the rendered-md directory for one provider+source pair. Tracks the
//! doltlite commit hash the renderer successfully processed last time,
//! plus the wall-clock cost of the most recent `dolt_diff_<table>`
//! scan so we can see how the diff query scales as the raw store grows.
//!
//! Lives at `<data_root>/<stanza>/rendered_md/_render_cursor.json` — one
//! cursor per stanza, at the root of that stanza's rendered-md tree.
//! Assumes a single renderer process — no locking, no atomic-rename
//! dance.
//!
//! The cursor is read at the top of a provider's `render_all`, used as
//! `from_ref` for the per-provider `dolt_diff_<table>` union query, and
//! re-written with the new HEAD + scan duration after `on_doc_complete`
//! has succeeded for every doc the diff turned up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// JSON shape on disk. New fields land as `Option<…>` so cursors from
/// older render versions still parse cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderCursor {
    /// Doltlite HEAD commit at the time of the last successful render.
    /// Used as `from_ref` in the next run's `dolt_diff_<table>` union.
    pub last_rendered_hash: String,
    /// Wall-clock milliseconds the previous run's `dolt_diff` union
    /// query took. `None` on the first cursor write (cold-start render
    /// did no diff). Kept here so users can eyeball "is the prolly-tree
    /// diff getting slower?" without having to scrape sync logs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_scan_ms: Option<u64>,
    /// RFC 3339 timestamp of when we last wrote the cursor — i.e. when
    /// the most recent successful render completed. Informational.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_render_at: Option<String>,
    /// The render params that produced the documents this cursor points
    /// past. Not informational: [`read_for_params`] invalidates the
    /// cursor when they differ, because the diff-driven skip would
    /// otherwise apply new params only to documents that happen to
    /// change. See that function for why render invalidates wholesale
    /// where the download side reacts proportionally.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<serde_json::Value>,
}

/// Standard cursor path for a stanza: one cursor at the root of that stanza's
/// rendered-md tree, `<data_root>/<stanza>/rendered_md/_render_cursor.json`.
pub fn cursor_path(data_root: &Path, stanza: &str) -> PathBuf {
    crate::layout::rendered_md_root(data_root, stanza).join("_render_cursor.json")
}

pub fn read(path: &Path) -> Result<Option<RenderCursor>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let c: RenderCursor = serde_json::from_str(&s)
                .with_context(|| format!("parse render cursor {}", path.display()))?;
            Ok(Some(c))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read render cursor {}", path.display())),
    }
}

/// [`read`], but treating a params change as "no cursor".
///
/// The cursor turns each render into a `dolt_diff` over what changed
/// upstream, so a render param only ever reaches documents that happen
/// to be in that diff. Widening `only_render_labels` surfaces nothing
/// (no email in the newly-allowed mailbox changed), and changing
/// `period` re-buckets only the chats that moved. Dropping the cursor
/// re-renders the whole tree under the new params.
///
/// Unlike the download side — where a cursor guards rate-limited network
/// calls and so earns a proportional response — render is local work
/// over an on-disk store, so wholesale invalidation is the right trade
/// and much easier to reason about.
///
/// `None` stored params means a cursor written before this field
/// existed. That reads as "no information", never as "changed": every
/// rendered tree in the field is in that state on first upgrade, and
/// invalidating them all would re-render every mirror at once.
pub fn read_for_params(path: &Path, current: &serde_json::Value) -> Result<Option<RenderCursor>> {
    let Some(cursor) = read(path)? else {
        return Ok(None);
    };
    match &cursor.params {
        Some(stored) if stored != current => {
            tracing::info!(
                event = "render_cursor_params_changed",
                cursor = %path.display(),
                from = %stored,
                to = %current,
                "render params changed; re-rendering the whole tree",
            );
            Ok(None)
        }
        _ => Ok(Some(cursor)),
    }
}

/// Write a cursor with the new commit hash, the scan duration from the
/// run that's about to be persisted, and the render params that run
/// used. Caller passes `scan_elapsed = None` on cold-start renders (no
/// diff query happened).
pub fn write(
    path: &Path,
    hash: &str,
    scan_elapsed: Option<Duration>,
    params: &serde_json::Value,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let last_render_at = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let body = serde_json::to_string_pretty(&RenderCursor {
        last_rendered_hash: hash.to_string(),
        last_scan_ms: scan_elapsed.map(|d| d.as_millis() as u64),
        last_render_at: Some(last_render_at),
        params: Some(params.clone()),
    })
    .context("serialize render cursor")?;
    std::fs::write(path, body).with_context(|| format!("write render cursor {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_with_scan_ms() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "my-source");
        write(&p, "abc123", Some(Duration::from_millis(42)), &json!({})).unwrap();
        let read_back = read(&p).unwrap().unwrap();
        assert_eq!(read_back.last_rendered_hash, "abc123");
        assert_eq!(read_back.last_scan_ms, Some(42));
        assert!(read_back.last_render_at.is_some());
    }

    #[test]
    fn missing_cursor_is_none() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "missing");
        assert!(read(&p).unwrap().is_none());
    }

    #[test]
    fn params_change_invalidates_the_cursor() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "src");
        write(&p, "h", None, &json!({"period": "month"})).unwrap();
        assert!(read_for_params(&p, &json!({"period": "month"}))
            .unwrap()
            .is_some());
        // Any difference invalidates — render is local work, so there's
        // no reason to reason about widening vs narrowing here.
        assert!(read_for_params(&p, &json!({"period": "day"}))
            .unwrap()
            .is_none());
    }

    #[test]
    fn cursor_without_params_survives_upgrade() {
        // A cursor written before the field existed. Must NOT re-render
        // every tree in the field on first upgrade.
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "src");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"last_rendered_hash": "old"}"#).unwrap();
        let got = read_for_params(&p, &json!({"period": "day"}))
            .unwrap()
            .expect("legacy cursor should survive");
        assert_eq!(got.last_rendered_hash, "old");
    }

    #[test]
    fn missing_cursor_is_none_for_params_read() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "nope");
        assert!(read_for_params(&p, &json!({})).unwrap().is_none());
    }

    #[test]
    fn cold_start_scan_ms_is_omitted() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "src");
        write(&p, "h", None, &json!({})).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(
            !s.contains("last_scan_ms"),
            "cold-start cursor should omit last_scan_ms, got:\n{s}"
        );
    }
}
