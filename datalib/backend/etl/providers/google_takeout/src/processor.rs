//! Program-A `DataProcessor`s for the `google_takeout` source. File-backed:
//! download walks the unzipped Takeout tree at `export.path` and lands the
//! opted-in feeds into a provider-owned doltlite raw store; render renders
//! the chat-shaped feeds (Google Chat / Google Voice). The source owns its raw
//! store (open/commit/checkpoint); the orchestrator only drives `run`.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_google_takeout_config::{GoogleTakeoutConfig, GoogleTakeoutSync};

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: GoogleTakeoutConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let export = config
        .export
        .ok_or_else(|| anyhow!("google_takeout source {name} missing `export.path`"))?;
    Ok(vec![Box::new(GoogleTakeoutIngest {
        id: format!("google_takeout/{name}/download"),
        raw_path,
        input_path: export.path(),
        sync: sync_flags(export),
    })])
}

fn sync_flags(s: GoogleTakeoutSync) -> ingest::SyncFlags {
    ingest::SyncFlags {
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

struct GoogleTakeoutIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
    sync: ingest::SyncFlags,
}

#[async_trait]
impl DataProcessor for GoogleTakeoutIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            db,
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
