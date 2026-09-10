//! Program-A `DataProcessor`s for the `claude` source: the live API
//! walk and the export ingest, writing one raw store.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_claude_config::{ClaudeApiSync, ClaudeConfig};

use crate::download;

/// Download wave: `api` walks claude.ai, `export` ingests an unpacked
/// bulk export from its `path`. `validate` has already refused both.
pub fn plan_download(
    ctx: PlanContext,
    config: ClaudeConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(ClaudeDownload {
            id: format!("claude/{name}/download"),
            raw_path,
            sync,
            latchkey: config.latchkey_settings.clone(),
        }));
    } else if let Some(export) = config.export {
        procs.push(Box::new(ClaudeExportIngest {
            id: format!("claude/{name}/download"),
            raw_path,
            input_path: export.path(),
        }));
    }
    Ok(procs)
}

struct ClaudeDownload {
    id: String,
    raw_path: PathBuf,
    sync: ClaudeApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for ClaudeDownload {
    fn id(&self) -> &str {
        &self.id
    }

    /// Seals after each conversation and the blobs it names, and prunes
    /// only what the listing walk said is gone. So between checkpoints the
    /// store is the previous snapshot plus whatever this run has fetched --
    /// a superset, never a gap. A truncate before the refill would break
    /// that, and happens only under `--reset-and-redownload`, which does
    /// not checkpoint at all.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        // The CAS goes in too. claude stores attachment bytes in a sibling
        // file, so a checkpoint that sealed only the entities store would
        // publish a message naming blobs no reader can resolve yet.
        let session = datalib_etl::raw_store::RawStoreSession::open_with_blobs(
            db.pool().clone(),
            Some(db.cas().pool().clone()),
            entity_db,
            ctx,
        )
        .await;
        let s = download::fetch(download::FetchOptions {
            db,
            latchkey: self.latchkey.clone(),
            // users.json is expected alongside the raw store (playback seeds it).
            export_dir: Some(self.raw_path.clone()),
            overlap: self
                .sync
                .refresh_most_recent_n_chat_count
                .map(|v| v as usize)
                .unwrap_or(0),
            sleep_between: Duration::ZERO,
            since: self.sync.since.clone(),
            conv_uuids: self.sync.conv_uuids.clone(),
            projects: self.sync.projects,
            project_uuids: self.sync.project_uuids.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            sealer: Some(session.sealer()),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} out_of_scope={} errors={} forbidden_orgs={} pruned={} \
             total={} projects={} projects_skipped={} project_docs={} project_docs_skipped={} \
             requests={} forbidden_retry_attempts={} forbidden_retry_recoveries={}",
            s.fetched,
            s.skipped,
            s.out_of_scope,
            s.errors,
            s.forbidden_orgs,
            s.pruned,
            s.total,
            s.projects_fetched,
            s.projects_skipped,
            s.project_docs_fetched,
            s.project_docs_skipped,
            s.requests,
            s.forbidden_retry_attempts,
            s.forbidden_retry_recoveries,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

/// The export ingest: an unpacked bulk export on disk becomes rows in
/// the same raw store the API downloader writes.
struct ClaudeExportIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for ClaudeExportIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::export::ingest(download::export::IngestOptions {
            db,
            input_path: self.input_path.clone(),
            // The run-pinned `now`, so every bookkeeping stamp this
            // ingest writes agrees with the rest of the run.
            now: ctx.now.to_string(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "users={} conversations={} projects={} project_docs={} pruned={}",
            s.users, s.conversations, s.projects, s.project_docs, s.pruned,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
