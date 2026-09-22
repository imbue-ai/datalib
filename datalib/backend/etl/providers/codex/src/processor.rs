//! The ingest wave for the `codex` source: its planner and the
//! [`DataProcessor`] it plans.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_codex_config::CodexConfig;

use crate::ingest;

pub fn plan_ingest(ctx: PlanContext, config: CodexConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config
        .sessions
        .ok_or_else(|| anyhow!("codex source {name} missing its `sessions` table"))?
        .path();
    Ok(vec![Box::new(CodexIngest {
        id: format!("codex/{name}/ingest"),
        raw_path,
        input_path,
    })])
}

struct CodexIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for CodexIngest {
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
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "files={} read={} threads={} subagents={} records={} malformed_lines={} \
             not_transcripts={} unreadable={}",
            s.files,
            s.files_read,
            s.threads,
            s.subagents,
            s.records,
            s.malformed_lines,
            s.not_transcripts,
            s.unreadable,
        );
        session.finish(ctx, summary).await
    }
}
