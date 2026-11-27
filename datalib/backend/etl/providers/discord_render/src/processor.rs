//! The render step for the discord source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::CasEdgeRow;
use datalib_etl::processor::PlanContext;
use datalib_etl_discord::ingest::schema_raw::{
    split_message_key, DiscordAttachmentRow, ATTACHMENTS, MESSAGES,
};
use datalib_etl_discord_config::DiscordRenderConfig;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};

use crate::ids;
use crate::render::{render_store, RENDER_VERSION};
use crate::store;

/// One render processor, always: it renders whatever is in the raw store.
pub fn plan_render(
    ctx: PlanContext,
    config: DiscordRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        DiscordRender,
    ))
}

struct DiscordRender;

#[async_trait]
impl SourceRender for DiscordRender {
    const PROVIDER: &'static str = "discord";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    /// A message is its own row; a file is its message's.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        let key = match table {
            MESSAGES => id,
            ATTACHMENTS => DiscordAttachmentRow::owning_id_of(id)?,
            _ => return None,
        };
        let (_, message) = split_message_key(key)?;
        Some(ids::message(source_id, &message).uuid)
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        // No store: this source has never been downloaded, which is the
        // normal state of every source in a fresh config.
        let Some(store) = store::open(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("discord open {}", raw_path.display()))?
        else {
            return Ok("rendered".into());
        };
        let mut unparsed = store.unparsed.clone();
        let rendered = render_store(
            &store,
            ctx.name,
            ctx.page_size,
            ctx.progress,
            &mut |md| ctx.emit_doc(md),
            &mut |page| page.declare(ctx, &mut unparsed),
        );
        let narrowed = store.render.is_some();
        let head = store.new_head.clone();
        store.close();
        rendered?;
        // Everything but the messages is read whole every run.
        let mut whole = vec!["account", "users", "channels", "guilds", "roles"];
        if !narrowed {
            whole.push(MESSAGES);
        }
        ctx.report_unparsed(
            &ReadScope::Whole(whole),
            &unparsed,
            Some(self.render_version()),
        )?;
        if let Some(head) = head {
            ctx.consumed(&head);
        }
        Ok("rendered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_discord::ingest::schema_raw::message_key;

    /// A download's problem about a file reaches the grid row of its
    /// message.
    #[test]
    fn a_file_is_its_messages_row() {
        let key = message_key("71", 175_928_847_299_117_063);
        let message = ids::message("src", "175928847299117063").uuid;
        let item = |table, id: &str| DiscordRender.item_of_entity("src", table, id);
        assert_eq!(item(MESSAGES, &key), Some(message.clone()));
        let edge = DiscordAttachmentRow::pk_recipe(&key, "5");
        assert_eq!(item(ATTACHMENTS, &edge), Some(message));
        assert_eq!(item("users", "1701"), None);
        assert_eq!(item(MESSAGES, "no-separator"), None);
    }
}
