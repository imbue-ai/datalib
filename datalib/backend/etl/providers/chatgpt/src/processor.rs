//! Program-A `DataProcessor`s for the `chatgpt` source: the live API
//! walk and the export ingest, writing one raw store.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_chatgpt_config::{ChatgptApiSync, ChatgptConfig};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

/// Ingest wave: `api` walks chatgpt.com, asking `export` first when
/// both are set; `export` alone ingests the export as a snapshot.
pub fn plan_ingest(ctx: PlanContext, config: ChatgptConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(ChatgptIngest {
            id: format!("chatgpt/{name}/download"),
            raw_path,
            sync,
            latchkey: latchkey_settings,
            export: config.export.map(|e| e.path()),
        }));
    } else if let Some(export) = config.export {
        procs.push(Box::new(ChatgptExportIngest {
            id: format!("chatgpt/{name}/download"),
            raw_path,
            input_path: export.path(),
        }));
    }
    Ok(procs)
}

struct ChatgptIngest {
    id: String,
    raw_path: PathBuf,
    sync: ChatgptApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
    export: Option<PathBuf>,
}

#[async_trait]
impl DataProcessor for ChatgptIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Upserts conversations a flush at a time and prunes to the
    /// enumeration it just walked. Between checkpoints the store is therefore the previous
    /// snapshot plus whatever this run has fetched — a superset, never a
    /// gap — so a consumer reading one sees stale rows at worst, and the
    /// prune's deletions reach it through the same diff on the next pass.
    /// The one shape that would break that, a truncate before the refill,
    /// never happens inside a run: a reset is its own committed step.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            let s = ingest::fetch(ingest::FetchOptions {
                db,
                latchkey: self.latchkey.clone(),
                max_pages: self.sync.max_pages.map(|v| v as usize),
                limit: self.sync.limit.map(|v| v as usize),
                sleep_between: self.sync.sleep_between(),
                since: self.sync.since.clone(),
                conv_uuids: self.sync.conv_uuids.clone(),
                now: Some(ctx.now.to_string()),
                export: self.export.clone(),
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
            })
            .await?;
            Ok(format!(
                "fetched={} from_export={} skipped={} out_of_scope={} errors={} listing={} \
                 pruned={} blobs_from_export={} requests={}",
                s.fetched,
                s.from_export,
                s.skipped,
                s.out_of_scope,
                s.errors,
                s.listing,
                s.pruned,
                s.blobs_from_export,
                s.requests,
            ))
        })
        .await
    }
}

/// The export ingest: an unpacked data export on disk becomes rows in
/// the same raw store the API download writes.
struct ChatgptExportIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for ChatgptExportIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |_| async {
            let s = ingest::export::ingest(ingest::export::IngestOptions {
                db,
                input_path: self.input_path.clone(),
                now: ctx.now.to_string(),
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
            })
            .await?;
            Ok(format!(
                "conversations={} unchanged={} pruned={} new_blobs={} skipped_blobs={} \
                 not_in_export={}",
                s.conversations,
                s.unchanged,
                s.pruned,
                s.new_blobs,
                s.skipped_blobs,
                s.not_in_export,
            ))
        })
        .await
    }
}
