//! An agent-session render, a page of transcripts at a time. The
//! `transcripts` rows (one small row per session file) are read whole up
//! front; a page's `records` only when it renders, and only for the
//! transcripts this run renders, so what a run holds follows the page
//! and not the store (`docs/dev/data_architecture_parse_and_render.md`
//! §"Memory: a page at a time").

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::doltlite_raw::{self, DiffScanSpec};
use datalib_etl::pin::Pin;
use datalib_etl_chat_common::render::{ChatRenderer, RenderProfile};
use datalib_etl_chat_common::types::NormalizedChat;
use datalib_etl_chat_common::RenderTarget;
use datalib_etl_render::inputs::{Bucket, Buckets, RawRange};
use datalib_etl_render::pages::{batches, PageSize};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::TRANSCRIPT_BUCKETS_SQL;

/// What a record's JSON comes to on average: 4.3 KB across a 2.2 GB
/// Claude Code store, with no session's average over four times that.
/// It turns a page's byte budget into a count of records, which the
/// index answers without reading one.
const RECORD_BYTES: u64 = 4 * 1024;

/// One agent-session source's part of a paged render: how its records
/// load and how a page of them becomes chats.
pub trait SessionRender {
    type Record;

    fn profile(&self) -> RenderProfile;

    /// The document id for a raw transcript id, with no row in hand.
    fn chat_uuid(&self, transcript_id: &str) -> String;

    /// The records of the transcripts named in `ids`, a JSON array to
    /// bind as one value.
    fn load_records(
        &self,
        pool: &SqlitePool,
        ids: &str,
    ) -> impl Future<Output = Result<Vec<Self::Record>>>;

    /// One chat per transcript in `page`, from `records`. `titles` is
    /// every transcript's, for a subagent named after its parent.
    fn build_chats(
        &self,
        page: &[(String, Value)],
        records: &[Self::Record],
        titles: &HashMap<String, String>,
    ) -> Vec<NormalizedChat>;
}

/// What a paged render did. The processor stamps `new_head` once
/// every page's buckets are declared.
#[derive(Debug, Default)]
pub struct PagedOutcome {
    pub rendered: usize,
    /// Transcripts left alone: nothing they read changed.
    pub skipped: usize,
    pub new_head: Option<String>,
}

/// Every transcript's title, by transcript id.
pub fn titles(transcripts: &[(String, Value)]) -> HashMap<String, String> {
    transcripts
        .iter()
        .filter_map(|(id, meta)| crate::str_of(meta, "title").map(|t| (id.clone(), t.to_string())))
        .collect()
}

/// Renders the transcripts in the store at `db_path` that this run has
/// to, a page at a time. `on_buckets` hears each page's buckets before
/// the next page is read; its first call names, with nothing, the ones
/// whose transcript is gone, so their documents go.
pub fn render_paged<S: SessionRender>(
    source: &S,
    db_path: &Path,
    range: RawRange<'_>,
    size: PageSize,
    target: RenderTarget<'_>,
    on_buckets: &mut dyn FnMut(&[Bucket]) -> Result<()>,
) -> Result<PagedOutcome> {
    if !db_path.exists() {
        return Ok(PagedOutcome::default());
    }
    // Pinned at open; no commit means nothing committed to render.
    let Some(reader) = block_on(doltlite_raw::open_reader(db_path, range.pin))? else {
        return Ok(PagedOutcome::default());
    };
    let rendered = render_open(
        source,
        reader.pool(),
        reader.pin(),
        range,
        size,
        target,
        on_buckets,
    );
    // Closed, not dropped, on the error path too: the next open of this
    // store is a second connection until this one is gone.
    block_on(reader.close());
    rendered
}

fn render_open<S: SessionRender>(
    source: &S,
    pool: &SqlitePool,
    pin: &Pin,
    range: RawRange<'_>,
    size: PageSize,
    target: RenderTarget<'_>,
    on_buckets: &mut dyn FnMut(&[Bucket]) -> Result<()>,
) -> Result<PagedOutcome> {
    let (transcripts, counts, scan) = block_on(async {
        let transcripts = doltlite_raw::load_payloads_with_id(pool, "transcripts").await?;
        let counts = record_counts(pool).await?;
        let scan = doltlite_raw::scan_buckets(
            pool,
            range.cursor,
            pin,
            &DiffScanSpec {
                global_fanout_tables: &[],
                bucket_query: TRANSCRIPT_BUCKETS_SQL,
            },
        )
        .await?;
        anyhow::Ok((transcripts, counts, scan))
    })?;

    let titles = titles(&transcripts);
    // No early return on an empty store: a run that deleted every
    // transcript still has to declare their buckets empty so the
    // documents go.
    let plan = plan(transcripts, range, scan.render.as_ref(), |id| {
        source.chat_uuid(id)
    });
    on_buckets(&plan.gone)?;

    let profile = source.profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        target.out_root,
        target.source_id,
        plan.render.len(),
        target.progress,
        target.on_doc_complete,
    );
    let weights: Vec<u64> = plan
        .render
        .iter()
        .map(|(id, _)| counts.get(id).copied().unwrap_or(0))
        .collect();
    let no_blobs = BlobBundle::default();
    for page in pages(&weights, size) {
        let page = &plan.render[page];
        let ids: Vec<&str> = page.iter().map(|(id, _)| id.as_str()).collect();
        let ids = serde_json::to_string(&ids)?;
        let records = block_on(source.load_records(pool, &ids))?;
        for chat in source.build_chats(page, &records, &titles) {
            renderer.render_chat(&chat, &no_blobs)?;
        }
        on_buckets(&renderer.take_buckets())?;
    }
    Ok(PagedOutcome {
        rendered: renderer.finish().docs_rendered,
        skipped: plan.skipped,
        new_head: scan.new_head,
    })
}

