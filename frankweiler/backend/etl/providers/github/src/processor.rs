//! Program-A `DataProcessor`s for the github (`github_api`) source. github
//! contributes a translate processor (always) and an extract processor when
//! `sync:` is present (managed). The source owns its raw store (open/commit/
//! checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_github_config::{GithubApiSync, GithubConfig};

use crate::extract;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: GithubConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(GithubExtract {
            id: format!("github/{name}/extract"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(ctx: PlanContext, config: GithubConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GithubRender {
        id: format!("github/{name}/translate"),
        raw_path,
    })])
}

struct GithubExtract {
    id: String,
    raw_path: PathBuf,
    sync: GithubApiSync,
}

#[async_trait]
impl DataProcessor for GithubExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let targets = self
            .sync
            .pull_requests
            .iter()
            .map(|s| extract::parse_pr_ref(s))
            .collect::<Result<Vec<_>>>()
            .context("parse github pull_requests refs")?;
        let s = extract::fetch(extract::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            // Same fix as gitlab: don't force full_sync, so discovery narrows
            // via saved `sync_scope_state`. Unlike gitlab, github's per-PR
            // loop has no skip optimization yet, so every discovered PR still
            // gets four API calls — but narrowing keeps the discovered set
            // small to begin with.
            refresh_window_days: self
                .sync
                .refresh_window_days
                .map(|v| v.max(0) as u32)
                .unwrap_or(0),
            max_prs: self.sync.max_prs.map(|v| v as usize),
            targets,
            sleep_between: Duration::ZERO,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            ..Default::default()
        })
        .await?;
        let summary = format!(
            "prs(new={}) issue_comments(new={}) reviews(new={}) review_comments(new={})",
            s.new_prs, s.new_issue_comments, s.new_reviews, s.new_review_comments,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct GithubRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl DataProcessor for GithubRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse_api_dir, render_github};
        let parsed = parse_api_dir(&self.raw_path)
            .with_context(|| format!("github parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_github(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_github")?;
        Ok("rendered".into())
    }
}
