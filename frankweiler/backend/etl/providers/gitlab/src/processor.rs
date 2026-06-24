//! Program-A `DataProcessor`s for the gitlab (`gitlab_api`) source.
//! `gitlab_api` contributes extract + translate; translate is
//! fingerprint-driven (no render cursor). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl_gitlab_config::{GitlabApiSync, GitlabConfig};

use crate::extract;

/// Build the SourcePlan: always a translate processor; an extract processor
/// when `sync:` is present (managed).
pub fn plan(common: PlanCommon, config: GitlabConfig) -> Result<SourcePlan> {
    let PlanCommon { name, raw_path, .. } = common;
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(GitlabRender {
        id: format!("gitlab/{name}/translate"),
        raw_path: raw_path.clone(),
    }));
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(GitlabExtract {
            id: format!("gitlab/{name}/extract"),
            raw_path,
            sync,
        }));
    }
    Ok(plan)
}

struct GitlabExtract {
    id: String,
    raw_path: PathBuf,
    sync: GitlabApiSync,
}

#[async_trait]
impl DataProcessor for GitlabExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let targets = self
            .sync
            .merge_requests
            .iter()
            .map(|s| extract::parse_mr_ref(s))
            .collect::<Result<Vec<_>>>()
            .context("parse gitlab merge_requests refs")?;
        let s = extract::fetch(extract::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            // full_sync stays false (FetchOptions default) so the
            // gitlab provider honors saved `sync_scope_state` and
            // narrows discovery via `updated_after`. The previous
            // unconditional `true` here disabled the entire
            // incremental path — every run re-discovered and
            // re-fetched every MR in the user's scope. The
            // `--reset-and-redownload` flag still forces a clean
            // re-pull via `db.reset()` when actually needed.
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
            ..Default::default()
        })
        .await?;
        let summary = format!(
            "mrs(new={} skipped_unchanged={}) discussions(new={}) requests={}",
            s.new_mrs, s.skipped_unchanged_mrs, s.new_discussions, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct GitlabRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl DataProcessor for GitlabRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse_api_dir, render_gitlab};
        let parsed = parse_api_dir(&self.raw_path)
            .with_context(|| format!("gitlab parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_gitlab(
            &parsed,
            ctx.root,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_gitlab")?;
        Ok("rendered".into())
    }
}
