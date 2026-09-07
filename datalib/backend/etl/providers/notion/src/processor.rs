//! Program-A `DataProcessor`s for the `notion_api` source. Notion always
//! contributes a render processor; when `sync:` is present it also
//! contributes a download processor (the live Notion mirror). The source
//! owns its raw store (open/commit/checkpoint); the orchestrator only drives
//! `run`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::http::HttpResponse;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_notion_config::NotionRenderConfig;
use datalib_etl_notion_config::{NotionConfig, NotionSync};

use crate::download;

/// Download wave: present iff `sync:` (managed). Consumes the
/// playback root (BFS seeds in synth/playback mode).
pub fn plan_download(
    ctx: PlanContext,
    config: NotionConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let playback_root = ctx.playback_root;
    let latchkey = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(NotionDownload {
            id: format!("notion/{name}/download"),
            raw_path,
            sync,
            playback_root,
            latchkey,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: NotionRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(NotionRender {
        id: format!("notion/{name}/render"),
        raw_path,
    })])
}

struct NotionDownload {
    id: String,
    raw_path: PathBuf,
    sync: NotionSync,
    playback_root: Option<PathBuf>,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for NotionDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        // `roots` narrows the mirror; empty means the whole workspace.
        // In playback mode the fixture tree is the workspace, so seeds
        // are derived from every synthesized page response.
        let mut seeds: Vec<String> = self.sync.roots.clone();
        if let Some(pb) = self.playback_root.as_ref() {
            let derived = derive_notion_seeds(&pb.join("notion")).context("derive notion seeds")?;
            seeds.extend(derived);
        }
        seeds.sort();
        seeds.dedup();
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            latchkey: self.latchkey.clone(),
            subtree_pages: seeds,
            max_pages: self.sync.max_pages.map(|m| m as usize),
            refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
            comments: self.sync.comments,
            attachments: self.sync.attachments,
            sleep_between: Duration::ZERO,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            ..Default::default()
        })
        .await?;
        let summary = format!(
            "pages(new={}/upd={}) comments(new={}/upd={}) requests={}",
            s.new_pages, s.upd_pages, s.new_comments, s.upd_comments, s.official_requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct NotionRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl DataProcessor for NotionRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render::render_notion};
        let parsed = parse_api_dir(&self.raw_path)
            .with_context(|| format!("notion parse {}", self.raw_path.display()))?;
        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        //
        // This is the stronger half of the two deletion mechanisms — it
        // needs no `dolt_diff` (which notion is not on yet) and cannot miss
        // a deletion a diff failed to mention.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);
        render_notion(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("render_notion")?;
        ctx.retain_documents(&seen);
        Ok("rendered".into())
    }
}

fn derive_notion_seeds(notion_dir: &Path) -> Result<Vec<String>> {
    let mut seeds = Vec::new();
    if !notion_dir.is_dir() {
        return Ok(seeds);
    }
    for entry in
        fs::read_dir(notion_dir).with_context(|| format!("read_dir {}", notion_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let resp: HttpResponse = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let body: serde_json::Value = match serde_json::from_slice(&resp.body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if body.get("object").and_then(|v| v.as_str()) == Some("page") {
            if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
                seeds.push(id.to_string());
            }
        }
    }
    seeds.sort();
    seeds.dedup();
    Ok(seeds)
}
