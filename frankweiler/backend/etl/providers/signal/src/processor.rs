//! Program-A `DataProcessor`s for the `signal_backup` source. A managed
//! signal source (`sync:` present) contributes download + render; the
//! render processor is always present (renders whatever is in the raw
//! store). The source owns its raw store (open/commit/checkpoint); the
//! orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use frankweiler_etl::periodize::Period;
use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_signal_config::SignalRenderConfig;
use frankweiler_etl_signal_config::{SignalConfig, SignalSync};

use crate::download;

/// Download wave. Signal REQUIRES a `sync.snapshot_dir`: a managed
/// signal source without a `sync:` block has nowhere to read snapshots
/// from, so error exactly as the old orchestrator's `for_source` did.
pub fn plan_download(
    ctx: PlanContext,
    config: SignalConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .sync
        .ok_or_else(|| anyhow!("signal_backup source {name} missing sync.snapshot_dir"))?;
    if sync.period.is_some() {
        anyhow::bail!(
            "signal `sync.period` is a render knob — put `period` in the \
             render step's params instead"
        );
    }
    Ok(vec![Box::new(SignalDownload {
        id: format!("signal/{name}/download"),
        raw_path,
        sync,
    })])
}

/// Render wave. The period comes from the render step's own params
/// (default `month`).
pub fn plan_render(
    ctx: PlanContext,
    config: SignalRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let period = Period::from_config(config.period.as_deref()).context("signal period")?;
    Ok(vec![Box::new(SignalRender {
        id: format!("signal/{name}/render"),
        raw_path,
        name,
        period,
    })])
}

/// Signal's download processor. Owns its raw doltlite store end to end: opens
/// it, registers an opaque interrupt-commit hook, decrypts the newest snapshot
/// under `snapshot_dir`, commits, closes.
struct SignalDownload {
    id: String,
    raw_path: PathBuf,
    sync: SignalSync,
}

#[async_trait]
impl DataProcessor for SignalDownload {
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
            snapshot_root: self.sync.snapshot_dir.clone(),
            // Default: `<snapshot_root>/files/XX/<name>` — the layout Signal
            // Android produces. Override via a future SignalSync knob if it
            // matters.
            files_root: None,
            aep_env_var: self.sync.aep_env_var.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "recipients={} chats={} chat_items={} media_files={} snapshot={}",
            s.recipients, s.chats, s.chat_items, s.media_files, s.snapshot,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

/// Signal's render processor — reads the raw store (driven by the render
/// cursor's commit) and emits one rendered markdown per period-bucket through
/// the fused-Load callback.
struct SignalRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    period: Period,
}

#[async_trait]
impl DataProcessor for SignalRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse, render_all};

        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read signal render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            self.period,
            &self.name,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("signal parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("signal render_all")?;
        Ok("rendered".into())
    }
}
