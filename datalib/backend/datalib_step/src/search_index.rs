//! The search index beside qmd's (docs/dev/plans/search_index_from_ram.md):
//! what each source's `keyword_index` and `embed`, and the aggregator, do
//! with it. Keywords always; embedding only where the config turns it on
//! (`embed_search_index`), since search does not read these vectors yet.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_embedder::{Device, Embedder};
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_runtime::layout;
use datalib_schema::markdowns::MarkdownRow;
use datalib_search_index::{prompt, ChunkSpan, Index, IndexDoc, IndexSection, InferenceCache};

/// Documents read and indexed together, one transaction each.
const DOCUMENT_BATCH: usize = 200;
/// Chunks embedded between two writes to the cache, and two checks for a stop.
const CHUNK_BATCH: usize = 32;

pub(crate) fn embedding_on() -> bool {
    std::env::var(datalib_dag::subprocess::ENV_EMBED_SEARCH_INDEX).as_deref() == Ok("1")
}

/// The pinned model's name in the inference cache: its file and the sha256
/// it was verified against, so another build of the model misses.
pub(crate) fn model_key() -> String {
    let model = &datalib_runtime::qmd::EMBED_MODELS[0];
    format!("{}@{}", model.file, model.sha256)
}

/// What a keyword pass or an embed did, for its log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Done {
    pub(crate) put: usize,
    pub(crate) removed: usize,
    pub(crate) embedded: usize,
    /// Documents whose file and render store disagree, so they are left for
    /// the next pass: the render is mid-write, or the file was touched.
    pub(crate) unsettled: usize,
}

/// A document the search covers: one with a file, outside `unindexed/`.
fn searchable(row: &MarkdownRow) -> bool {
    row.md_path.as_deref().is_some_and(|p| {
        !Path::new(p)
            .components()
            .any(|c| c.as_os_str() == layout::UNINDEXED_DIR)
    })
}

/// A rendered document as the index takes it, from its `markdowns` row and
/// its file's text. `None` when the two disagree: the text is not what the
/// row's hash names, or its sections do not spell the text.
pub(crate) fn index_doc(source_id: &str, row: &MarkdownRow, text: &str) -> Option<IndexDoc> {
    let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
    if row.content_hash.as_ref().is_some_and(|h| *h != hash) {
        return None;
    }
    let layout = row
        .section_layout
        .as_deref()
        .and_then(datalib_schema::markdowns::parse_layout);
    let sections = match layout {
        Some(spans) => {
            let mut at = 0;
            let mut out = Vec::with_capacity(spans.len());
            for span in spans {
                let end = at + span.bytes;
                out.push(IndexSection {
                    uuid: span.uuid,
                    text: text.get(at..end)?.to_string(),
                });
                at = end;
            }
            if at != text.len() {
                return None;
            }
            out
        }
        None => vec![IndexSection {
            uuid: None,
            text: text.to_string(),
        }],
    };
    Some(IndexDoc {
        source_id: source_id.to_string(),
        markdown_uuid: row.markdown_uuid.clone(),
        content_hash: hash,
        title: row.title.clone().unwrap_or_default(),
        sections,
    })
}

/// This source's searchable `markdowns` rows at its render store's head,
/// by id. Empty when the store has nothing committed.
async fn rendered(root: &Path, group: &str) -> Result<BTreeMap<String, MarkdownRow>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, group);
    tokio::task::spawn_blocking(move || {
        let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
            return Ok(BTreeMap::new());
        };
        let rows = store.markdown_rows();
        store.close();
        Ok(rows?
            .into_iter()
            .filter(searchable)
            .map(|r| (r.markdown_uuid.clone(), r))
            .collect())
    })
    .await
    .context("render store read panicked")?
}

fn read_text(root: &Path, row: &MarkdownRow) -> Option<String> {
    std::fs::read_to_string(root.join(row.md_path.as_deref()?)).ok()
}

