//! Program-A `DataProcessor`s for the `notion_api` source. Notion always
//! contributes a translate processor; when `sync:` is present it also
//! contributes an extract processor (the live Notion mirror). The source
//! owns its raw store (open/commit/checkpoint); the orchestrator only drives
//! `run`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::http::HttpResponse;
use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl::raw_store::PoolCheckpoint;
use frankweiler_etl_notion_config::{NotionApiSync, NotionConfig};

use crate::extract;

/// Build the SourcePlan: always a translate processor; an extract processor
/// when `sync:` is present (managed mirror). Translate-only `notion_api`
/// sources (no `sync:`) yield translate only.
pub fn plan(common: PlanCommon, config: NotionConfig) -> Result<SourcePlan> {
    let PlanCommon {
        name,
        raw_path,
        playback_root,
        ..
    } = common;
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(NotionRender {
        id: format!("notion/{name}/translate"),
        raw_path: raw_path.clone(),
    }));
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(NotionExtract {
            id: format!("notion/{name}/extract"),
            raw_path,
            sync,
            playback_root,
        }));
    }
    Ok(plan)
}

struct NotionExtract {
    id: String,
    raw_path: PathBuf,
    sync: NotionApiSync,
    playback_root: Option<PathBuf>,
}

#[async_trait]
impl DataProcessor for NotionExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = extract::RawDb::open(&extract::db_path_for(&self.raw_path)).await?;
        let pool = db.pool().clone();
        ctx.register_checkpoint(
            &self.id,
            PoolCheckpoint::new(
                pool.clone(),
                format!("extract {}: interrupted (Ctrl-C)", ctx.name),
            ),
        );
        // Notion has no listing endpoint; in playback mode we derive seeds by
        // scanning the fixture tree for every synthesized page response.
        // Outside playback we honor the configured subtree seeds verbatim.
        let mut seeds: Vec<String> = self
            .sync
            .subtrees
            .as_ref()
            .map(|t| t.pages.clone())
            .unwrap_or_default();
        if let Some(pb) = self.playback_root.as_ref() {
            let derived = derive_notion_seeds(&pb.join("notion")).context("derive notion seeds")?;
            seeds.extend(derived);
        }
        seeds.sort();
        seeds.dedup();
        let s = extract::fetch(extract::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            subtree_pages: seeds,
            inbox: self.sync.inbox.as_ref().is_some_and(|i| i.enabled),
            inbox_mirror_referenced: self
                .sync
                .inbox
                .as_ref()
                .and_then(|i| i.mirror_referenced_pages)
                .unwrap_or(true),
            space: self.sync.inbox.as_ref().and_then(|i| i.space.clone()),
            sleep_between: Duration::ZERO,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
            ..Default::default()
        })
        .await?;
        let summary = format!(
            "pages(new={}/upd={}) blocks(new={}/upd={}) comments(new={}/upd={}) requests(official={}/unofficial={})",
            s.new_pages,
            s.upd_pages,
            s.new_blocks,
            s.upd_blocks,
            s.new_comments,
            s.upd_comments,
            s.official_requests,
            s.unofficial_requests,
        );
        Ok(frankweiler_etl::raw_store::commit_and_close(pool, ctx.name, summary).await)
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

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse_api_dir, render::render_notion_official};
        let parsed = parse_api_dir(&self.raw_path)
            .with_context(|| format!("notion parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_notion_official(
            &parsed,
            ctx.root,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_notion_official")?;
        Ok("rendered".into())
    }
}

/// Walk `<playback>/notion/*.json`, decode each as an [`HttpResponse`], and
/// collect every page id. Used to seed the BFS in playback mode (Notion has
/// no listing endpoint, so without this there'd be nothing to walk).
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
