//! The render wave for the zoom source: its planner and the
//! [`RenderProcessor`] it plans.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::processor::PlanContext;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::changed::changed_chats;
use datalib_etl_chat_common::render::render_all;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Buckets, Inputs, RawRange};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_zoom::ingest::schema_raw::{
    PARTICIPANTS, RECORDINGS, RECORDING_FILES, REPORTED_MEETINGS, SUMMARIES,
};
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

    /// A fetch problem names a meeting by its UUID, or a file by
    /// `{uuid}#{file}`; it belongs on that meeting's document.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        [
            RECORDINGS,
            REPORTED_MEETINGS,
            PARTICIPANTS,
            SUMMARIES,
            RECORDING_FILES,
        ]
        .contains(&table)
        .then(|| ids::meeting(source_id, store::meeting_of(id)).uuid)
    }

    fn upstream_of_entity(&self, table: &str, id: &str) -> Option<(&'static str, String)> {
        [
            RECORDINGS,
            REPORTED_MEETINGS,
            PARTICIPANTS,
            SUMMARIES,
            RECORDING_FILES,
        ]
        .contains(&table)
        .then(|| (ids::KIND_MEETING, store::meeting_of(id).to_string()))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let db_path = db_path_for(raw_path);
        if !db_path.exists() {
            return Ok("no store yet".to_string());
        }
        // On this thread rather than as part of the `Send` future
        // `async_trait` builds, as google_drive's render does: the blob
        // loader's future is not one rustc can prove `Send`.
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
    // Narrowed here as well as in `changed_chats`, so only the meetings
    // that render have their transcripts read.
    let uuid_by_chat: HashMap<String, String> = listing
        .meetings
        .keys()
        .map(|uuid| (ids::meeting(source_id, uuid).uuid, uuid.clone()))
        .collect();
    let narrowed = range.narrow_by(listing.render.as_ref(), |key| {
        uuid_by_chat.get(key).cloned()
    });
    let only_one_user = (listing.user_emails.len() == 1)
        .then(|| listing.user_emails.values().next().cloned())
        .flatten();

    let mut chats = Vec::new();
    let mut files_by_chat = Vec::new();
    for (uuid, meeting) in &listing.meetings {
        if narrowed.render.as_ref().is_some_and(|r| !r.contains(uuid)) {
            continue;
        }
        let transcript = match meeting.transcript() {
            Some(file) => Some(store::transcript(db, file).await?),
            None => None,
        };
        let account = meeting
            .str_field("host_id")
            .and_then(|host| listing.user_emails.get(host).cloned())
            .or_else(|| only_one_user.clone());
        let inputs = Inputs::default();
        let chat = build_chat(
            source_id,
            MeetingInput {
                meeting,
                transcript,
                account,
            },
            &inputs,
        );
        let refs: Vec<String> = meeting
            .files
            .iter()
            .filter(|f| f.blake3.is_some())
            .map(|f| f.edge_id.clone())
            .collect();
        files_by_chat.push((chat.id.clone(), refs));
        chats.push(chat);
    }
    let blobs: HashMap<String, BlobBundle> =
        BlobBundle::load_many(db.pool(), Some(db.cas()), store::FILES_SQL, files_by_chat)
            .await
            .context("load the recording files")?;

    let changed = changed_chats(chats, range, listing.render.as_ref(), |uuid| {
        ids::meeting(source_id, uuid).uuid
    });
    let summary = render_all(
        &profile(),
        &changed.chats,
        root,
        source_id,
        &blobs,
        progress,
        on_doc,
    )
    .context("zoom chat-common render")?;
    let mut buckets = changed.buckets;
    buckets.extend(summary.buckets);
    Ok((buckets, changed.chats.len()))
}
