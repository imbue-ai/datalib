//! Program-A `DataProcessor`s for the `google_takeout` source. File-backed:
//! download walks the unzipped Takeout tree at `input_path` and lands the
//! opted-in feeds into a provider-owned doltlite raw store; render renders
//! the chat-shaped feeds (Google Chat / Google Voice). The source owns its raw
//! store (open/commit/checkpoint); the orchestrator only drives `run`.
//!
//! This is where the SyncFlags duplication the refactor kills now lives: the
//! mapping `GoogleTakeoutSync` → `download::SyncFlags` is provider-owned here
//! (it used to be hand-copied in the orchestrator's `ExtractPlan::for_source`).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_google_takeout_config::GoogleTakeoutRenderConfig;
use frankweiler_etl_google_takeout_config::{GoogleTakeoutConfig, GoogleTakeoutSync};

use crate::download;

/// Download wave: mirror the export tree at input_path into the raw
/// store, gated by the per-part `sync:` flags.
pub fn plan_download(
    ctx: PlanContext,
    config: GoogleTakeoutConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    Ok(vec![Box::new(GoogleTakeoutDownload {
        id: format!("google_takeout/{name}/download"),
        raw_path,
        input_path,
        sync: sync_flags(config.sync.unwrap_or_default()),
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GoogleTakeoutRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GoogleTakeoutRender {
        id: format!("google_takeout/{name}/render"),
        raw_path,
        name,
    })])
}

/// Map the provider-owned config `GoogleTakeoutSync` onto the download crate's
/// `SyncFlags`, field-for-field — the duplication this refactor moves out of
/// the orchestrator and into the provider.
fn sync_flags(s: GoogleTakeoutSync) -> download::SyncFlags {
    download::SyncFlags {
        maps_reviews: s.maps_reviews,
        maps_saved_places: s.maps_saved_places,
        maps_photos: s.maps_photos,
        youtube_watch_history: s.youtube_watch_history,
        youtube_subscriptions: s.youtube_subscriptions,
        google_chat: s.google_chat,
        gemini_apps: s.gemini_apps,
        google_voice: s.google_voice,
        google_voice_include_spam: s.google_voice_include_spam,
    }
}

struct GoogleTakeoutDownload {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
    sync: download::SyncFlags,
}

#[async_trait]
impl DataProcessor for GoogleTakeoutDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            input_path: self.input_path.clone(),
            sync: self.sync.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "maps(reviews={} saved={} photos={}) youtube(watch={} subs={}) \
                 chat(groups={} users={} messages={}) gemini(activity={}) \
                 blobs={} parse_errors={}",
            s.maps_reviews,
            s.maps_saved_places,
            s.maps_photos,
            s.youtube_watch_history,
            s.youtube_subscriptions,
            s.chat_groups,
            s.chat_users,
            s.chat_messages,
            s.gemini_activity,
            s.blobs_stored,
            s.parse_errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct GoogleTakeoutRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for GoogleTakeoutRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // Only the chat-shaped feeds (Google Chat / Google Voice) render; the
        // other feeds stay queryable in the raw store.
        let prior: &HashMap<String, String> = ctx.prior_fingerprints;
        let mut on_doc = |md| ctx.emit_doc(md);
        crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            prior,
            &mut on_doc,
        )?;
        Ok("rendered".into())
    }
}
