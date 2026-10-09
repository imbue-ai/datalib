//! The render wave for the google_takeout source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GoogleTakeoutRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        GoogleTakeoutRender,
    ))
}

struct GoogleTakeoutRender;

#[async_trait]
impl SourceRender for GoogleTakeoutRender {
    const PROVIDER: &'static str = "google_takeout";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    /// A message or activity row is its own row; an attachment is its
    /// message's or prompt's. Looked up rather than minted: an item's
    /// uuid carries its date, which the raw key does not.
    fn upstream_of_entity(&self, table: &str, id: &str) -> Option<(&'static str, String)> {
        use crate::ids::*;
        use datalib_etl::blob_cas::CasEdgeRow;
        use datalib_etl::bulk::BulkUpsertable;
        use datalib_etl_google_takeout::ingest::google_voice::schema_raw::{
            VoiceAttachmentRow, VoiceMessageRow,
        };
        use datalib_etl_google_takeout::ingest::schema_raw::{
            ChatAttachmentRow, ChatMessageRow, GeminiActivityRow, GeminiAttachmentRow,
            MapsPhotoRow, MapsReviewRow, MapsSavedPlaceRow, YoutubeSubscriptionRow,
            YoutubeWatchRow,
        };
        let (kind, message) = match table {
            t if t == ChatMessageRow::TABLE => (KIND_MESSAGE, id),
            t if t == ChatAttachmentRow::TABLE => {
                (KIND_MESSAGE, ChatAttachmentRow::owning_id_of(id)?)
            }
            t if t == VoiceMessageRow::TABLE => (KIND_VOICE_MESSAGE, id),
            t if t == VoiceAttachmentRow::TABLE => {
                (KIND_VOICE_MESSAGE, VoiceAttachmentRow::owning_id_of(id)?)
            }
            t if t == GeminiActivityRow::TABLE => (KIND_GEMINI_PROMPT, id),
            t if t == GeminiAttachmentRow::TABLE => {
                (KIND_GEMINI_PROMPT, GeminiAttachmentRow::owning_id_of(id)?)
            }
            t if t == YoutubeWatchRow::TABLE => (KIND_YOUTUBE_WATCH, id),
            t if t == YoutubeSubscriptionRow::TABLE => (KIND_YOUTUBE_SUBSCRIPTION, id),
            t if t == MapsReviewRow::TABLE => (KIND_MAPS_REVIEW, id),
            t if t == MapsSavedPlaceRow::TABLE => (KIND_MAPS_SAVED_PLACE, id),
            t if t == MapsPhotoRow::TABLE => (KIND_MAPS_PHOTO, id),
            _ => return None,
        };
        Some((kind, message.to_string()))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            raw_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
        )?;
        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
