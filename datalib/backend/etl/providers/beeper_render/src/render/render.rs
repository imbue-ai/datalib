//! Beeper's render stage: normalize, then hand off to `chat-common`.
//!
//! Everything about *how* a Beeper message looks now lives in
//! `datalib_etl_chat_common`. What is left here is the stage plumbing —
//! loading each bucket's attachment bytes out of the blob CAS a batch at
//! a time, and
//! running one pass per bridged network, because the `grid_rows`
//! taxonomy and the `source_label` are per-network.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use datalib_etl::blob_cas::{self, BlobBundle};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{Buckets, ChatRenderer};
use datalib_etl_chat_common::NormalizedChat;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::pages::batches;

use super::normalize::{bundle_key, to_networks, Avatars};
use super::parse::{DocBucket, ParsedBeeper};

/// Bump when Beeper's own contribution to the rendered output changes.
/// The shared layout has its own number — see
/// `datalib_etl_chat_common::LAYOUT_VERSION`.
/// v3: ids are minted through `datalib_id` under the configured source,
///     every row carries its backpointer, and an event's id carries its
///     `timestamp_ms` in its leading bits (`datalib_id`'s v8 layout).
///     The raw store keys rooms, users and events by their Matrix ids,
///     so an existing root resets and downloads again; every uuid
///     moved, `chat_uuid` among them.
/// v5: rendered from the mirrored `index.db` tables; a Slack room links
///     to its channel.
/// v6: people are written by their handles and are source contacts,
///     with their avatars; a chat with no name is called by its people.
pub const RENDER_VERSION: u32 = 6;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub blobs_materialized: usize,
    /// Every room rendered, with what it read — what the processor
    /// declares through `RenderCtx::declare_bucket`.
    pub buckets: Buckets,
}

/// Entry point. Renders every `(room, period)` bucket the parser
/// produced, calling `on_doc_complete` once per document, its
/// attachment bytes read a batch of at most `blob_bytes` at a time.
pub fn render_all(
    parsed: &ParsedBeeper,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    raw_db_path: &Path,
    blob_bytes: u64,
) -> Result<RenderSummary> {
    progress.set_length(Some(parsed.docs.len() as u64));
    let cas = block_on(blob_cas::BlobCas::open_for_render(raw_db_path))
        .with_context(|| format!("open the blob store beside {}", raw_db_path.display()))?;
    let rendered = render_open(
        parsed,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
        cas.as_ref(),
        blob_bytes,
    );
    // Closed, not dropped: the next open of this store is a second
    // connection until this one is actually gone.
    if let Some(cas) = cas {
        block_on(cas.close());
    }
    rendered
}

fn render_open(
    parsed: &ParsedBeeper,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    cas: Option<&blob_cas::BlobCas>,
    blob_bytes: u64,
) -> Result<RenderSummary> {
    let avatars = match cas {
        Some(cas) => block_on(load_avatars(parsed, cas))?,
        None => Avatars::new(),
    };
    let docs: HashMap<String, &DocBucket> =
        parsed.docs.iter().map(|d| (bundle_key(d), d)).collect();
    let mut summary = RenderSummary {
        docs_total: parsed.docs.len(),
        ..Default::default()
    };
    let empty = BlobBundle::default();
    for network in to_networks(parsed, source_id, &avatars) {
        let mut renderer = ChatRenderer::new(
            &network.profile,
            out_dir,
            source_id,
            network.chats.len(),
            progress,
            &mut *on_doc_complete,
        );
        let doc_of = |chat: &NormalizedChat| docs.get(&chat.id).copied();
        let weights: Vec<u64> = network
            .chats
            .iter()
            .map(|chat| doc_of(chat).map_or(0, blob_bytes_of))
            .collect();
        for range in batches(&weights, blob_bytes) {
            let batch = &network.chats[range];
            let bundles = match cas {
                Some(cas) => block_on(load_bundles(batch.iter().filter_map(doc_of), cas))?,
                None => HashMap::new(),
            };
            summary.blobs_materialized += bundles.values().map(BlobBundle::len).sum::<usize>();
            for chat in batch {
                renderer.render_chat(chat, bundles.get(&chat.id).unwrap_or(&empty))?;
            }
        }
        let s = renderer.finish();
        summary.docs_rendered += s.docs_rendered;
        summary.buckets.extend(s.buckets);
    }
    Ok(summary)
}

/// The bytes a bucket's attachments take, as the parse found them.
fn blob_bytes_of(doc: &DocBucket) -> u64 {
    doc.messages
        .iter()
        .flat_map(|m| &m.blobs)
        .filter(|b| b.blake3.is_some())
        .map(|b| b.byte_len.unwrap_or(0).max(0) as u64)
        .sum()
}

/// Each bucket's attachment bytes, keyed the way `chat-common` looks
/// them up.
///
/// Beeper's parse resolves a blob's `blake3` but not its bytes, so this
/// is the one place that reads the CAS. `blake3` doubles as the
/// `ref_id`: it is what the normalized attachment carries, and what the
/// bundle is keyed on.
async fn load_bundles<'d>(
    docs: impl Iterator<Item = &'d DocBucket>,
    cas: &blob_cas::BlobCas,
) -> Result<HashMap<String, BlobBundle>> {
    let mut out: HashMap<String, BlobBundle> = HashMap::new();
    for doc in docs {
        let mut bundle = BlobBundle::new();
        for m in &doc.messages {
            for b in &m.blobs {
                let Some(hash) = b.blake3.as_deref() else {
                    continue;
                };
                let Some(mut described) = cas.describe(hash).await? else {
                    bundle.mark_missing(hash);
                    continue;
                };
                described.content_type = described.content_type.or_else(|| b.content_type.clone());
                bundle.add_stored(hash, cas.step(), described, Some(b.slot.clone()));
            }
        }
        if !bundle.is_empty() || bundle.has_missing() {
            out.insert(bundle_key(doc), bundle);
        }
    }
    Ok(out)
}

/// Every loaded person's avatar that is an image.
async fn load_avatars(parsed: &ParsedBeeper, cas: &blob_cas::BlobCas) -> Result<Avatars> {
    let mut avatars = Avatars::new();
    let wanted = parsed
        .people
        .values()
        .flatten()
        .filter_map(|p| p.avatar.as_deref());
    for hash in wanted {
        if avatars.contains_key(hash) {
            continue;
        }
        let Some(obj) = cas.get(hash).await? else {
            continue;
        };
        if let Some(content_type) = image_type(&obj.bytes) {
            avatars.insert(hash.to_string(), (content_type.to_string(), obj.bytes));
        }
    }
    Ok(avatars)
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// The image type of `bytes` by their opening, for the files the app
/// caches under names with no extension.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_avatar_is_known_by_its_opening_bytes() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(
            image_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_type(b"%PDF-1.7"), None);
    }
}