/// `<group>/keyword_index`'s half: this source's documents in the search
/// index, brought in line with its render store.
pub(crate) async fn update_keywords(
    root: &Path,
    group: &str,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let rows = rendered(root, group).await?;
    let index = Index::open(&layout::search_index_db(root)).await?;
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
    // A row from a renderer that declares no sections has no hash; its
    // file's own hash stands in, which is the one render would have stored.
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for (uuid, row) in rows {
        let hash = match &row.content_hash {
            Some(h) => h.clone(),
            None => match read_text(root, row) {
                Some(text) => blake3::hash(text.as_bytes()).to_hex().to_string(),
                None => continue,
            },
        };
        current.insert(uuid.clone(), hash);
    }
    let owed = datalib_search_index::owed(&current, &indexed);
    progress.set_message("search index: keywords");
    progress.set_length(Some(owed.put.len() as u64));
    for batch in owed.put.chunks(DOCUMENT_BATCH) {
        if stop.requested() {
            break;
        }
        let mut docs = Vec::with_capacity(batch.len());
        for uuid in batch {
            let row = &rows[uuid];
            match read_text(root, row).and_then(|t| index_doc(group, row, &t)) {
                Some(doc) => docs.push(doc),
                None => done.unsettled += 1,
            }
        }
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
    let index = Index::open(&layout::search_index_db(root)).await?;
    let spans = index.chunk_spans(group).await;
    index.close().await;
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
    spans: Vec<ChunkSpan>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let model = model_key();
    let hashes: Vec<String> = spans.iter().map(|s| s.hash.clone()).collect();
    let missing: HashSet<String> = cache.missing(&model, &hashes).await?.into_iter().collect();
    if missing.is_empty() {
        return Ok(done);
    }
    let rows = rendered(root, group).await?;
    let mut by_doc: BTreeMap<&str, Vec<&ChunkSpan>> = BTreeMap::new();
    let mut queued = HashSet::new();
    for span in &spans {
        if missing.contains(&span.hash) && queued.insert(span.hash.as_str()) {
            by_doc.entry(&span.markdown_uuid).or_default().push(span);
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
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut result = Ok(());
    for (uuid, chunks) in by_doc {
        let prompts = match rows
            .get(uuid)
            .and_then(|row| Some((row, read_text(root, row)?)))
        {
            Some((row, text)) => prompts_for(row, &text, &chunks),
            None => None,
        };
        let Some(prompts) = prompts else {
            done.unsettled += 1;
            continue;
        };
        pending.extend(prompts);
        while pending.len() >= CHUNK_BATCH || (!pending.is_empty() && stop.requested()) {
            let batch: Vec<_> = pending.drain(..pending.len().min(CHUNK_BATCH)).collect();
            match embed_batch(&mut embedder, batch).await {
                Ok(vectors) => {
                    cache.put(&model, &vectors).await?;
                    done.embedded += vectors.len();
                    progress.inc(vectors.len() as u64);
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
            break;
        }
    }
    if result.is_ok() && !pending.is_empty() && !stop.requested() {
        let vectors = embed_batch(&mut embedder, std::mem::take(&mut pending)).await?;
        cache.put(&model, &vectors).await?;
        done.embedded += vectors.len();
        progress.inc(vectors.len() as u64);
    }
    // On Metal, ggml aborts at exit while a model is still loaded.
    drop(embedder.take());
    result.map(|()| done)
}

/// `(hash, prompt)` for each chunk of one document, or `None` when the file
/// no longer holds the text the index hashed: the document changed after
/// the keyword pass, and the next one re-chunks it.
fn prompts_for(
    row: &MarkdownRow,
    text: &str,
    chunks: &[&ChunkSpan],
) -> Option<Vec<(String, String)>> {
    let title = row.title.as_deref().unwrap_or_default();
    chunks
        .iter()
        .map(|c| {
            let piece = text.get(c.start..c.end)?;
            (datalib_search_index::chunk::chunk_hash(title, piece) == c.hash)
                .then(|| (c.hash.clone(), prompt::document(piece, Some(title))))
        })
        .collect()
}

async fn embed_batch(
    embedder: &mut Option<Embedder>,
    batch: Vec<(String, String)>,
) -> Result<Vec<(String, Vec<f32>)>> {
    let mut e = embedder.take().context("the embedder is gone")?;
    let (e, out) = tokio::task::spawn_blocking(move || {
        let out: Result<Vec<_>> = batch
            .into_iter()
            .map(|(hash, prompt)| Ok((hash, e.embed(&prompt)?)))
            .collect();
        (e, out)
    })
    .await
    .context("embedding panicked")?;
    *embedder = Some(e);
    out
}

/// The aggregator's half: drop every source it no longer reads, then let
/// the cache keep only what the index still names. Touches nothing that
/// does not exist yet.
pub(crate) async fn sweep(root: &Path, groups: &[String]) -> Result<(usize, u64)> {
    let index_path = layout::search_index_db(root);
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
    let gone = cache.keep_only(&model_key(), &hashes).await;
    cache.close().await;
    Ok((removed, gone?))
}

async fn sweep_index(index: &Index, groups: &[String]) -> Result<(usize, HashSet<String>)> {
    let removed = index.keep_sources(groups).await?;
    Ok((removed, index.all_hashes().await?))
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
            edges: Vec::new(),
            contacts: Vec::new(),
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
        let index = Index::open(&layout::search_index_db(root)).await.unwrap();
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
        assert_eq!(held["d-tea"], blake3::hash(&file).to_hex().to_string());
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

    /// A file that no longer holds what the store says it does is left for
    /// the next pass, and counted, rather than indexed half right.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_disagrees_with_its_store_is_left_and_counted() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let tea = rendered_doc(
            root,
            "d-tea",
            thread("Tea"),
            doc_md_path(&render_root(root), "d-tea"),
        );
        commit(root, std::slice::from_ref(&tea), &[]);
        std::fs::write(&tea.md_path, "rewritten behind the store's back").unwrap();
        assert_eq!(
            update(root).await,
            Done {
                unsettled: 1,
                ..Done::default()
            }
        );
        assert!(indexed(root).await.is_empty());
    }

    fn row(title: &str, layout: Option<&str>, hash: Option<String>) -> MarkdownRow {
        MarkdownRow {
            markdown_uuid: "d".into(),
            source_id: GROUP.into(),
            provider: "test".into(),
            kind: "Test".into(),
            title: Some(title.into()),
            created_at: None,
            modified_at: None,
            item_count: Some(1),
            md_path: Some("x.md".into()),
            upstream_cursor: None,
            renderer_version: None,
            bucket_key: None,
            section_layout: layout.map(str::to_string),
            content_hash: hash,
        }
    }

    #[test]
    fn a_document_is_cut_into_its_sections_by_its_layout() {
        let text = "head\nbody one\n";
        let layout = r#"[{"uuid":null,"bytes":5},{"uuid":"m1","bytes":9}]"#;
        let doc = index_doc(GROUP, &row("T", Some(layout), None), text).unwrap();
        let got: Vec<(Option<&str>, &str)> = doc
            .sections
            .iter()
            .map(|s| (s.uuid.as_deref(), s.text.as_str()))
            .collect();
        assert_eq!(got, [(None, "head\n"), (Some("m1"), "body one\n")]);
        assert_eq!(doc.title, "T");
        assert_eq!(
            doc.content_hash,
            blake3::hash(text.as_bytes()).to_hex().to_string()
        );
    }

    #[test]
    fn a_layout_or_hash_that_does_not_fit_the_text_is_refused() {
        let short = r#"[{"uuid":null,"bytes":3}]"#;
        assert!(index_doc(GROUP, &row("T", Some(short), None), "four").is_none());
        let wrong = Some("0".repeat(64));
        assert!(index_doc(GROUP, &row("T", None, wrong), "four").is_none());
        let doc = index_doc(GROUP, &row("T", None, None), "four").unwrap();
        assert_eq!(doc.sections.len(), 1);
    }

    /// A chunk is embedded only when the file still holds the text the
    /// index hashed; else the document waits for the next keyword pass.
    #[test]
    fn a_chunk_whose_text_moved_is_not_embedded() {
        let text = "hello world";
        let span = |hash: String| ChunkSpan {
            markdown_uuid: "d".into(),
            start: 0,
            end: 5,
            hash,
        };
        let r = row("T", None, None);
        let good = span(datalib_search_index::chunk::chunk_hash("T", "hello"));
        assert_eq!(
            prompts_for(&r, text, &[&good]),
            Some(vec![(
                good.hash.clone(),
                prompt::document("hello", Some("T"))
            )])
        );
        let stale = span(datalib_search_index::chunk::chunk_hash("T", "howdy"));
        assert_eq!(prompts_for(&r, text, &[&stale]), None);
    }
}
