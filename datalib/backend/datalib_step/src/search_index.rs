//! The search index (docs/dev/plans/search_index_from_ram.md): what each
//! source's `keyword_index` and `embed`, and the aggregator, do with it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use datalib_embedder::{Device, Embedder};
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl_render::indexed_markdown::{index_key, IndexedMarkdownStore};
use datalib_runtime::layout;
use datalib_schema::markdowns::MarkdownRow;
#[cfg(test)]
use datalib_search_index::text::{self, Piece, INDEX_TEXT_VERSION};
use datalib_search_index::{Index, IndexDoc, InferenceCache, Reader};

/// Documents read and indexed together, one transaction each.
const DOCUMENT_BATCH: usize = 200;
/// Chunks embedded between two writes to the cache, and two checks for a stop.
const CHUNK_BATCH: usize = 32;

/// What a keyword pass or an embed did, for its log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Done {
    pub(crate) put: usize,
    pub(crate) removed: usize,
    pub(crate) embedded: usize,
    /// Documents whose file and render store disagree, so they are left for
    /// the next pass: the render is mid-write, or the file was touched.
    pub(crate) unsettled: usize,
    /// Chunks longer than the model's context, cut to fit before they were
    /// embedded: dense text, code or encoded data, whose tail is lost.
    pub(crate) truncated: usize,
    /// The text the model read, and how long it took: the embed's speed.
    pub(crate) prompt_bytes: usize,
    pub(crate) embed_time: Duration,
}

impl Done {
    fn add(&mut self, batch: &Embedded) {
        self.embedded += batch.vectors.len();
        self.truncated += batch.truncated;
        self.prompt_bytes += batch.prompt_bytes;
        self.embed_time += batch.took;
    }

    /// Chunks and kilobytes of prompt text a second of the model's time,
    /// or `None` before it has embedded anything.
    pub(crate) fn rate(&self) -> Option<(f64, f64)> {
        let secs = self.embed_time.as_secs_f64();
        (self.embedded > 0 && secs > 0.0).then(|| {
            (
                self.embedded as f64 / secs,
                self.prompt_bytes as f64 / 1000.0 / secs,
            )
        })
    }
}

/// One batch's vectors, and what it cost the model.
struct Embedded {
    vectors: Vec<(String, Vec<f32>)>,
    truncated: usize,
    prompt_bytes: usize,
    took: Duration,
}

/// A document the search covers: one with a file, outside `unindexed/`.
pub(crate) fn searchable(row: &MarkdownRow) -> bool {
    row.md_path.as_deref().is_some_and(|p| {
        !Path::new(p)
            .components()
            .any(|c| c.as_os_str() == layout::UNINDEXED_DIR)
    })
}

/// This source's searchable `markdowns` rows at its render store's head,
/// by id. Empty when the store has nothing committed; `None` when an older
/// build wrote it, which its next render rewrites in this build's shape.
async fn rendered(root: &Path, group: &str) -> Result<Option<BTreeMap<String, MarkdownRow>>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, group);
    let group = group.to_string();
    tokio::task::spawn_blocking(move || {
        let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
            return Ok(Some(BTreeMap::new()));
        };
        let rows = match store.in_this_shape() {
            Ok(true) => store.markdown_rows().map(Some),
            Ok(false) => Ok(None),
            Err(e) => Err(e),
        };
        store.close();
        let Some(rows) = rows? else {
            tracing::warn!(
                group = %group,
                "the render store was written by an older build; the search index takes \
                 this source once its render has rewritten it"
            );
            return Ok(None);
        };
        Ok(Some(
            rows.into_iter()
                .filter(searchable)
                .map(|r| (r.markdown_uuid.clone(), r))
                .collect(),
        ))
    })
    .await
    .context("render store read panicked")?
}

/// `IndexedMarkdownStore::index_docs` off the runtime, opening the store at
/// its head.
async fn draw_batch(root: &Path, group: &str, uuids: Vec<String>) -> Result<Vec<IndexDoc>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, group);
    let group = group.to_string();
    tokio::task::spawn_blocking(move || {
        let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
            return Ok(Vec::new());
        };
        let out = store.index_docs(&group, &uuids);
        store.close();
        out
    })
    .await
    .context("render store read panicked")?
}

/// `<group>/keyword_index`'s half: this source's documents in the search
/// index, brought in line with its render store.
pub(crate) async fn update_keywords(
    root: &Path,
    group: &str,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    retire_separate_file(root)?;
    let Some(rows) = rendered(root, group).await? else {
        return Ok(Done::default());
    };
    let index = Index::open(&layout::grid_index_db(root)).await?;
    let out = update_keywords_in(&index, root, group, &rows, progress, stop).await;
    index.close().await;
    out
}

