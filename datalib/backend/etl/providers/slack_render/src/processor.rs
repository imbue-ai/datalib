//! The render wave for the slack source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use datalib_etl_slack_config::SlackRenderConfig;
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SlackRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        SlackRender,
    ))
}

struct SlackRender;

#[async_trait]
impl SourceRender for SlackRender {
    const PROVIDER: &'static str = "slack";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    /// A message is its own row; an attachment is its message's; a
    /// thread's replies are its root's, whose key the thread's stamp
    /// shares.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        use datalib_etl::blob_cas::CasEdgeRow;
        use datalib_etl::bulk::BulkUpsertable;
        use datalib_etl_slack::ingest::schema_raw::{
            split_key, MessageRow, RepliesPagesRow, SlackAttachmentRow,
        };
        let message_key = if table == MessageRow::TABLE || table == RepliesPagesRow::TABLE {
            id
        } else if table == SlackAttachmentRow::TABLE {
            SlackAttachmentRow::owning_id_of(id)?
        } else {
            return None;
        };
        let (team, channel, ts) = split_key(message_key)?;
        Some(crate::render::ids::message(source_id, team, channel, ts).uuid)
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        use datalib_etl_slack::ingest::schema_raw::split_key;
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("slack parse {}", raw_path.display()))?;
        // Users are read whole every run; messages only for the changed
        // threads on a narrowed run.
        let scope = match parsed.scan.render {
            None => ReadScope::Whole(vec!["users", "messages"]),
            Some(_) => ReadScope::Whole(vec!["users"]),
        };
        ctx.report_unparsed(&scope, &parsed.unparsed, Some(self.render_version()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("slack render_all")?;
        // A thread this run looked at that has no message left builds no
        // chat, so chat-common never sees it: declared with nothing, its
        // documents go. The rendered ones follow and replace that.
        for key in parsed.scan.render.iter().flatten() {
            let Some((team, channel, ts)) = split_key(key) else {
                continue;
            };
            ctx.declare_bucket(
                &crate::render::ids::thread(ctx.name, team, channel, ts).uuid,
                &[],
            )?;
        }
        ctx.finish(&summary.buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::blob_cas::CasEdgeRow;
    use datalib_etl_slack::ingest::schema_raw::{slack_message_key, SlackAttachmentRow};

    /// A download's problem about an attachment reaches the grid row of
    /// its message: the key the download wrote, taken apart, mints the id
    /// the render gives that message.
    #[test]
    fn an_attachment_is_its_messages_row() {
        let ts = "1700000000.000100";
        let message_key = slack_message_key("T1", "C1", ts);
        let attachment = SlackAttachmentRow::pk_recipe(&message_key, "F1");
        let message = crate::render::ids::message("src", "T1", "C1", ts).uuid;
        let item = |table, id| SlackRender.item_of_entity("src", table, id);
        assert_eq!(
            item("slack_attachments", &attachment),
            Some(message.clone())
        );
        assert_eq!(item("messages", &message_key), Some(message.clone()));
        // A thread whose replies could not be read is its root's row.
        assert_eq!(item("replies_pages", &message_key), Some(message));
        assert_eq!(item("users", "U1"), None);
        assert_eq!(item("slack_attachments", "no-separator"), None);
    }
}
