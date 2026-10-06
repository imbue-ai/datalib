//! The render wave for the chatgpt source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_chatgpt_config::ChatgptRenderConfig;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ChatgptRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ChatgptRender,
    ))
}

struct ChatgptRender;

#[async_trait]
impl SourceRender for ChatgptRender {
    const PROVIDER: &'static str = "chatgpt";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    /// A conversation is its own row; an attachment is its conversation's.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        use datalib_etl::blob_cas::CasEdgeRow;
        use datalib_etl::bulk::BulkUpsertable;
        use datalib_etl_chatgpt::ingest::schema_raw::{ConversationAttachmentRow, ConversationRow};
        let conversation = if table == ConversationRow::TABLE {
            id
        } else if table == ConversationAttachmentRow::TABLE {
            ConversationAttachmentRow::owning_id_of(id)?
        } else {
            return None;
        };
        Some(crate::render::ids::conversation(source_id, conversation).uuid)
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("chatgpt parse {}", raw_path.display()))?;
        ctx.report_unparsed(
            &ReadScope::Whole(vec!["conversations"]),
            &parsed.unparsed,
            Some(self.render_version()),
        )?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("chatgpt render_all")?;
        // A conversation this run looked at that builds no page is
        // declared with nothing, so its page goes; the rendered ones
        // follow and replace that.
        for conv_id in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::ids::conversation(ctx.name, conv_id).uuid,
                &[],
            )?;
        }
        ctx.declare_empty(parsed.scan.gone.iter().map(String::as_str))?;
        ctx.finish(&buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::blob_cas::CasEdgeRow;
    use datalib_etl_chatgpt::ingest::schema_raw::ConversationAttachmentRow;

    /// A download's problem about a conversation or one of its
    /// attachments reaches the conversation's grid row: the key the
    /// download wrote mints the id the render gives that conversation.
    #[test]
    fn an_attachment_is_its_conversations_row() {
        let attachment = ConversationAttachmentRow::pk_recipe("c1", "file-1");
        let conversation = crate::render::ids::conversation("src", "c1").uuid;
        let item = |table, id| ChatgptRender.item_of_entity("src", table, id);
        assert_eq!(
            item("chatgpt_attachments", &attachment),
            Some(conversation.clone())
        );
        assert_eq!(item("conversations", "c1"), Some(conversation));
        assert_eq!(item("me", "u1"), None);
        assert_eq!(item("chatgpt_attachments", "no-separator"), None);
    }
}