async fn update_keywords_in(
    index: &Index,
    root: &Path,
    group: &str,
    rows: &BTreeMap<String, MarkdownRow>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let indexed = index.indexed(group).await?;
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for (uuid, row) in rows {
        match index_key(row) {
            Some(key) => {
                current.insert(uuid.clone(), key);
            }
            None => done.unsettled += 1,
        }
    }
    let owed = datalib_search_index::owed(&current, &indexed);
    progress.set_message("search index: keywords");
    progress.set_length(Some(owed.put.len() as u64));
    for batch in owed.put.chunks(DOCUMENT_BATCH) {
        if stop.requested() {
            break;
        }
        let docs = draw_batch(root, group, batch.to_vec()).await?;
        done.unsettled += batch.len() - docs.len();
        index.put(&docs).await?;
        done.put += docs.len();
        progress.inc(batch.len() as u64);
    }
    index.remove(&owed.remove).await?;
    done.removed = owed.remove.len();
    Ok(done)
}

/// `<group>/embed`'s half: every chunk of this source the inference cache
/// has no vector for, embedded and cached.
pub(crate) async fn embed(
    root: &Path,
    group: &str,
    model_path: &Path,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    // Read-only: an embed writes only the cache, and a writer of the
    // index's file (a keyword pass, the grid's load) must not hold it up.
    let Some(reader) = Reader::open(
        &layout::grid_index_db(root),
        &layout::inference_cache_db(root),
    )
    .await?
    else {
        return Ok(Done::default());
    };
    let spans = reader.chunk_hashes(group).await;
    reader.close().await;
    let spans = spans?;
    let cache = InferenceCache::open(&layout::inference_cache_db(root)).await?;
    let out = embed_into(&cache, root, group, model_path, spans, progress, stop).await;
    cache.close().await;
    out
}

async fn embed_into(
    cache: &InferenceCache,
    root: &Path,
    group: &str,
    model_path: &Path,
    spans: Vec<(String, String)>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let model = datalib_search_index::model_key();
    let hashes: Vec<String> = spans.iter().map(|(_, hash)| hash.clone()).collect();
    let missing: HashSet<String> = cache.missing(&model, &hashes).await?.into_iter().collect();
    if missing.is_empty() {
        return Ok(done);
    }
    let mut by_doc: BTreeMap<&str, HashSet<&str>> = BTreeMap::new();
    let mut queued = HashSet::new();
    for (doc, hash) in &spans {
        if missing.contains(hash) && queued.insert(hash.as_str()) {
            by_doc.entry(doc).or_default().insert(hash);
        }
    }
    progress.set_message("search index: embedding");
    progress.set_length(Some(queued.len() as u64));

    let model_path = model_path.to_path_buf();
    let mut embedder = Some(
        tokio::task::spawn_blocking(move || Embedder::open(&model_path, Device::Best, 0))
            .await
            .context("embedder open panicked")??,
    );
    if let Some(embedder) = &embedder {
        tracing::info!(
            device = %embedder.device(),
            cuda = datalib_embedder::cuda_module_report().unwrap_or("not looked for"),
            "loaded the search model"
        );
    }
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut result = Ok(());
    let owing: Vec<String> = by_doc.keys().map(|u| u.to_string()).collect();
    'docs: for uuids in owing.chunks(DOCUMENT_BATCH) {
        for doc in draw_batch(root, group, uuids.to_vec()).await? {
            let wanted = &by_doc[doc.markdown_uuid.as_str()];
            let prompts = prompts_for(&doc, wanted);
            let Some(prompts) = prompts else {
                done.unsettled += 1;
                continue;
            };
            pending.extend(prompts);
            while pending.len() >= CHUNK_BATCH || (!pending.is_empty() && stop.requested()) {
                let batch: Vec<_> = pending.drain(..pending.len().min(CHUNK_BATCH)).collect();
                match embed_batch(&mut embedder, batch).await {
                    Ok(batch) => {
                        cache.put(&model, &batch.vectors).await?;
                        done.add(&batch);
                        progress.inc(batch.vectors.len() as u64);
                    }
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
                if stop.requested() {
                    break;
                }
            }
            if result.is_err() || stop.requested() {
                break 'docs;
            }
        }
    }
    if result.is_ok() && !pending.is_empty() && !stop.requested() {
        result = match embed_batch(&mut embedder, std::mem::take(&mut pending)).await {
            Ok(batch) => {
                let put = cache.put(&model, &batch.vectors).await;
                done.add(&batch);
                progress.inc(batch.vectors.len() as u64);
                put
            }
            Err(e) => Err(e),
        };
    }
    // On Metal, ggml aborts at exit while a model is still loaded.
    drop(embedder.take());
    result.map(|()| done)
}

