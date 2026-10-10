//! Program-A `DataProcessor`s for the `notion` source. Notion always
//! contributes a render processor; when `api` is present it also
//! contributes a download processor (the live Notion mirror). The source
//! owns its raw store (open/commit/checkpoint); the orchestrator only drives
//! `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_notion_config::{NotionConfig, NotionSync};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

/// Ingest wave: present iff `api`.
pub fn plan_ingest(ctx: PlanContext, config: NotionConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(NotionIngest {
            id: format!("notion/{name}/download"),
            raw_path,
            sync,
            latchkey,
        }));
    }
    Ok(procs)
}

struct NotionIngest {
    id: String,
    raw_path: PathBuf,
    sync: NotionSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for NotionIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Seals as pages, bodies and comments land.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            // `roots` narrows the mirror; empty means the whole workspace.
            let mut seeds: Vec<String> = self.sync.roots.clone();
            seeds.sort();
            seeds.dedup();
            let s = ingest::fetch(ingest::FetchOptions {
                latchkey: self.latchkey.clone(),
                subtree_pages: seeds,
                max_pages: self.sync.max_pages.map(|m| m as usize),
                refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
                comments: self.sync.comments,
                attachments: self.sync.attachments,
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
                ..ingest::FetchOptions::new(db)
            })
            .await?;
            Ok(format!(
                "pages(listed={}/new={}/upd={}) bodies={} comments={} requests={}",
                s.listed, s.new_pages, s.upd_pages, s.bodies, s.comments, s.official_requests,
            ))
        })
        .await
    }
}
