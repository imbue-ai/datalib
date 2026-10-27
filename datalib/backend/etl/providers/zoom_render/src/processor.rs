//! The render wave for the zoom source: its planner and the
//! [`RenderProcessor`] it plans.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::changed::changed_chats;
use datalib_etl_chat_common::render::render_all;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Buckets, Inputs, RawRange};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_zoom::ingest::{db_path_for, RawDb};
use datalib_etl_zoom_config::ZoomRenderConfig;

use crate::render::{build_chat, profile, MeetingInput, RENDER_VERSION};
use crate::{ids, store};

pub fn plan_render(
    ctx: PlanContext,
    config: ZoomRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ZoomRender,
    ))
}

struct ZoomRender;

#[async_trait]
impl SourceRender for ZoomRender {
    const PROVIDER: &'static str = "zoom";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    /// A fetch problem names a meeting by its UUID; it belongs on that
    /// meeting's document.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        store::TABLES
            .contains(&table)
            .then(|| ids::meeting(source_id, id).uuid)
    }

    fn upstream_of_entity(&self, table: &str, id: &str) -> Option<(&'static str, String)> {
        store::TABLES
            .contains(&table)
            .then(|| (ids::KIND_MEETING, id.to_string()))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let db_path = db_path_for(raw_path);
        if !db_path.exists() {
            return Ok("no store yet".to_string());
        }
        // On this thread rather than as part of the `Send` future
        // `async_trait` builds: the document callback `render_store`
        // holds across its awaits is not `Send`.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let Some(db) = RawDb::open_reader(&db_path, ctx.raw_range().pin).await? else {
                    return Ok("nothing committed yet".to_string());
                };
                let head = db.pin().map(|p| p.commit().to_string());
                let mut on_doc = |md| ctx.emit_doc(md);
                let result = render_store(
                    &db,
                    ctx.root,
                    ctx.name,
                    ctx.raw_range(),
                    ctx.progress,
                    &mut on_doc,
                )
                .await;
                db.close().await;
                let (buckets, rendered) = result?;
                ctx.finish(&buckets, head.as_deref())?;
                Ok(format!("rendered {rendered} meeting(s)"))
            })
        })
    }
}

/// Render every meeting `range` says has to render, handing each
/// document to `on_doc`. Returns the buckets to declare and how many
/// meetings rendered.
pub async fn render_store(
    db: &RawDb,
    root: &Path,
    source_id: &str,
    range: RawRange<'_>,
    progress: &Progress,
    on_doc: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<(Buckets, usize)> {
    let listing = store::load(db, range).await?;
    let only_one_user = (listing.user_emails.len() == 1)
        .then(|| listing.user_emails.values().next().cloned())
        .flatten();
    let chats = listing
        .meetings
        .values()
        .map(|meeting| {
            let account = meeting
                .str_field("host_id")
                .and_then(|host| listing.user_emails.get(host).cloned())
                .or_else(|| only_one_user.clone());
            let inputs = Inputs::default();
            build_chat(source_id, MeetingInput { meeting, account }, &inputs)
        })
        .collect();
    let changed = changed_chats(chats, range, listing.changed.as_ref(), |uuid| {
        ids::meeting(source_id, uuid).uuid
    });
    let summary = render_all(
        &profile(),
        &changed.chats,
        root,
        source_id,
        &HashMap::new(),
        progress,
        on_doc,
    )
    .context("zoom chat-common render")?;
    let mut buckets = changed.buckets;
    buckets.extend(summary.buckets);
    Ok((buckets, changed.chats.len()))
}
