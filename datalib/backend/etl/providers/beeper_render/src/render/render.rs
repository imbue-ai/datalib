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

use super::normalize::{bundle_key, to_networks};
use super::parse::ParsedBeeper;

/// Bump when Beeper's own contribution to the rendered output changes.
/// The shared layout has its own number — see
/// `datalib_etl_chat_common::LAYOUT_VERSION`.
/// v3: ids are minted through `datalib_id` under `ProviderGlobal`,
///     every row carries its backpointer, and an event's id carries its
///     `timestamp_ms` in its leading bits (`datalib_id`'s v8 layout).
///     The raw store's keys are the same ids, so an existing root
///     re-ingests; every uuid moved, `chat_uuid` among them.
pub const RENDER_VERSION: u32 = 3;

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

    let blobs_by_chat = load_blobs(parsed, raw_db_path)?;
    let blobs_materialized = blobs_by_chat.values().map(BlobBundle::len).sum();

    let mut summary = RenderSummary {
        docs_total: parsed.docs.len(),
        blobs_materialized,
        ..Default::default()
    };

    for network in to_networks(parsed, source_id) {
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
/// them up.
///
/// Beeper's parse resolves a blob's `blake3` but not its bytes, so this
/// is the one place that reads the CAS. `blake3` doubles as the
/// `ref_id`: it is what the normalized attachment carries, and what the
/// bundle is keyed on.
fn load_blobs(parsed: &ParsedBeeper, raw_db_path: &Path) -> Result<HashMap<String, BlobBundle>> {
    let cas_path = blob_cas::cas_path_for(raw_db_path);
    if !cas_path.is_file() {
        return Ok(HashMap::new());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let cas = blob_cas::BlobCas::open_reader(&cas_path)
                .await
                .with_context(|| format!("open CAS at {}", cas_path.display()))?;
            let result = async {
                let mut out: HashMap<String, BlobBundle> = HashMap::new();
                for doc in &parsed.docs {
                    let mut bundle = BlobBundle::new();
                    for m in &doc.messages {
                        for b in &m.blobs {
                            let Some(hash) = b.blake3.as_deref() else {
                                continue;
                            };
                            let Some(obj) = cas.get(hash).await? else {
                                continue;
                            };
                            bundle.add(
                                hash,
                                obj.bytes,
                                obj.content_type.or_else(|| b.content_type.clone()),
                                Some(b.slot.clone()),
                            );
                        }
                    }
                    if !bundle.is_empty() {
                        out.insert(bundle_key(doc), bundle);
                    }
                }
                Ok::<_, anyhow::Error>(out)
            }
            .await;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            cas.close().await;
            result
        })
    })
}
