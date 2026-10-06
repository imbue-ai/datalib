//! Program-A `DataProcessor` for the `apple_messages` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_apple_messages_config::{chat_db_path, AppleMessagesConfig};
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions};

pub fn mirror_options(config: &AppleMessagesConfig) -> Result<MirrorOptions> {
    let messages = config
        .messages
        .as_ref()
        .ok_or_else(|| {
            anyhow!("apple_messages: missing `messages.path` (the Messages folder, or a chat.db)")
        })?
        .path();
    Ok(MirrorOptions {
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.effective_excluded_tables(),
        exclude_columns: config.effective_excluded_columns(),
        gc: config.gc,
        ..MirrorOptions::new(chat_db_path(&messages, messages.is_dir()))
    })
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: AppleMessagesConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    Ok(vec![Box::new(AppleMessagesIngest {
        id: format!("apple_messages/{}/download", ctx.name),
        raw_path: config.common.raw_path().to_path_buf(),
        options: mirror_options(&config)?,
    })])
}

/// Owns its doltlite store end to end: open, register the interrupt
/// hook, mirror, commit + close via `run_store`.
struct AppleMessagesIngest {
    id: String,
    raw_path: PathBuf,
    options: MirrorOptions,
}

#[async_trait]
impl DataProcessor for AppleMessagesIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let pool = mirror::open_mirror(&entity_db).await?;
        ctx.run_store(pool.clone(), None, |_| async {
            let stats = mirror::run(&pool, &self.options, ctx.progress).await?;
            Ok(stats.summary())
        })
        .await
    }
}
