//! A chat-shaped render's attachment bytes, loaded a batch of chats at a
//! time just before the batch renders, rather than every chat's up front:
//! `docs/dev/data_architecture_parse_and_render.md` §"Memory: a page at a
//! time". A chat's bundle stays whole — its period pages share one
//! `blobs/` folder and the filenames its bundle picks — so one very large
//! chat is still one batch.

use std::collections::HashMap;
use std::future::Future;

use anyhow::Result;
use datalib_etl::blob_cas::{self, BlobBundle};
use datalib_etl::doltlite_raw;
use datalib_etl_render::pages::batches;
use sqlx::SqlitePool;
use std::path::PathBuf;

use crate::changed::{changed_chats, RenderOutcome, RenderTarget};
use crate::render::{ChatRenderer, RenderProfile};
use crate::types::NormalizedChat;
use datalib_etl::doltlite_raw::DiffScan;
use datalib_etl_render::inputs::RawRange;

/// Where a pass reads its chats' attachment bytes: the raw store whose
/// edge table names each ref's hash, and the blob store beside it, both
/// open for the pass. A chat's refs are its attachments' `ref_id`s.
pub struct ChatBlobs<'a> {
    refs_pool: &'a SqlitePool,
    cas_pool: Option<&'a SqlitePool>,
    /// The `BlobBundle::load_many` projection for a chat: most sources
    /// have one edge table; Google Takeout has one per kind of chat.
    projection: &'a (dyn Fn(&NormalizedChat) -> &'static str + Sync),
}

impl<'a> ChatBlobs<'a> {
    pub fn new(
        refs_pool: &'a SqlitePool,
        cas_pool: Option<&'a SqlitePool>,
        projection: &'a (dyn Fn(&NormalizedChat) -> &'static str + Sync),
    ) -> Self {
        Self {
            refs_pool,
            cas_pool,
            projection,
        }
    }

    /// What each chat's bytes weigh, in order.
    pub fn weights(&self, chats: &[NormalizedChat]) -> Result<Vec<u64>> {
        let chats: Vec<&NormalizedChat> = chats.iter().collect();
        let mut weights = vec![0u64; chats.len()];
        for (projection, members) in self.by_projection(&chats) {
            let refs = members.iter().map(|&i| (i, refs_of(chats[i])));
            let sizes = block_on(BlobBundle::sizes_many(
                self.refs_pool,
                self.cas_pool,
                projection,
                refs,
            ))?;
            for (i, bytes) in sizes {
                weights[i] = bytes;
            }
        }
        Ok(weights)
    }

    /// Each chat's bundle, by chat id.
    pub fn load(&self, chats: &[&NormalizedChat]) -> Result<HashMap<String, BlobBundle>> {
        let mut out = HashMap::new();
        for (projection, members) in self.by_projection(chats) {
            let refs = members
                .iter()
                .map(|&i| (chats[i].id.clone(), refs_of(chats[i])));
            out.extend(block_on(BlobBundle::load_many(
                self.refs_pool,
                self.cas_pool,
                projection,
                refs,
            ))?);
        }
        Ok(out)
    }

    /// `chats`' indices, grouped by the projection each is read through.
    fn by_projection(&self, chats: &[&NormalizedChat]) -> Vec<(&'static str, Vec<usize>)> {
        let mut out: Vec<(&'static str, Vec<usize>)> = Vec::new();
        for (i, chat) in chats.iter().enumerate() {
            let projection = (self.projection)(chat);
            match out.iter_mut().find(|(p, _)| *p == projection) {
                Some((_, members)) => members.push(i),
                None => out.push((projection, vec![i])),
            }
        }
        out
    }
}

fn refs_of(chat: &NormalizedChat) -> Vec<String> {
    let mut refs: Vec<String> = chat
        .buckets
        .iter()
        .flat_map(|doc| &doc.items)
        .flat_map(|item| &item.attachments)
        .filter_map(|att| att.ref_id.clone())
        .collect();
    refs.sort();
    refs.dedup();
    refs
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Renders `chats`, loading their bytes from `blobs` a batch of at most
/// `blob_bytes` at a time.
pub fn render_batched(
    renderer: &mut ChatRenderer<'_>,
    chats: &[NormalizedChat],
    blobs: &ChatBlobs<'_>,
    blob_bytes: u64,
) -> Result<()> {
    let weights = blobs.weights(chats)?;
    let empty = BlobBundle::default();
    for range in batches(&weights, blob_bytes) {
        let batch: Vec<&NormalizedChat> = chats[range].iter().collect();
        let loaded = blobs.load(&batch)?;
        for chat in batch {
            renderer.render_chat(chat, loaded.get(&chat.id).unwrap_or(&empty))?;
        }
    }
    Ok(())
}

/// The raw store a pass's chats were built from, and the commit they were
/// built at: where [`render_batched_at`] reads their bytes.
#[derive(Debug, Clone)]
pub struct BlobsAt {
    pub db_path: PathBuf,
    pub commit: String,
}

/// [`render_batched`] reading `chats`' bytes at `at`, through the raw
/// store and the blob store beside it, both open for the call only.
pub fn render_batched_at(
    at: &BlobsAt,
    projection: &(dyn Fn(&NormalizedChat) -> &'static str + Sync),
    renderer: &mut ChatRenderer<'_>,
    chats: &[NormalizedChat],
    blob_bytes: u64,
) -> Result<()> {
    let BlobsAt { db_path, commit } = at;
    let Some(reader) = block_on(doltlite_raw::open_reader(db_path, Some(commit)))? else {
        return Ok(());
    };
    let cas = match block_on(blob_cas::open_cas_for_render(db_path)) {
        Ok(cas) => cas,
        Err(e) => {
            block_on(reader.close());
            return Err(e.context(format!("open the blob store beside {}", db_path.display())));
        }
    };
    let blobs = ChatBlobs::new(reader.pool(), cas.as_ref(), projection);
    let rendered = render_batched(renderer, chats, &blobs, blob_bytes);
    block_on(async {
        reader.close().await;
        if let Some(cas) = cas {
            cas.close().await;
        }
    });
    rendered
}

/// [`crate::render_changed`], with each chat's bytes loaded a batch at a
/// time instead of handed in for every chat.
#[allow(clippy::too_many_arguments)]
pub fn render_changed_batched(
    profile: &RenderProfile,
    all: Vec<NormalizedChat>,
    scan: DiffScan,
    range: RawRange<'_>,
    uuid_of: impl Fn(&str) -> String,
    blobs: &ChatBlobs<'_>,
    blob_bytes: u64,
    target: RenderTarget<'_>,
) -> Result<RenderOutcome> {
    let mut changed = changed_chats(all, range, scan.render.as_ref(), uuid_of);
    let docs_total = changed.chats.iter().map(|c| c.buckets.len()).sum();
    let mut renderer = ChatRenderer::new(
        profile,
        target.out_root,
        target.source_id,
        docs_total,
        target.progress,
        target.on_doc_complete,
    );
    render_batched(&mut renderer, &changed.chats, blobs, blob_bytes)?;
    let summary = renderer.finish();
    changed.buckets.extend(summary.buckets);
    Ok(RenderOutcome {
        rendered: summary.docs_rendered,
        skipped: changed.skipped,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
        buckets: changed.buckets,
    })
}
