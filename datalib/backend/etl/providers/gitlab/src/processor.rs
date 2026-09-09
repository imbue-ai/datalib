//! Program-A `DataProcessor`s for the gitlab (`gitlab_api`) source.
//! `gitlab_api` contributes download + render; render is
//! fingerprint-driven (no render cursor). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_gitlab_config::GitlabRenderConfig;
use datalib_etl_gitlab_config::{GitlabApiSync, GitlabConfig};
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: GitlabConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(GitlabDownload {
            id: format!("gitlab/{name}/download"),
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
    config: GitlabRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
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
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
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
            latchkey: self.latchkey.clone(),
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
            ..download::FetchOptions::new(db)
        })
        .await?;
        let summary = format!(
            "mrs(new={} skipped_unchanged={}) discussions(new={}) pruned={} requests={}",
            s.new_mrs, s.skipped_unchanged_mrs, s.new_discussions, s.pruned, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct GitlabRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl RenderProcessor for GitlabRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::grid_rows::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render_gitlab};
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read gitlab render cursor {}", cursor_path.display()))?;
        let parsed = parse_api_dir(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("gitlab parse {}", self.raw_path.display()))?;

        // Named, not swept: this render is narrowed by the diff, so what it
        // emits is only what changed. Handing that to `retain_documents`
        // would delete every MR that merely held still.
        let mut dropped = 0usize;
        for bucket in &parsed.vanished_buckets {
            let Some((proj, iid)) = bucket.rsplit_once('!') else {
                continue;
            };
            let Ok(iid) = iid.parse::<u32>() else {
                continue;
            };
            dropped += ctx.remove_conversation(&crate::render::parse::gitlab_mr_uuid(proj, iid))?;
        }

        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_gitlab(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_gitlab")?;
        Ok(format!(
            "rendered={} skipped={} dropped={}",
            s.rendered, parsed.docs_skipped, dropped
        ))
    }
}
