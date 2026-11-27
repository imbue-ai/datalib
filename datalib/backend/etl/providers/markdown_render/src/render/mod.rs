//! The `markdown` render side: each stored note as one document — its
//! path in the front matter, its properties shown as they were written,
//! and its text as its author wrote it.

pub mod note;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use datalib_etl_markdown::ingest::schema_raw::{
    MarkdownFileRow, MARKDOWN_FILES, MARKDOWN_SCAN_META,
};
use datalib_etl_markdown::ingest::{db_path_for, RawDb};
use datalib_etl_render::inputs::{changed_rows, Input, RawRange};

/// v1: one document per note.
pub const RENDER_VERSION: u32 = 1;

/// The notes at one commit, and which of them this run renders.
#[derive(Debug, Default)]
pub struct Loaded {
    /// The notes to render, in path order.
    pub notes: Vec<MarkdownFileRow>,
    /// The `markdown_scan_meta` row: its id, and the folder the notes'
    /// paths are under.
    pub scan: Option<(String, PathBuf)>,
    /// Paths this run was asked to render that no note holds any more:
    /// each goes to `declare_empty`, so its document is deleted.
    pub gone: Vec<String>,
    /// The commit read.
    pub head: Option<String>,
}

/// One open of the store for the whole pass. `None` means the store
/// could not be read, which is not the same as a folder with no notes.
pub async fn load(raw_dir: &Path, range: RawRange<'_>) -> Result<Option<Loaded>> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
        return Ok(None);
    };
    let loaded = read(&db, range).await;
    // Closed before returning, on the error path too: the next open of
    // this store is a second connection until this one is gone.
    db.close().await;
    loaded.map(Some)
}

async fn read(db: &RawDb, range: RawRange<'_>) -> Result<Loaded> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    // A render unit is a note's path, which is its row's key: the notes whose
    // rows changed, joined with the ones the render driver found stale
    // (a new scan root makes every note stale, since every note read its row).
    let changed = changed_rows(db.pool(), range, pin, &[MARKDOWN_FILES]).await?;
    let forward = changed.map(|mut by_table| by_table.remove(MARKDOWN_FILES).unwrap_or_default());
    let render = range.narrow(forward.as_ref());
    let notes = db.notes(render.as_ref()).await?;
    let held: HashSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();
    let mut gone: Vec<String> = render
        .iter()
        .flatten()
        .filter(|key| !held.contains(key.as_str()))
        .cloned()
        .collect();
    gone.sort();
    Ok(Loaded {
        scan: db.scan_root().await?,
        notes,
        gone,
        head: Some(pin.commit().to_string()),
    })
}

/// What one note's document reads: its row, and the scan row its link
/// is rooted at.
pub fn inputs_of(note: &MarkdownFileRow, scan_id: Option<&str>) -> Vec<Input> {
    let mut inputs = vec![Input::new(MARKDOWN_FILES, &note.id)];
    if let Some(id) = scan_id {
        inputs.push(Input::new(MARKDOWN_SCAN_META, id));
    }
    inputs
}
