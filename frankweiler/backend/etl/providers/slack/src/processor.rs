//! Program-A `DataProcessor`s for the slack_api source (download + render).
//!
//! Slack is the one provider that consumes the wire-event tape: its download
//! processor wires its own `EventTape` onto its `RawDb` (when enabled via the
//! resolved shared config, surfaced via `config.common.event_tape_enabled()`) — so
//! the orchestrator no longer needs the `HasEventTape` capability or the
//! `DbHandle::attach_event_tape` no-op-for-everyone-else hook.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_slack_config::SlackRenderConfig;
use frankweiler_etl_slack_config::{SlackApiSync, SlackConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(ctx: PlanContext, config: SlackConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let blob_size_limit_bytes = config.common.blob_size_limit_bytes;
    let event_tape_enabled = config.common.event_tape_enabled();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(SlackDownload {
            id: format!("slack/{name}/download"),
            raw_path,
            sync,
            blob_size_limit_bytes,
            event_tape_enabled,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SlackRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(SlackRender {
        id: format!("slack/{name}/render"),
        raw_path,
        name,
    })])
}

struct SlackDownload {
    id: String,
    raw_path: PathBuf,
    sync: SlackApiSync,
    blob_size_limit_bytes: Option<u64>,
    event_tape_enabled: bool,
}

#[async_trait]
impl DataProcessor for SlackDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let mut db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        // Slack owns its wire-event tape: mirror every upsert to JSONL when the
        // resolved shared config leaves it enabled. (The orchestrator used to
        // attach this; now the one provider that consumes it does.)
        if self.event_tape_enabled {
            let tape = Arc::new(frankweiler_etl::event_tape::EventTape::new(
                frankweiler_etl::raw_layout::events_dir(&self.raw_path),
            ));
            tracing::info!(
                source = %ctx.name,
                events_dir = %tape.dir().display(),
                "event tape enabled — mirroring upserts to JSONL",
            );
            db.attach_event_tape(tape);
        }
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            channels: self.sync.channels.clone(),
            direct_messages: self.sync.direct_messages,
            since: self
                .sync
                .since
                .clone()
                .unwrap_or_else(|| download::DEFAULT_SINCE.into()),
            refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
            members_only: !self.sync.all_channels && self.sync.channels.is_none(),
            media: self.sync.media,
            blob_size_limit_bytes: self.blob_size_limit_bytes,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let media = s
            .media
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let summary = format!("msgs={} replies={} media[{}]", s.messages, s.replies, media);
        Ok(session.finish(ctx, summary).await)
    }
}

struct SlackRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for SlackRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read slack render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("slack parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("slack render_all")?;
        Ok("rendered".into())
    }
}
