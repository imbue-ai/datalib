//! Beeper's render stage: normalize, then hand off to `chat-common`.
//!
//! Everything about *how* a Beeper message looks now lives in
//! `datalib_etl_chat_common`. What is left here is the stage plumbing —
//! loading each bucket's attachment bytes out of the blob CAS, and
//! running one pass per bridged network, because the `grid_rows`
//! taxonomy and the `source_label` are per-network.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use datalib_etl::blob_cas::{self, BlobBundle};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::Buckets;
use datalib_etl_render::grid_index::RenderedMarkdown;

use super::normalize::{bundle_key, to_networks, Avatars};
use super::parse::ParsedBeeper;

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
/// produced, calling `on_doc_complete` once per document.
pub fn render_all(
    parsed: &ParsedBeeper,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    raw_db_path: &Path,
) -> Result<RenderSummary> {
    progress.set_length(Some(parsed.docs.len() as u64));

    let (blobs_by_chat, avatars) = load_blobs(parsed, raw_db_path)?;
    let blobs_materialized = blobs_by_chat.values().map(BlobBundle::len).sum();

    let mut summary = RenderSummary {
        docs_total: parsed.docs.len(),
        blobs_materialized,
        ..Default::default()
    };

    for network in to_networks(parsed, source_id, &avatars) {
        let s = datalib_etl_chat_common::render_all(
            &network.profile,
            &network.chats,
            out_dir,
            source_id,
            &blobs_by_chat,
            progress,
            on_doc_complete,
        )?;
        summary.docs_rendered += s.docs_rendered;
        summary.buckets.extend(s.buckets);
    }
    Ok(summary)
}

/// Every bucket's attachment bytes, keyed the way `chat-common` looks
/// them up, and every loaded person's avatar that is an image.
///
/// Beeper's parse resolves a blob's `blake3` but not its bytes, so this
/// is the one place that reads the CAS. `blake3` doubles as the
/// `ref_id`: it is what the normalized attachment carries, and what the
/// bundle is keyed on.
fn load_blobs(
    parsed: &ParsedBeeper,
    raw_db_path: &Path,
) -> Result<(HashMap<String, BlobBundle>, Avatars)> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(cas) = blob_cas::BlobCas::open_for_render(raw_db_path)
                .await
                .with_context(|| format!("open the blob store beside {}", raw_db_path.display()))?
            else {
                return Ok((HashMap::new(), Avatars::new()));
            };
            let result = async {
                let mut out: HashMap<String, BlobBundle> = HashMap::new();
                for doc in &parsed.docs {
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
                            described.content_type =
                                described.content_type.or_else(|| b.content_type.clone());
                            bundle.add_stored(hash, cas.step(), described, Some(b.slot.clone()));
                        }
                    }
                    if !bundle.is_empty() || bundle.has_missing() {
                        out.insert(bundle_key(doc), bundle);
                    }
                }
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
                Ok::<_, anyhow::Error>((out, avatars))
            }
            .await;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            cas.close().await;
            result
        })
    })
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
