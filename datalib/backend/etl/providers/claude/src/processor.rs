//! Program-A `DataProcessor`s for the Claude source types.
//!
//! Two source types, one renderer. `claude_api` downloads from the live
//! claude.ai API ([`plan_download`]); `claude_export` ingests an
//! unpacked bulk export off disk ([`plan_export_download`]). Both write
//! the *same six tables* of the same raw store, which is what lets
//! [`plan_render`] serve either one with a single parser.
//!
//! The source owns its raw store (open/commit/checkpoint); the
//! orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_claude_config::ClaudeRenderConfig;
use datalib_etl_claude_config::{ClaudeApiSync, ClaudeConfig, ClaudeExportConfig};

use crate::download;

/// `claude_api` download wave: empty unless a `sync:` block says what
/// to fetch. Absent `sync:` means "no download this run" — render still
/// reads whatever an earlier run left in the raw store.
pub fn plan_download(
    ctx: PlanContext,
    config: ClaudeConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let latchkey_settings = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(ClaudeDownload {
            id: format!("claude/{name}/download"),
            raw_path,
            sync,
            latchkey: latchkey_settings,
        }));
    }
    Ok(procs)
}

/// `claude_export` download wave: always present. Ingests the unpacked
/// export at `common.input_path` into the raw store at
/// `common.raw_path` — the same `input_path` / `raw_path` split every
/// other file-backed source uses.
pub fn plan_export_download(
    ctx: PlanContext,
    config: ClaudeExportConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    // Without one, `input_or_raw_path()` would fall back to the raw dir
    // and we would ingest the store into itself — which reads as an
    // export with nothing in it. Say so instead.
    let Some(input_path) = config.common.input_path.clone() else {
        anyhow::bail!(
            "claude_export needs `common.input_path` set to the directory you              unpacked the Claude export into (the one holding conversations.json)"
        );
    };
    Ok(vec![Box::new(ClaudeExportIngest {
        id: format!("claude/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        input_path,
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ClaudeRender {
        id: format!("claude/{name}/render"),
        raw_path,
        name,
        max_project_doc_bytes: config.max_project_doc_bytes,
    })])
}

struct ClaudeDownload {
    id: String,
    raw_path: PathBuf,
    sync: ClaudeApiSync,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for ClaudeDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            latchkey: self.latchkey.clone(),
            // users.json is expected alongside the raw store (playback seeds it).
            export_dir: Some(self.raw_path.clone()),
            overlap: self
                .sync
                .refresh_most_recent_n_chat_count
                .map(|v| v as usize)
                .unwrap_or(0),
            sleep_between: Duration::ZERO,
            since: self.sync.since.clone(),
            conv_uuids: self.sync.conv_uuids.clone(),
            projects: self.sync.projects,
            project_uuids: self.sync.project_uuids.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} out_of_scope={} errors={} forbidden_orgs={} total={} \
             projects={} projects_skipped={} project_docs={} project_docs_skipped={} \
             requests={} forbidden_retry_attempts={} forbidden_retry_recoveries={}",
            s.fetched,
            s.skipped,
            s.out_of_scope,
            s.errors,
            s.forbidden_orgs,
            s.total,
            s.projects_fetched,
            s.projects_skipped,
            s.project_docs_fetched,
            s.project_docs_skipped,
            s.requests,
            s.forbidden_retry_attempts,
            s.forbidden_retry_recoveries,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

/// `claude_export`'s download processor: an unpacked bulk export on
/// disk becomes rows in the same raw store the API downloader writes.
struct ClaudeExportIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for ClaudeExportIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::export::ingest(download::export::IngestOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            input_path: self.input_path.clone(),
            // The run-pinned `now`, so every bookkeeping stamp this
            // ingest writes agrees with the rest of the run.
            now: ctx.now.to_string(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "users={} conversations={} projects={} project_docs={} pruned={}",
            s.users, s.conversations, s.projects, s.project_docs, s.pruned,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct ClaudeRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    /// See [`ClaudeRenderConfig::max_project_doc_bytes`].
    max_project_doc_bytes: Option<usize>,
}

#[async_trait]
impl DataProcessor for ClaudeRender {
    fn id(&self) -> &str {
        &self.id
    }

    /// The value every document this processor writes carries: the render
    /// path stamps `profile.render_version`, which is this constant.
    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read claude render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("claude parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            crate::render::render::RenderOptions {
                max_project_doc_bytes: self.max_project_doc_bytes,
            },
            ctx.progress,
            &mut on_doc,
        )
        .context("claude render_all")?;
        Ok("rendered".into())
    }
}
