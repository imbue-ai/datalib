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
            latchkey: self.latchkey.clone(),
            subtree_pages: seeds,
            max_pages: self.sync.max_pages.map(|m| m as usize),
            refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
            comments: self.sync.comments,
            attachments: self.sync.attachments,
            sleep_between: Duration::ZERO,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            ..download::FetchOptions::new(db)
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
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read notion render cursor {}", cursor_path.display()))?;
        let parsed = parse_api_dir(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("notion parse {}", self.raw_path.display()))?;
        // Documents whose source is gone. They go before the render, so a
        // run interrupted afterwards has already dropped them rather than
        // leaving a document pointing at a page Notion no longer has.
        //
        // A page and its threads are separate conversations, so a vanished
        // page names both: its own uuid, and each discussion the store
        // still remembers hanging off it. The discussion pass then catches
        // a thread whose last comment went while its page survived.
        let mut dropped = 0usize;
        for page in &parsed.vanished_pages {
            dropped += ctx.remove_conversation(page)?;
            for disc in discussions_of(&parsed, page) {
                dropped += ctx.remove_conversation(&disc)?;
            }
        }
        for disc in &parsed.vanished_discussions {
            dropped += ctx.remove_conversation(disc)?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        render_notion(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_notion")?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}

/// Discussions the store still remembers hanging off `page_id`.
///
/// A vanished page's comment rows outlive it — nothing prunes them —
/// which is what makes its threads findable at all. Without this, a
/// deleted page's threads would be orphaned documents no later run
/// would ever name.
fn discussions_of(parsed: &crate::render::ParsedNotion, page_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &parsed.comments {
        if c.get("page_id").and_then(|v| v.as_str()) != Some(page_id) {
            continue;
        }
        if let Some(d) = c.get("discussion_id").and_then(|v| v.as_str()) {
            if !d.is_empty() && !out.iter().any(|x| x == d) {
                out.push(d.to_string());
            }
        }
    }
    out
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
