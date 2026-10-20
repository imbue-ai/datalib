//! The render wave for the email source: its planner and the
//! [`RenderProcessor`] it plans.

use crate::render::render::OutlinkFormat;
use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_email::ingest;
use datalib_etl_email_config::EmailOutlink;
use datalib_etl_email_config::EmailRenderConfig;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: EmailRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let outlink = config.outlink_format.map(outlink_format);
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        EmailRender {
            outlink,
            only_render_labels: config.only_render_labels.clone(),
        },
    ))
}

/// Email's render processor — reads the raw store and emits one rendered
/// markdown per thread through the fused-Load callback.
pub struct EmailRender {
    outlink: Option<OutlinkFormat>,
    /// Render only threads with at least one email under one of these mailbox
    /// label paths (empty = render everything extracted).
    only_render_labels: Vec<String>,
}

#[async_trait]
impl SourceRender for EmailRender {
    const PROVIDER: &'static str = "email";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    // Both knobs change the rendered output for documents the diff
    // would never surface, so a change to either re-renders everything.
    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(crate::render::render::render_params(
            self.outlink,
            &self.only_render_labels,
        ))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::open;
        use crate::render::render::{render_store, PageEnd};

        let db = ingest::db_path_for(raw_path);
        if !db.exists() {
            tracing::info!(
                source = %ctx.name,
                db = %db.display(),
                "email render: no raw db — skipping",
            );
            return Ok("skipped (no raw db)".into());
        }

        let Some(store) = open(
            &db,
            ctx.name,
            ctx.raw_range(),
            !self.only_render_labels.is_empty(),
        )?
        else {
            return Ok("rendered".into());
        };
        // `threads` is read by no render, so a stale parse problem on
        // one is cleared with the rest.
        ctx.report_unparsed(
            &ReadScope::Whole(vec!["accounts", "mailboxes", "threads"]),
            &store.unparsed,
            Some(self.render_version()),
        )?;
        ctx.declare_empty(store.scan.gone.iter().map(String::as_str))?;
        // Each page is declared before the next is read. A stale thread
        // no email still belongs to builds no chat: declared with
        // nothing, its documents go once the diff says its emails left.
        // One the label filter keeps out goes on the filter's word. The
        // rendered ones follow and replace that.
        let mut on_page = |page: PageEnd| -> Result<()> {
            ctx.declare_empty(page.looked_at.iter().map(String::as_str))?;
            for thread in &page.rendered.excluded {
                ctx.exclude_bucket(thread)?;
            }
            ctx.declare_buckets(&page.rendered.buckets)
        };
        let rendered = render_store(
            &store,
            ctx.root,
            ctx.name,
            self.outlink,
            &self.only_render_labels,
            ctx.page_size,
            ctx.progress,
            &mut |md| ctx.emit_doc(md),
            &mut on_page,
        );
        let head = store.scan.new_head.clone();
        store.close();
        rendered?;
        if let Some(head) = head {
            ctx.consumed(&head);
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
