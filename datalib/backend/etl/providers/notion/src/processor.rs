//! Program-A `DataProcessor`s for the `notion` source. Notion always
//! contributes a render processor; when `api` is present it also
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
use datalib_etl_notion_config::{NotionConfig, NotionSync};

use crate::ingest;

/// Ingest wave: present iff `api`. Consumes the
/// playback root (BFS seeds in synth/playback mode).
pub fn plan_ingest(ctx: PlanContext, config: NotionConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let playback_root = ctx.playback_root;
    let latchkey = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(NotionIngest {
            id: format!("notion/{name}/download"),
            raw_path,
            sync,
            playback_root,
            latchkey,
        }));
    }
    Ok(procs)
}

struct NotionIngest {
    id: String,
    raw_path: PathBuf,
    sync: NotionSync,
    playback_root: Option<PathBuf>,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for NotionIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
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
        let s = ingest::fetch(ingest::FetchOptions {
            latchkey: self.latchkey.clone(),
            subtree_pages: seeds,
            max_pages: self.sync.max_pages.map(|m| m as usize),
            refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
            comments: self.sync.comments,
            attachments: self.sync.attachments,
            sleep_between: Duration::ZERO,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            ..ingest::FetchOptions::new(db)
        })
        .await?;
        let summary = format!(
            "pages(new={}/upd={}) comments(new={}/upd={}) requests={}",
            s.new_pages, s.upd_pages, s.new_comments, s.upd_comments, s.official_requests,
        );
        Ok(session.finish(ctx, summary).await)
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
