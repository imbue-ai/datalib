//! The incremental half of a chat-shaped render: which of a source's
//! chats this run renders, which buckets it names empty first, and the
//! outcome the processor declares.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::doltlite_raw::DiffScan;
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Buckets, RawRange};

use crate::render::{render_all, RenderProfile};
use crate::types::NormalizedChat;

/// The chats one pass renders, and the buckets it names before it
/// renders any.
#[derive(Debug, Default)]
pub struct ChangedChats {
    /// Every chat the run looked at, and every stale bucket whose rows
    /// are gone, with no inputs: a chat with nothing left builds no
    /// documents, and chat-common never sees it, so this is what makes
    /// its old ones go. The rendered chats' buckets follow and replace
    /// these.
    pub buckets: Buckets,
    pub chats: Vec<NormalizedChat>,
    /// Chats built but left alone: nothing they read changed.
    pub skipped: usize,
}

/// Cut `all` down to the chats this run renders: those the provider's
/// diff names in `changed`, by chat id, and those the driver found stale
/// by chat uuid. `uuid_of` mints the bucket key for a chat id no chat
/// was built for — one whose rows are all gone.
pub fn changed_chats(
    all: Vec<NormalizedChat>,
    range: RawRange<'_>,
    changed: Option<&HashSet<String>>,
    uuid_of: impl Fn(&str) -> String,
) -> ChangedChats {
    let id_by_uuid: HashMap<&str, &str> = all
        .iter()
        .map(|c| (c.chat_uuid.as_str(), c.id.as_str()))
        .collect();
    let narrowed = range.narrow_by(changed, |key| id_by_uuid.get(key).map(|id| id.to_string()));
    let uuid_by_id: HashMap<&str, &str> = all
        .iter()
        .map(|c| (c.id.as_str(), c.chat_uuid.as_str()))
        .collect();
    let buckets = narrowed
        .render
        .iter()
        .flatten()
        .map(|id| match uuid_by_id.get(id.as_str()) {
            Some(uuid) => uuid.to_string(),
            None => uuid_of(id),
        })
        .chain(narrowed.gone.iter().cloned())
        .map(|key| Bucket {
            key,
            inputs: Vec::new(),
        })
        .collect();
    let Some(render) = narrowed.render else {
        return ChangedChats {
            buckets,
            chats: all,
            skipped: 0,
        };
    };
    let before = all.len();
    let chats: Vec<NormalizedChat> = all.into_iter().filter(|c| render.contains(&c.id)).collect();
    ChangedChats {
        buckets,
        skipped: before - chats.len(),
        chats,
    }
}

/// What one render pass of a chat-shaped source did, and what its
/// processor acts on: the buckets to declare and the commit to stamp.
#[derive(Debug, Clone, Default)]
pub struct RenderOutcome {
    pub rendered: usize,
    pub skipped: usize,
    pub new_head: Option<String>,
    pub scan_elapsed: Option<std::time::Duration>,
    pub buckets: Buckets,
}

/// Where a pass writes and what it tells the step as it goes.
pub struct RenderTarget<'a> {
    pub out_root: &'a Path,
    pub source_id: &'a str,
    pub progress: &'a Progress,
    pub on_doc_complete: &'a mut dyn FnMut(RenderedMarkdown) -> Result<()>,
}

/// The whole incremental pass for a source rendered under one profile:
/// [`changed_chats`] against the diff in `scan`, then [`render_all`].
pub fn render_changed(
    profile: &RenderProfile,
    all: Vec<NormalizedChat>,
    scan: DiffScan,
    range: RawRange<'_>,
    uuid_of: impl Fn(&str) -> String,
    blobs_by_chat: &HashMap<String, BlobBundle>,
    target: RenderTarget<'_>,
) -> Result<RenderOutcome> {
    let mut changed = changed_chats(all, range, scan.render.as_ref(), uuid_of);
    let summary = render_all(
        profile,
        &changed.chats,
        target.out_root,
        target.source_id,
        blobs_by_chat,
        target.progress,
        target.on_doc_complete,
    )?;
    changed.buckets.extend(summary.buckets);
    Ok(RenderOutcome {
        rendered: summary.docs_rendered,
        skipped: changed.skipped,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
        buckets: changed.buckets,
    })
}
