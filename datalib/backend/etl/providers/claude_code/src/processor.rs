//! The ingest wave for the `claude_code` source: its planner and the
//! [`DataProcessor`] it plans.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_claude_code_config::{ClaudeCodeConfig, DEFAULT_SESSIONS_DIR};

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: ClaudeCodeConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config
        .sessions
        .ok_or_else(|| anyhow!("claude_code source {name} missing its `sessions` table"))?
        .path_or(DEFAULT_SESSIONS_DIR);
    Ok(vec![Box::new(ClaudeCodeIngest {
        id: format!("claude_code/{name}/ingest"),
        raw_path,
        input_path,
    })])
}

struct ClaudeCodeIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for ClaudeCodeIngest {
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
        session.finish(ctx, s.line()).await
    }
}