/// `(hash, prompt)` for each chunk of `doc` the cache lacks, or `None`
/// when the document no longer draws one of them: it changed after the
/// keyword pass, and the next one re-chunks it.
fn prompts_for(doc: &IndexDoc, wanted: &HashSet<&str>) -> Option<Vec<(String, String)>> {
    let prompts: BTreeMap<String, String> = doc
        .prompted()
        .into_iter()
        .filter(|p| wanted.contains(p.hash.as_str()))
        .map(|p| (p.hash, p.prompt))
        .collect();
    (prompts.len() == wanted.len()).then(|| prompts.into_iter().collect())
}

/// Each prompt's vector, and how many had to be cut to fit the context.
async fn embed_batch(
    embedder: &mut Option<Embedder>,
    batch: Vec<(String, String)>,
) -> Result<Embedded> {
    let mut e = embedder.take().context("the embedder is gone")?;
    let prompt_bytes = batch.iter().map(|(_, prompt)| prompt.len()).sum();
    let started = Instant::now();
    let (e, out) = tokio::task::spawn_blocking(move || {
        let mut cut = 0;
        let out: Result<Vec<_>> = batch
            .into_iter()
            .map(|(hash, prompt)| {
                let (vector, truncated) = e.embed_fitted(&prompt)?;
                cut += usize::from(truncated);
                Ok((hash, vector))
            })
            .collect();
        (e, out.map(|v| (v, cut)))
    })
    .await
    .context("embedding panicked")?;
    let took = started.elapsed();
    *embedder = Some(e);
    out.map(|(vectors, truncated)| Embedded {
        vectors,
        truncated,
        prompt_bytes,
        took,
    })
}

/// The aggregator's half: drop every source it no longer reads, then let
/// the cache keep only what the index still names. Touches nothing that
/// does not exist yet.
pub(crate) async fn sweep(root: &Path, groups: &[String]) -> Result<(usize, u64)> {
    retire_separate_file(root)?;
    let index_path = layout::grid_index_db(root);
    if !index_path.exists() {
        return Ok((0, 0));
    }
    let index = Index::open(&index_path).await?;
    let kept = sweep_index(&index, groups).await;
    index.close().await;
    let (removed, hashes) = kept?;
    let cache_path = layout::inference_cache_db(root);
    if !cache_path.exists() {
        return Ok((removed, 0));
    }
    let cache = InferenceCache::open(&cache_path).await?;
    let gone = cache
        .keep_only(&datalib_search_index::model_key(), &hashes)
        .await;
    cache.close().await;
    Ok((removed, gone?))
}

async fn sweep_index(index: &Index, groups: &[String]) -> Result<(usize, HashSet<String>)> {
    let removed = index.keep_sources(groups).await?;
    Ok((removed, index.all_hashes().await?))
}

