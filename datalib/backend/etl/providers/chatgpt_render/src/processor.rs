//! The render wave for the chatgpt source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_chatgpt_config::ChatgptRenderConfig;
use datalib_etl_render::processor::{ReadScope, RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ChatgptRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ChatgptRender {
        id: format!("chatgpt/{name}/render"),
        raw_path,
        name,
    })])
}

struct ChatgptRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for ChatgptRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(&self.raw_path, &self.name, ctx.raw_range())
            .with_context(|| format!("chatgpt parse {}", self.raw_path.display()))?;
        ctx.report_unparsed(
            &ReadScope::Whole(vec!["conversations"]),
            &parsed.unparsed,
            self.render_version(),
        )?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("chatgpt render_all")?;
        // A conversation this run looked at that builds no page is
        // declared with nothing, so its page goes; the rendered ones
        // follow and replace that.
        for conv_id in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::ids::conversation(&self.name, conv_id).uuid,
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
