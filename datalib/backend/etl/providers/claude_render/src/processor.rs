//! The render wave for the claude source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_claude_config::ClaudeRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ClaudeRender {
            max_project_doc_bytes: config.max_project_doc_bytes,
        },
    ))
}

struct ClaudeRender {
    /// See [`ClaudeRenderConfig::max_project_doc_bytes`].
    max_project_doc_bytes: Option<usize>,
}

#[async_trait]
impl SourceRender for ClaudeRender {
    const PROVIDER: &'static str = "claude";

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
        use datalib_etl_claude::ingest::schema_raw::{ConversationAttachmentRow, ConversationRow};
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
            .with_context(|| format!("claude parse {}", raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(
            &parsed,
            ctx.root,
            ctx.name,
            crate::render::render::RenderOptions {
                max_project_doc_bytes: self.max_project_doc_bytes,
            },
            ctx.progress,
            &mut on_doc,
        )
        .context("claude render_all")?;
        // A conversation that would not build keeps its page, and the
        // problem says why.
        for (id, why) in &parsed.failed {
            let uuid = crate::render::ids::conversation(ctx.name, id).uuid;
            ctx.report_document_failed(&uuid, why, Some(self.render_version()))?;
            ctx.fail_bucket(&uuid, why)?;
        }
        // A bucket this run looked at is a conversation or a project;
        // both uuids are declared with nothing, so whichever page it had
        // that this run did not produce goes. The rendered ones follow
        // and replace that.
        for bucket in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::ids::conversation(ctx.name, bucket).uuid,
                &[],
            )?;
            ctx.declare_bucket(&crate::render::ids::project(ctx.name, bucket).uuid, &[])?;
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
    use datalib_etl_claude::ingest::schema_raw::ConversationAttachmentRow;

    /// A download's problem about a conversation or one of its
    /// attachments reaches the conversation's grid row: the key the
    /// download wrote mints the id the render gives that conversation.
    #[test]
    fn an_attachment_is_its_conversations_row() {
        let attachment = ConversationAttachmentRow::pk_recipe("c1", "f1");
        let conversation = crate::render::ids::conversation("src", "c1").uuid;
        let render = ClaudeRender {
            max_project_doc_bytes: None,
        };
        let item = |table, id| render.item_of_entity("src", table, id);
        assert_eq!(
            item("claude_attachments", &attachment),
            Some(conversation.clone())
        );
        assert_eq!(item("conversations", "c1"), Some(conversation));
        assert_eq!(item("project_docs", "d1"), None);
        assert_eq!(item("claude_attachments", "no-separator"), None);
    }
}
