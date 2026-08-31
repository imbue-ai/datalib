//! Program-A `DataProcessor` for the `media` source.
//!
//! `media` is **download-only** — it indexes a tree of audio, images,
//! video and playlists into a doltlite raw store and renders nothing.
//! So [`plan_download`] contributes a single processor and
//! [`plan_render`] returns none: "download-only" is structural (a
//! missing processor), not a flag. Same shape as `fsindex`.
//!
//! The source owns its raw store end to end (open, register the
//! interrupt hook, write, commit) via the standard `RawStoreSession`;
//! the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_media_config::{MediaConfig, MediaRenderConfig};

use crate::download;

pub fn plan_download(ctx: PlanContext, config: MediaConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    config.validate()?;
    let name = ctx.name;
    Ok(vec![Box::new(MediaDownload {
        id: format!("media/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        root: config.common.input_or_raw_path().to_path_buf(),
        ignore: config.ignore,
        max_bytes: config.max_bytes,
        payload_max_bytes: config.payload_max_bytes,
        playlists: config.playlists,
        skip_dataless: config.skip_dataless,
    })])
}

/// Render wave: `media` renders nothing, so this is always empty.
pub fn plan_render(
    ctx: PlanContext,
    config: MediaRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let _ = (ctx, config);
    Ok(Vec::new())
}

struct MediaDownload {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    ignore: Vec<String>,
    max_bytes: Option<u64>,
    payload_max_bytes: Option<u64>,
    playlists: bool,
    skip_dataless: bool,
}

#[async_trait]
impl DataProcessor for MediaDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db,
            source_name: ctx.name.to_string(),
            root: self.root.clone(),
            ignore: self.ignore.clone(),
            max_bytes: self.max_bytes,
            payload_max_bytes: self.payload_max_bytes,
            playlists: self.playlists,
            skip_dataless: self.skip_dataless,
            force_rehash: ctx.control.reset_and_redownload,
            now: ctx.now.to_string(),
            progress: ctx.progress.clone(),
        })
        .await?;
        // `payload_skipped` and `dataless_skipped` are in the summary
        // deliberately: both are silent-by-nature behaviors, and a
        // number in the step's own output is what makes "why is every
        // payload_blake3 NULL?" answerable without a code read.
        let summary = format!(
            "files={} items={} audio={} images={} videos={} hashed={} reused={} \
             payload_hashed={} payload_skipped={} playlists={} entries={} entries_in_tree={} \
             hls_skipped={} dataless_skipped={} too_large={} removed={} errors={}",
            s.files_seen,
            s.items,
            s.audio,
            s.images,
            s.videos,
            s.hashed,
            s.reused,
            s.payload_hashed,
            s.payload_skipped,
            s.playlists,
            s.playlist_entries,
            s.playlist_entries_in_tree,
            s.hls_skipped,
            s.dataless_skipped,
            s.too_large,
            s.removed,
            s.errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
