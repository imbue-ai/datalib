//! Program-A `DataProcessor`s for the chatgpt_api source (download + render).
//! The source owns its raw store; the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_chatgpt_config::{ChatgptApiSync, ChatgptConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: ChatgptConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(ChatgptDownload {
            id: format!("chatgpt/{name}/download"),
            raw_path,
            sync,
            latchkey: latchkey_settings,
        }));
    }
    Ok(procs)
}

struct ChatgptDownload {
    id: String,
    raw_path: PathBuf,
    sync: ChatgptApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for ChatgptDownload {
    fn id(&self) -> &str {
        &self.id
    }

    /// Upserts conversations one at a time and prunes to the enumeration it
    /// just walked. Between checkpoints the store is therefore the previous
    /// snapshot plus whatever this run has fetched — a superset, never a
    /// gap — so a consumer reading one sees stale rows at worst, and the
    /// prune's deletions reach it through the same diff on the next pass.
    /// The one shape that would break that, a truncate before the refill,
    /// happens only under `--reset-and-redownload`, and that run does not
    /// checkpoint at all.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db,
            latchkey: self.latchkey.clone(),
            max_pages: self.sync.max_pages.map(|v| v as usize),
            limit: self.sync.limit.map(|v| v as usize),
            sleep_between: Duration::ZERO,
            since: self.sync.since.clone(),
            conv_uuids: self.sync.conv_uuids.clone(),
            fetched_at: Some(ctx.now.to_string()),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            sealer: Some(session.sealer()),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} out_of_scope={} errors={} listing={} pruned={} requests={}",
            s.fetched, s.skipped, s.out_of_scope, s.errors, s.listing, s.pruned, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
