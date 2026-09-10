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

        // Two-phase parse driven by the render cursor's commit, identical to
        // the old registry path; `prior_fingerprints` is intentionally unused
        // for email (the cursor is the single source of truth).
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        // Both knobs change the rendered output for documents the diff
        // would never surface, so a cursor from a different pair has to
        // go — see `render_cursor::read_for_params`.
        let render_params =
            crate::render::render::render_params(self.outlink, &self.only_render_labels);
        let cursor = datalib_etl::render_cursor::read_for_params(&cursor_path, &render_params)?;
        let parsed = parse(&db, cursor.as_ref().map(|c| c.last_rendered_hash.as_str()))?;

        // Threads the mailbox lost — a JMAP `destroyed`, a Gmail history
        // deletion, or a message gone from a re-ingested mbox.
        let mut dropped = 0usize;
        for (account_id, thread_id) in &parsed.vanished_threads {
            dropped += ctx
                .remove_conversation(&crate::render::render::thread_uuid(account_id, thread_id))?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            self.outlink,
            &self.only_render_labels,
            ctx.progress,
            &mut on_doc,
        )?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}

fn outlink_format(f: EmailOutlink) -> OutlinkFormat {
    match f {
        EmailOutlink::Gmail => OutlinkFormat::Gmail,
        EmailOutlink::Fastmail => OutlinkFormat::Fastmail,
    }
}
