//! The render wave for the signal source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use datalib_etl_signal_config::SignalRenderConfig;
use std::path::PathBuf;

pub fn plan_render(
    ctx: PlanContext,
    config: SignalRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
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
impl RenderProcessor for SignalRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render_all};

        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        // `period` decides how messages bucket into documents, so a
        // cursor written under a different one points past documents
        // that no longer exist under this one.
        let render_params = crate::render::render_params(self.period);
        let cursor = datalib_etl::render_cursor::read_for_params(&cursor_path, &render_params)
            .with_context(|| format!("read signal render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            self.period,
            &self.name,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("signal parse {}", self.raw_path.display()))?;
        // Chats the newest backup no longer carries. Signal periodizes,
        // so one chat owns several documents; the store resolves how many.
        let mut dropped = 0usize;
        for chat_id in &parsed.vanished_buckets {
            dropped +=
                ctx.remove_conversation(&crate::render::signal_chat_uuid(&self.name, chat_id))?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            &render_params,
            &mut on_doc,
        )
        .context("signal render_all")?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}
