//! Per-source render cursor stored as a small JSON file at the root of
//! the rendered-md directory for one provider+source pair. Tracks the
//! doltlite commit hash the renderer successfully processed last time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// JSON shape on disk. New fields land as `Option<…>` so cursors from
/// older render versions still parse cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderCursor {
    /// Doltlite HEAD commit at the time of the last successful render.
    /// Used as `from_ref` in the next run's `dolt_diff_<table>` union.
    pub last_rendered_hash: String,
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

/// The params record for a provider with no render-specific knobs (its
/// render config is the bare envelope). A stable empty object, so
/// [`read_for_params`] never invalidates — but the provider still goes
/// through the same read/write pair, so adding a knob later is a local
/// change rather than a silent one.
pub fn no_params() -> serde_json::Value {
    serde_json::json!({})
}

pub fn write(path: &Path, hash: &str, params: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let last_render_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let body = serde_json::to_string_pretty(&RenderCursor {
        last_rendered_hash: hash.to_string(),
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
    fn round_trip() {
        let td = tempfile::tempdir().unwrap();
        let p = cursor_path(td.path(), "my-source");
        write(&p, "abc123", &json!({})).unwrap();
        let read_back = read(&p).unwrap().unwrap();
        assert_eq!(read_back.last_rendered_hash, "abc123");
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
        write(&p, "h", &json!({"period": "month"})).unwrap();
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
}
