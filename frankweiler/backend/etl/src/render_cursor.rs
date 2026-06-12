//! Per-source render cursor stored as a small JSON file at the root of
//! the rendered-md directory for one provider+source pair. Tracks the
//! doltlite commit hash the renderer successfully processed last time,
//! plus the wall-clock cost of the most recent `dolt_diff_<table>`
//! scan so we can see how the diff query scales as the raw store grows.
//!
//! Lives at `<out_dir>/rendered_md/<provider>/<source_name>/_render_cursor.json`.
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
}

/// Standard cursor path for `<provider>/<source_name>`. Matches the
/// directory layout every chat-style provider already writes into.
pub fn cursor_path(out_dir: &Path, provider: &str, source_name: &str) -> PathBuf {
    out_dir
        .join("rendered_md")
        .join(provider)
        .join(source_name)
        .join("_render_cursor.json")
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

/// Write a cursor with the new commit hash and the scan duration from
/// the run that's about to be persisted. Caller passes `scan_elapsed =
/// None` on cold-start renders (no diff query happened).
pub fn write(path: &Path, hash: &str, scan_elapsed: Option<Duration>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let last_render_at = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let body = serde_json::to_string_pretty(&RenderCursor {
        last_rendered_hash: hash.to_string(),
        last_scan_ms: scan_elapsed.map(|d| d.as_millis() as u64),
        last_render_at: Some(last_render_at),
    })
    .context("serialize render cursor")?;
    std::fs::write(path, body).with_context(|| format!("write render cursor {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_scan_ms() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "signal", "my-source");
        write(&p, "abc123", Some(Duration::from_millis(42))).unwrap();
        let read_back = read(&p).unwrap().unwrap();
        assert_eq!(read_back.last_rendered_hash, "abc123");
        assert_eq!(read_back.last_scan_ms, Some(42));
        assert!(read_back.last_render_at.is_some());
    }

    #[test]
    fn missing_cursor_is_none() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "signal", "missing");
        assert!(read(&p).unwrap().is_none());
    }

    #[test]
    fn cold_start_scan_ms_is_omitted() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "email", "src");
        write(&p, "h", None).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(
            !s.contains("last_scan_ms"),
            "cold-start cursor should omit last_scan_ms, got:\n{s}"
        );
    }
}
