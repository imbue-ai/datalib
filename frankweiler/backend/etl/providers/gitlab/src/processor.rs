//! Program-A `DataProcessor`s for the gitlab (`gitlab_api`) source.
//! `gitlab_api` contributes download + render; render is
//! fingerprint-driven (no render cursor). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_gitlab_config::{GitlabApiSync, GitlabConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: GitlabConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(GitlabDownload {
            id: format!("gitlab/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(ctx: PlanContext, config: GitlabConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GitlabRender {
        id: format!("gitlab/{name}/render"),
        raw_path,
    })])
}

struct GitlabDownload {
    id: String,
    raw_path: PathBuf,
    sync: GitlabApiSync,
}

#[async_trait]
impl DataProcessor for GitlabDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let targets = self
            .sync
            .merge_requests
            .iter()
            .map(|s| download::parse_mr_ref(s))
            .collect::<Result<Vec<_>>>()
            .context("parse gitlab merge_requests refs")?;
        let s = download::fetch(download::FetchOptions {
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
        use crate::render::{parse_api_dir, render_gitlab};
        let parsed = parse_api_dir(&self.raw_path)
            .with_context(|| format!("gitlab parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_gitlab(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_gitlab")?;
        Ok("rendered".into())
    }
}
