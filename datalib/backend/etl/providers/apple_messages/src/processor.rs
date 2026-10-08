//! Program-A `DataProcessor` for the `apple_messages` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl::run_problems;
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
        // With "Keep messages" set to a window, Messages evicts what is
        // older, and keeping it is the reason to mirror the database.
        append_only: true,
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
        let pool = &pool;
        ctx.run_store(pool.clone(), None, |_| async move {
            run_problems::collecting(pool, &ctx.control.stop, |found| async move {
                let stats =
                    mirror::run_or_report(pool, &self.options, ctx.progress, &found).await?;
                Ok(stats.map_or_else(|| "nothing mirrored".to_string(), |s| s.summary()))
            })
            .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_source_common::LocalPath;

    /// Messages evicts with "Keep messages" set to a window, so the mirror
    /// keeps what `chat.db` lets go.
    #[test]
    fn messages_are_mirrored_append_only() {
        let config = AppleMessagesConfig {
            messages: Some(LocalPath {
                path: "/Users/picard/Library/Messages".into(),
            }),
            ..Default::default()
        };
        assert!(mirror_options(&config).unwrap().append_only);
    }
}
