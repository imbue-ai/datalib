//! WhatsApp render — thin adapter over
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! Opens the blob_cas pair (entity pool + sibling CAS pool) so
//! chat-common can stream attachment bytes through the universal
//! `SqliteBlobReader` interface, then forwards every other arg
//! straight through.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobReader, InMemoryBlobReader, SqliteBlobReader};
use frankweiler_etl::doltlite_raw;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_chat_common::{
    render::{RenderProfile, RenderSummary},
    NormalizedChat,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing WhatsApp doc rebuilt.
///
/// v3 = attachment bytes now stream through `frankweiler_etl::blob_cas`
/// (the same store every other chat-style provider uses). The on-disk
/// `blobs/<short>.<ext>` filename is whatever
/// [`BlobView::rendered_filename`] picks, which is blake3-prefixed
/// (was sha256-prefixed in v2); existing docs all rebuild.
///
/// v2 = attachments now materialize bytes into the rendered page's
/// `blobs/` subdir, so images render inline instead of "(not yet
/// fetched)".
///
/// v1 = chat-common's unified block style + reactions inline +
/// per-message `id="m-{uuid}"` anchors.
///
/// [`BlobView::rendered_filename`]: frankweiler_etl::blob_cas::BlobView::rendered_filename
pub const RENDER_VERSION: u32 = 3;

const SOURCE_LABEL: &str = "WhatsApp";

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "whatsapp",
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "WhatsApp Chat".to_string(),
        message_kind: "WhatsApp Message".to_string(),
        reaction_kind: "WhatsApp Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render every chat. `raw_dir` is the source's `input_path` — same
/// value translate's `parse` walks — used here to derive the sibling
/// CAS path so the renderer can stream attachment bytes through
/// [`SqliteBlobReader`].
pub fn render_all(
    chats: &[NormalizedChat],
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let blobs = open_blob_reader(raw_dir)?;
    frankweiler_etl_chat_common::render::render_all(
        &profile(),
        chats,
        out_dir,
        source_name,
        blobs,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )
}

/// Build the `SqliteBlobReader` chat-common reads attachment bytes
/// through. Falls back to an empty in-memory reader when the raw store
/// or CAS files aren't present (e.g. a translate-only re-run against a
/// data root whose extract was skipped — the renderer emits placeholder
/// markdown in that case, same as for any other blob_cas miss).
fn open_blob_reader(raw_dir: &Path) -> Result<Arc<dyn BlobReader>> {
    let db_path = doltlite_raw::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(InMemoryBlobReader::empty_handle());
    }
    let cas_path = blob_cas::cas_path_for(&db_path);

    // Bridge sync render → async sqlx the same way `translate::parse`
    // does: borrow the current tokio Handle if there is one, else spin
    // a fresh runtime for the open. Sync orchestrator calls translate
    // from within `#[tokio::main]`, so the Handle path is the hot one.
    tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(h) => h.block_on(open_pools(&db_path, &cas_path)),
            Err(_) => tokio::runtime::Runtime::new()?.block_on(open_pools(&db_path, &cas_path)),
        }
    })
}

async fn open_pools(db_path: &Path, cas_path: &Path) -> Result<Arc<dyn BlobReader>> {
    let refs_pool = open_ro_pool(db_path).await?;
    if !cas_path.exists() {
        // Refs without bytes: a placeholder will fire for every
        // attachment whose `read_by_ref_id` returns None, which the
        // chat-common renderer already handles.
        return Ok(InMemoryBlobReader::empty_handle());
    }
    let cas_pool = open_ro_pool(cas_path).await?;
    Ok(SqliteBlobReader::new(refs_pool, cas_pool).into_handle())
}

async fn open_ro_pool(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("sqlite uri for {}", path.display()))?
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}
