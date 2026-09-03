//! Program-A `DataProcessor`s for the github (`github_api`) source. github
//! contributes a render processor (always) and a download processor when
//! `sync:` is present (managed). The source owns its raw store (open/commit/
//! checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_github_config::GithubRenderConfig;
use datalib_etl_github_config::{GithubApiSync, GithubConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: GithubConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(GithubDownload {
            id: format!("github/{name}/download"),
            raw_path,
            sync,
            latchkey: latchkey_settings,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GithubRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GithubRender {
        id: format!("github/{name}/render"),
        raw_path,
    })])
}

struct GithubDownload {
    id: String,
    raw_path: PathBuf,
    sync: GithubApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for GithubDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let targets = self
            .sync
            .pull_requests
            .iter()
            .map(|s| download::parse_pr_ref(s))
            .collect::<Result<Vec<_>>>()
            .context("parse github pull_requests refs")?;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            latchkey: self.latchkey.clone(),
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

    /// The value every sidecar this processor writes carries; the
    /// render step refuses to finish if the two disagree.
    fn render_version(&self) -> Option<u32> {
        Some(crate::render::grid_rows::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render_github};
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
