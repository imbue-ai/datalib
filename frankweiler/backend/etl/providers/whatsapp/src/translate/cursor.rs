//! Per-source render cursor stored as a small JSON file at the root of
//! the rendered-md directory for this WhatsApp source. Tracks the
//! `last_rendered_hash` — the doltlite commit hash the renderer
//! successfully processed last time. On the next run, `render_all`
//! asks `dolt_diff_wa_<table>` "which chats changed since that hash?"
//! and skips loading data for the rest.
//!
//! Lives at `<out_dir>/rendered_md/whatsapp/<source_name>/_render_cursor.json`.
//! Assumes a single renderer process — no locking, no atomic-rename
//! dance.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderCursor {
    pub last_rendered_hash: String,
}

pub fn cursor_path(out_dir: &Path, source_name: &str) -> PathBuf {
    out_dir
        .join("rendered_md")
        .join("whatsapp")
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

pub fn write(path: &Path, hash: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&RenderCursor {
        last_rendered_hash: hash.to_string(),
    })
    .context("serialize render cursor")?;
    std::fs::write(path, body).with_context(|| format!("write render cursor {}", path.display()))
}