/// The transcripts a run renders, and the buckets it names empty first.
#[derive(Debug)]
struct Plan {
    render: Vec<(String, Value)>,
    /// The changed transcripts with no row left, as buckets with no
    /// inputs.
    gone: Buckets,
    skipped: usize,
}

/// Cut `transcripts` down to the ones the provider's diff names in
/// `changed` and the ones the driver found stale by document id.
fn plan(
    transcripts: Vec<(String, Value)>,
    range: RawRange<'_>,
    changed: Option<&HashSet<String>>,
    uuid_of: impl Fn(&str) -> String,
) -> Plan {
    let id_by_uuid: HashMap<String, &str> = transcripts
        .iter()
        .map(|(id, _)| (uuid_of(id), id.as_str()))
        .collect();
    let narrowed = range.narrow_by(changed, |key| id_by_uuid.get(key).map(|id| id.to_string()));
    let Some(wanted) = narrowed.render else {
        return Plan {
            render: transcripts,
            gone: Vec::new(),
            skipped: 0,
        };
    };
    let total = transcripts.len();
    let render: Vec<(String, Value)> = transcripts
        .into_iter()
        .filter(|(id, _)| wanted.contains(id))
        .collect();
    let held: HashSet<&str> = render.iter().map(|(id, _)| id.as_str()).collect();
    let gone = wanted
        .iter()
        .filter(|id| !held.contains(id.as_str()))
        .map(|id| uuid_of(id))
        .chain(narrowed.gone)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .map(|key| Bucket {
            key,
            inputs: Vec::new(),
        })
        .collect();
    Plan {
        skipped: total - render.len(),
        render,
        gone,
    }
}

/// `weights` (records per transcript) cut into pages of at most
/// `size.bytes` worth of records and `size.buckets` transcripts. One
/// transcript over the budget is a page of its own.
fn pages(weights: &[u64], size: PageSize) -> Vec<Range<usize>> {
    let per_page = size.buckets.max(1);
    let mut out = Vec::new();
    for batch in batches(weights, (size.bytes / RECORD_BYTES).max(1)) {
        let mut start = batch.start;
        while start < batch.end {
            let end = (start + per_page).min(batch.end);
            out.push(start..end);
            start = end;
        }
    }
    out
}

/// How many records each transcript has, from the `records_transcript`
/// index: no record is read.
async fn record_counts(pool: &SqlitePool) -> Result<HashMap<String, u64>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT transcript_id, count(*) FROM records GROUP BY transcript_id")
            .fetch_all(pool)
            .await
            .context("count each transcript's records")?;
    Ok(rows
        .into_iter()
        .map(|(id, n)| (id, n.max(0) as u64))
        .collect())
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transcripts(ids: &[&str]) -> Vec<(String, Value)> {
        ids.iter()
            .map(|id| (id.to_string(), json!({ "title": id })))
            .collect()
    }

    fn uuid(id: &str) -> String {
        format!("u-{id}")
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.render.iter().map(|(id, _)| id.as_str()).collect()
    }

    fn keys(buckets: &Buckets) -> Vec<&str> {
        buckets.iter().map(|b| b.key.as_str()).collect()
    }

    /// The narrowed run loads only the transcripts the diff or the
    /// driver named, and names the ones with no row left empty, by the
    /// id they rendered under.
    #[test]
    fn a_narrowed_run_renders_only_what_changed() {
        let changed: HashSet<String> = ["b", "gone"].map(String::from).into();
        let stale: HashSet<String> = ["u-c", "u-deleted"].map(String::from).into();
        let range = RawRange {
            cursor: Some("abc"),
            pin: None,
            stale: Some(&stale),
        };
        let plan = plan(
            transcripts(&["a", "b", "c", "d"]),
            range,
            Some(&changed),
            uuid,
        );
        assert_eq!(ids(&plan), ["b", "c"]);
        assert_eq!(plan.skipped, 2);
        assert_eq!(keys(&plan.gone), ["u-deleted", "u-gone"]);
        assert!(plan.gone.iter().all(|b| b.inputs.is_empty()));
    }

    #[test]
    fn a_run_that_cannot_narrow_renders_everything() {
        let plan = plan(transcripts(&["a", "b"]), RawRange::cold(), None, uuid);
        assert_eq!(ids(&plan), ["a", "b"]);
        assert_eq!(plan.skipped, 0);
        assert!(plan.gone.is_empty());
    }

    #[test]
    fn pages_hold_a_budget_of_records_and_a_count_of_transcripts() {
        let size = PageSize {
            buckets: 2,
            bytes: 10 * RECORD_BYTES,
        };
        assert_eq!(
            pages(&[4, 4, 4, 1, 1, 1, 1, 30, 0], size),
            vec![0..2, 2..4, 4..6, 6..7, 7..8, 8..9]
        );
        assert!(pages(&[], size).is_empty());
    }
}
