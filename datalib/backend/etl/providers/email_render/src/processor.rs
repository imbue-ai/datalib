//! The render wave for the email source: its planner and the
//! [`RenderProcessor`] it plans.

use crate::render::render::OutlinkFormat;
use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_email::ingest;
use datalib_etl_email_config::EmailOutlink;
use datalib_etl_email_config::EmailRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: EmailRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let outlink = config.outlink_format.map(outlink_format);
    Ok(vec![Box::new(EmailRender {
        id: format!("email/{name}/render"),
        raw_path,
        name,
        outlink,
        only_render_labels: config.only_render_labels.clone(),
    })])
}

/// Email's render processor — reads the raw store and emits one rendered
/// markdown per thread through the fused-Load callback.
pub struct EmailRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    outlink: Option<OutlinkFormat>,
    /// Render only threads with at least one email under one of these mailbox
    /// label paths (empty = render everything extracted).
    only_render_labels: Vec<String>,
}

#[async_trait]
impl RenderProcessor for EmailRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    // Both knobs change the rendered output for documents the diff
    // would never surface, so a change to either re-renders everything.
    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(crate::render::render::render_params(
            self.outlink,
            &self.only_render_labels,
        ))
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::parse;
        use crate::render::render::render_all;

        let db = ingest::db_path_for(&self.raw_path);
        if !db.exists() {
            tracing::info!(
                source = %self.name,
                db = %db.display(),
                "email render: no raw db — skipping",
            );
            return Ok("skipped (no raw db)".into());
        }

        // Two-phase parse driven by the render cursor's commit.
        let parsed = parse(&db, ctx.raw_range(), !self.only_render_labels.is_empty())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(
            &parsed,
            ctx.root,
            &self.name,
            self.outlink,
            &self.only_render_labels,
            ctx.progress,
            &mut on_doc,
        )?;
        // A thread this run looked at that no email still belongs to,
        // or the label filter keeps out, builds no chat: declared with
        // nothing, its documents go. The rendered ones follow and
        // replace that.
        for (account_id, thread_id) in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::render::thread_uuid(account_id, thread_id),
                &[],
            )?;
        }
        for bucket in &parsed.scan.gone {
            ctx.declare_bucket(bucket, &[])?;
        }
        for bucket in &buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.scan.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}

fn outlink_format(f: EmailOutlink) -> OutlinkFormat {
    match f {
        EmailOutlink::Gmail => OutlinkFormat::Gmail,
        EmailOutlink::Fastmail => OutlinkFormat::Fastmail,
    }
}
