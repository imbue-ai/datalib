//! Program-A `DataProcessor`s for the `gitlab` source.
//! `gitlab` contributes download + render; render is
//! fingerprint-driven (no render cursor). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_gitlab_config::{GitlabApiSync, GitlabConfig};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

/// Ingest wave: present iff `api`.
pub fn plan_ingest(ctx: PlanContext, config: GitlabConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(GitlabIngest {
            id: format!("gitlab/{name}/download"),
            raw_path,
            sync,
            latchkey: latchkey_settings,
        }));
    }
    Ok(procs)
}

struct GitlabIngest {
    id: String,
    raw_path: PathBuf,
    sync: GitlabApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for GitlabIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let now = datalib_time::parse_strict(ctx.now)
            .with_context(|| format!("gitlab: run stamp {:?}", ctx.now))?;
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let pool = db.pool().clone();
        ctx.run_store(pool, None, |sealer| async {
            let targets = self
                .sync
                .merge_requests
                .iter()
                .map(|s| ingest::parse_mr_ref(s))
                .collect::<Result<Vec<_>>>()
                .context("parse gitlab merge_requests refs")?;
            let s = ingest::fetch(ingest::FetchOptions {
                latchkey: self.latchkey.clone(),
                refresh_window_days: self
                    .sync
                    .refresh_window_days
                    .map(|v| v.max(0) as u32)
                    .unwrap_or(0),
                max_mrs: self.sync.max_mrs.map(|v| v as usize),
                targets,
                sleep_between: Duration::ZERO,
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
                ..ingest::FetchOptions::new(db, now)
            })
            .await?;
            Ok(format!(
                "mrs(new={} skipped_unchanged={}) discussions(new={}) pruned={} requests={}",
                s.new_mrs, s.skipped_unchanged_mrs, s.new_discussions, s.pruned, s.requests,
            ))
        })
        .await
    }
}