/// Deletes the file the search index was before its tables moved into the
/// grid index's: what it held is made again in the grid index's file.
fn retire_separate_file(root: &Path) -> Result<()> {
    let old = layout::retired_search_index_db(root);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", old.display()));
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(
                path = %path.display(),
                "removed the search index's old file; its tables are in the grid index's now"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
}

/// Where the pinned embedding model is, once provisioned.
pub(crate) fn model_file(models_dir: &Path) -> PathBuf {
    models_dir.join(datalib_runtime::qmd::EMBED_MODELS[0].cache_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::grid_index::{doc_md_path, unindexed_doc_md_path, RenderedMarkdown};
    use datalib_etl_render::section::{join, Section};
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;

    const GROUP: &str = "ten-forward";

    fn render_root(root: &Path) -> PathBuf {
        datalib_etl::layout::render_markdown_root(root, GROUP)
    }

    /// A document as a renderer hands it over, its file written.
    fn rendered_doc(
        root: &Path,
        uuid: &str,
        sections: Vec<Section>,
        md_path: PathBuf,
    ) -> RenderedMarkdown {
        std::fs::create_dir_all(md_path.parent().unwrap()).unwrap();
        std::fs::write(&md_path, join(&sections)).unwrap();
        let rel = md_path.strip_prefix(root).unwrap().display().to_string();
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .conversation_name(Some("Ten Forward".to_string()))
            .entire_chat(format!("/chat/{uuid}"))
            .body("body")
            .markdown_uuid(Some(uuid.to_string()))
            .qmd_path(Some(rel))
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        RenderedMarkdown {
            section_layout: None,
            content_hash: None,
            markdown_uuid: uuid.to_string(),
            source_id: GROUP.to_string(),
            upstream_cursor: None,
            bucket_key: None,
            md_path,
            render_version: 1,
            rows: vec![row],
            sections,
            front: Default::default(),
            edges: Vec::new(),
            contacts: Vec::new(),
            data_nodes: Vec::new(),
            filings: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn thread(text: &str) -> Vec<Section> {
        vec![
            Section::unkeyed("---\n---\n\n# Ten Forward\n\n".into()),
            Section::keyed(
                "m1",
                format!("<div data-section-uuid=\"m1\">\n\n{text}\n\n</div>\n"),
            ),
        ]
    }

    fn commit(root: &Path, docs: &[RenderedMarkdown], removed: &[String]) {
        let store = IndexedMarkdownStore::open(&render_root(root)).unwrap();
        for d in docs {
            store.put_document(root, d).unwrap();
        }
        if !removed.is_empty() {
            store.remove_documents(root, removed).unwrap();
        }
        store.commit("test").unwrap();
        store.close();
    }

    async fn update(root: &Path) -> Done {
        update_keywords(root, GROUP, &Progress::noop(), &StopFlag::new())
            .await
            .unwrap()
    }

    async fn indexed(root: &Path) -> BTreeMap<String, String> {
        let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
        let out = index.indexed(GROUP).await.unwrap();
        index.close().await;
        out
    }

    /// The keyword pass takes what the render store holds, by its content
    /// hash: a new document is put, a changed one put again, an unchanged
    /// one left, a removed one removed, and one filed under `unindexed/`
    /// never indexed.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_keyword_pass_follows_the_render_store() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let rr = render_root(root);
        let tea = rendered_doc(
            root,
            "d-tea",
            thread("Tea, Earl Grey"),
            doc_md_path(&rr, "d-tea"),
        );
        let hush = rendered_doc(
            root,
            "d-hush",
            thread("Shields up"),
            doc_md_path(&rr, "d-hush"),
        );
        let quiet = rendered_doc(
            root,
            "d-quiet",
            thread("a repeat"),
            unindexed_doc_md_path(&rr, "d-quiet"),
        );
        commit(root, &[tea.clone(), hush.clone(), quiet], &[]);

        assert_eq!(
            update(root).await,
            Done {
                put: 2,
                ..Done::default()
            }
        );
        let held = indexed(root).await;
        let file = std::fs::read(&tea.md_path).unwrap();
        assert_eq!(
            held["d-tea"],
            format!("{}.t{INDEX_TEXT_VERSION}", blake3::hash(&file).to_hex())
        );
        assert!(!held.contains_key("d-quiet"));

        assert_eq!(update(root).await, Done::default(), "nothing changed");

        let tea2 = rendered_doc(root, "d-tea", thread("Tea, hot"), doc_md_path(&rr, "d-tea"));
        commit(root, &[tea2], &["d-hush".to_string()]);
        assert_eq!(
            update(root).await,
            Done {
                put: 1,
                removed: 1,
                ..Done::default()
            }
        );
        assert_eq!(indexed(root).await.keys().collect::<Vec<_>>(), ["d-tea"]);
    }

    /// A root upgraded from a build that kept the search index in a file
    /// of its own: the keyword pass deletes that file and writes the index
    /// into the grid index's.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_indexs_old_file_goes_and_its_tables_are_in_the_grid_index() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let old = layout::retired_search_index_db(root);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"an older build's index").unwrap();
        std::fs::write(format!("{}-wal", old.display()), b"its log").unwrap();
        let rr = render_root(root);
        let tea = rendered_doc(root, "d-tea", thread("Tea"), doc_md_path(&rr, "d-tea"));
        commit(root, &[tea], &[]);

        update(root).await;
        assert!(!old.exists());
        assert!(!std::path::Path::new(&format!("{}-wal", old.display())).exists());
        assert_eq!(indexed(root).await.len(), 1);
        assert!(layout::grid_index_db(root).exists());
    }

    /// A render store an older build wrote has neither a document's hash
    /// nor its layout, and its next render rewrites it. Until then the
    /// keyword pass leaves the source as indexed: it neither fails the step
    /// on the missing columns nor reads the store as empty and removes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_render_store_in_an_older_shape_is_left_until_it_is_rendered_again() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let rr = render_root(root);
        let tea = rendered_doc(
            root,
            "d-tea",
            thread("Tea, Earl Grey"),
            doc_md_path(&rr, "d-tea"),
        );
        commit(root, &[tea], &[]);
        assert_eq!(update(root).await.put, 1);

        let writer = datalib_etl::doltlite_raw::open_derived(
            &datalib_etl_render::indexed_markdown::path_for(&rr),
            &[],
            datalib_etl::doltlite_raw::StoreKind::Render,
        )
        .await
        .unwrap();
        for sql in [
            "ALTER TABLE markdowns DROP COLUMN content_hash",
            "ALTER TABLE markdowns DROP COLUMN section_layout",
            "UPDATE _datalib_meta SET value = 'an-older-shape' WHERE key = 'schema_hash'",
        ] {
            sqlx::query(sql).execute(&writer).await.unwrap();
        }
        datalib_etl::doltlite_raw::commit_run(&writer, "an older build's shape")
            .await
            .unwrap();
        writer.close().await;

        assert_eq!(update(root).await, Done::default());
        assert_eq!(indexed(root).await.keys().collect::<Vec<_>>(), ["d-tea"]);
    }

    /// The index draws a document's text from the render store, not its
    /// file: an item is its envelope and its body, with none of the markup
    /// the file draws around it, and a piece kept whole is drawn as it is.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_draws_from_the_store_not_the_file() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let rr = render_root(root);
        let mut tea = rendered_doc(root, "d-tea", Vec::new(), doc_md_path(&rr, "d-tea"));
        tea.sections = vec![
            Section::frame("---\n---\n\n# Ten Forward\n\n".into()),
            Section::item(
                "m1",
                "<div data-section-uuid=\"m1\">\n\n## Riker\n\nTea, Earl Grey\n\n</div>\n".into(),
                "Tea, Earl Grey".into(),
                Vec::new(),
            ),
            Section::unkeyed("Page 2: the replicator manual\n".into()),
        ];
        std::fs::write(&tea.md_path, join(&tea.sections)).unwrap();
        tea.rows[0].author = Some("Will Riker".into());
        tea.rows[0].uuid = "m1".into();
        tea.rows[0].created_at = Some("2026-03-03T14:05:00-08:00".into());
        commit(root, &[tea], &[]);
        std::fs::write(
            doc_md_path(&rr, "d-tea"),
            "the file is no longer read".repeat(10),
        )
        .unwrap();

        let docs = draw_batch(root, GROUP, vec!["d-tea".into()]).await.unwrap();
        let texts: Vec<&str> = docs[0].sections.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Will Riker — Tue 3 Mar 2026 14:05\n\nTea, Earl Grey\n\n",
                "Page 2: the replicator manual\n",
            ]
        );
        assert_eq!(update(root).await.put, 1);
    }

    /// The embed's speed is over the model's time alone, and a pass that
    /// embedded nothing has none rather than a division by zero.
    #[test]
    fn the_rate_is_chunks_and_kilobytes_a_second_of_the_models_time() {
        let done = Done {
            embedded: 280,
            prompt_bytes: 340_000,
            embed_time: Duration::from_secs(20),
            ..Done::default()
        };
        assert_eq!(done.rate(), Some((14.0, 17.0)));
        assert_eq!(Done::default().rate(), None);
    }

    /// A chunk is embedded only while the document still draws it; else
    /// the document waits for the next keyword pass.
    #[test]
    fn a_chunk_the_document_no_longer_draws_is_not_embedded() {
        let doc = IndexDoc {
            source_id: GROUP.into(),
            markdown_uuid: "d".into(),
            content_hash: "h".into(),
            title: "T".into(),
            head: None,
            sections: text::draw(
                &[Piece {
                    text: "hello".into(),
                    framed: true,
                    ..Piece::default()
                }],
                &text::Facts::default(),
            ),
        };
        let drawn = doc.prompted();
        let good: HashSet<&str> = [drawn[0].hash.as_str()].into();
        assert_eq!(
            prompts_for(&doc, &good),
            Some(vec![(drawn[0].hash.clone(), drawn[0].prompt.clone())])
        );
        let stale: HashSet<&str> = ["not-a-chunk-of-it"].into();
        assert_eq!(prompts_for(&doc, &stale), None);
    }
}
