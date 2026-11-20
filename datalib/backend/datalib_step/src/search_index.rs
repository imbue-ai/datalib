//! The search index (docs/dev/plans/search_index_from_ram.md): what the
//! `unified_grid_and_search_index` step's keyword pass and sweep, and
//! each source's `embed`, do with it. The keyword pass needs no model.
//! An `embed` cuts each document the keyword index holds into chunks and
//! files them beside the vectors, then embeds the chunks with none; it
//! opens the tokenizer only when a document needs cutting, and the model
//! only when a chunk needs a vector.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use datalib_embedder::{Device, Embedder};
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl_render::indexed_markdown::{self, index_key, IndexedMarkdownStore};
use datalib_runtime::layout;
use datalib_schema::markdowns::MarkdownRow;
#[cfg(test)]
use datalib_search_index::text::{self, Piece, INDEX_TEXT_VERSION};
use datalib_search_index::{ChunkedDoc, FiledChunk, Index, IndexDoc, InferenceCache, Reader};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::modal::{self, Reply, Route, SignIn};

/// Documents read and indexed together, one transaction each.
const DOCUMENT_BATCH: usize = 200;
/// Chunks embedded between two writes to the cache, and two checks for a stop.
const CHUNK_BATCH: usize = 32;
/// Vectors from Modal written to the cache together.
const MODAL_PUT_BATCH: usize = 256;

/// The model's tokenizer as the chunker counts with it: where each of a
/// text's tokens ends ([`datalib_search_index::chunk::TokenEnds`]).
pub(crate) type Cutter = Box<dyn Fn(&str) -> Result<Vec<usize>> + Send + Sync>;

/// What a keyword pass or an embed did, for its log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Done {
    pub(crate) put: usize,
    pub(crate) removed: usize,
    /// Documents an embed cut into chunks, new or changed since it last did.
    pub(crate) cut: usize,
    /// Chunks the cache had no vector for when the embed began.
    pub(crate) owed: usize,
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

/// A document the search covers: every one but those filed for the grid
/// alone (`markdowns.searchable`).
pub(crate) fn searchable(row: &MarkdownRow) -> bool {
    row.searchable
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

/// One source's documents in the search index, brought in line with its
/// render store: the `unified_grid_and_search_index` step's keyword pass, once per searched source.
/// The store is read at one commit for the whole pass, and a source whose
/// store and text version are where its last finished pass left them is
/// not read at all. A source not rendered yet has no store, and nothing
/// of it is indexed.
pub(crate) async fn update_keywords(
    root: &Path,
    group: &str,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    retire_separate_file(root)?;
    let render_root = datalib_etl::layout::render_markdown_root(root, group);
    // A source in the config whose render has not run yet has no file;
    // it is read as a store with nothing in it, like the grid pass does.
    let rendered = indexed_markdown::path_for(&render_root).is_file();
    let store = match rendered {
        false => None,
        true => match tokio::task::spawn_blocking(move || {
            IndexedMarkdownStore::open_for_reading(&render_root, None)
        })
        .await
        .context("render store open panicked")?
        {
            Ok(store) => store.map(Arc::new),
            // The grid pass files it as the source's problem, saying what
            // to run; the search keeps the source as it had it until then.
            Err(e) if datalib_store_meta::other_shape(&e).is_some() => {
                tracing::debug!(group, error = %format!("{e:#}"), "left as the search had it");
                return Ok(Done::default());
            }
            Err(e) => return Err(e),
        },
    };
    let index = Index::open(&layout::grid_index_db(root)).await;
    let out = match index {
        Ok(index) => {
            let out = keyword_pass(&index, group, store.clone(), progress, stop).await;
            index.close().await;
            out
        }
        Err(e) => Err(e),
    };
    if let Some(store) = store.and_then(Arc::into_inner) {
        tokio::task::spawn_blocking(move || store.close())
            .await
            .context("render store close panicked")?;
    }
    out
}

/// Where a pass over `store` reads: its commit, and the text version
/// every document is drawn with. `None` with no store to read.
fn read_at(store: Option<&IndexedMarkdownStore>) -> Option<String> {
    let pin = store?.pin()?;
    Some(format!(
        "{}{}",
        pin.commit(),
        datalib_search_index::text::key_suffix()
    ))
}

async fn keyword_pass(
    index: &Index,
    group: &str,
    store: Option<Arc<IndexedMarkdownStore>>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let reading = read_at(store.as_deref());
    if reading.is_some() && index.read_at(group).await? == reading {
        return Ok(done);
    }
    let rows = match &store {
        None => BTreeMap::new(),
        Some(store) => {
            let store = Arc::clone(store);
            let rows = tokio::task::spawn_blocking(move || store.markdown_rows())
                .await
                .context("render store read panicked")??;
            rows.into_iter()
                .filter(searchable)
                .map(|r| (r.markdown_uuid.clone(), r))
                .collect()
        }
    };
    // Forgotten first, so a pass cut short is never taken for a finished one.
    index.set_read_at(group, None).await?;
    let indexed = index.indexed(group).await?;
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for (uuid, row) in &rows {
        match index_key(row) {
            Some(key) => {
                current.insert(uuid.clone(), key);
            }
            None => done.unsettled += 1,
        }
    }
    let owed = datalib_search_index::owed(&current, &indexed);
    progress.set_message(&format!("search index: keywords for {group}"));
    progress.set_length(Some(owed.put.len() as u64));
    let mut cut_short = false;
    for batch in owed.put.chunks(DOCUMENT_BATCH) {
        if stop.requested() {
            cut_short = true;
            break;
        }
        let Some(store) = &store else { break };
        let store = Arc::clone(store);
        let (source, uuids) = (group.to_string(), batch.to_vec());
        let docs = tokio::task::spawn_blocking(move || store.index_docs(&source, &uuids))
            .await
            .context("render store read panicked")??;
        done.unsettled += batch.len() - docs.len();
        index.put(&docs).await?;
        done.put += docs.len();
        progress.inc(batch.len() as u64);
    }
    index.remove(&owed.remove).await?;
    done.removed = owed.remove.len();
    if !cut_short && done.unsettled == 0 {
        index.set_read_at(group, reading.as_deref()).await?;
    }
    Ok(done)
}

/// What the embed pass asks of a model: each prompt's vector, cut to fit
/// its context, and whether it had to be.
pub(crate) trait EmbedPrompt: Send + 'static {
    fn embed_fitted_many(&mut self, prompts: &[String]) -> Result<Vec<(Vec<f32>, bool)>>;
}

impl EmbedPrompt for Embedder {
    fn embed_fitted_many(&mut self, prompts: &[String]) -> Result<Vec<(Vec<f32>, bool)>> {
        Embedder::embed_fitted_many(self, prompts)
    }
}

/// Where a run that owes enough chunks sends them
/// (`embed_on_modal_from_n_chunks`), and how it gets there.
pub(crate) struct Modal {
    pub(crate) threshold: u64,
    pub(crate) sign_in: Box<dyn FnOnce() -> SignIn + Send>,
    pub(crate) driver: Box<dyn FnOnce() -> Result<modal::Driver> + Send>,
}

impl Modal {
    /// This process's sign-in and the driver in its runtime.
    pub(crate) fn for_this_process(threshold: u64) -> Modal {
        Modal {
            threshold,
            sign_in: Box::new(SignIn::of_this_process),
            driver: Box::new(modal::Driver::from_runtime),
        }
    }
}

/// `<group>/embed`'s half: every document of this source the keyword
/// index holds cut into chunks and filed, then every chunk the inference
/// cache has no vector for embedded and cached, here or on Modal. The
/// model's file is put in place, and its tokenizer and weights opened,
/// only when a document needs cutting or a chunk a vector.
pub(crate) async fn embed(
    root: &Path,
    group: &str,
    model: crate::index_steps::ModelFile,
    on_modal: Option<Modal>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let for_cutting = model.clone();
    let cutter = move || -> Result<Cutter> {
        let tokenizer = datalib_embedder::Tokenizer::open(&for_cutting.path()?)?;
        Ok(Box::new(move |text: &str| tokenizer.ends(text)))
    };
    let open = move || {
        let embedder = Embedder::open(&model.path()?, Device::Best, 0)?;
        let batching = embedder.batching();
        tracing::info!(
            device = %embedder.device(),
            cuda = datalib_embedder::cuda_module_report().unwrap_or("not looked for"),
            prompts_per_call = batching.prompts,
            tokens_per_call = batching.tokens,
            "loaded the search model"
        );
        Ok(embedder)
    };
    embed_with(root, group, cutter, open, on_modal, progress, stop).await
}

/// [`embed`] with the tokenizer `cutter` opens and the model `open`
/// loads, each called only when it is needed.
async fn embed_with<E: EmbedPrompt>(
    root: &Path,
    group: &str,
    cutter: impl FnOnce() -> Result<Cutter> + Send + 'static,
    open: impl FnOnce() -> Result<E> + Send + 'static,
    on_modal: Option<Modal>,
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
    let indexed = reader.documents_of(group).await;
    let finished = reader.pass_finished(group).await;
    reader.close().await;
    let (indexed, finished) = (indexed?, finished?);
    let cache = InferenceCache::open(&layout::inference_cache_db(root)).await?;
    let out = async {
        let cut = cut_owed(
            &cache, root, group, &indexed, finished, cutter, progress, stop,
        )
        .await?;
        if stop.requested() {
            return Ok(cut);
        }
        let embedded = embed_into(&cache, root, group, open, on_modal, progress, stop).await?;
        Ok(Done {
            cut: cut.cut,
            removed: cut.removed,
            unsettled: cut.unsettled + embedded.unsettled,
            ..embedded
        })
    }
    .await;
    cache.close().await;
    out
}

/// Cuts into chunks, and files, every document of `group` the keyword
/// index holds (`indexed`, `markdown_uuid → content_hash`) that was not
/// cut from the rendering it holds, and forgets the ones it no longer
/// holds once its keyword pass has `finished`: a pass cut short, over an
/// index emptied for a new shape, has not put back what it will, and
/// forgetting those would cost their vectors at the next sweep. `cutter`
/// is called only when a document is owed.
#[allow(clippy::too_many_arguments)]
async fn cut_owed(
    cache: &InferenceCache,
    root: &Path,
    group: &str,
    indexed: &BTreeMap<String, String>,
    finished: bool,
    cutter: impl FnOnce() -> Result<Cutter> + Send + 'static,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let owed = datalib_search_index::owed(indexed, &cache.chunked(group).await?);
    if finished {
        cache.forget_chunked(&owed.remove).await?;
        done.removed = owed.remove.len();
    } else if !owed.remove.is_empty() {
        tracing::info!(
            group,
            documents = owed.remove.len(),
            "the keyword pass has not finished; their chunks are kept until it does"
        );
    }
    if owed.put.is_empty() {
        return Ok(done);
    }
    progress.set_message("search index: cutting chunks");
    progress.set_length(Some(owed.put.len() as u64));
    let cutter: Arc<Cutter> = Arc::new(
        tokio::task::spawn_blocking(cutter)
            .await
            .context("tokenizer open panicked")??,
    );
    for batch in owed.put.chunks(DOCUMENT_BATCH) {
        if stop.requested() {
            break;
        }
        let drawn = draw_batch(root, group, batch.to_vec()).await?;
        // A document drawn from another rendering than the index holds is
        // cut once the keyword pass has taken that rendering.
        let settled: Vec<IndexDoc> = drawn
            .into_iter()
            .filter(|d| indexed.get(&d.markdown_uuid) == Some(&d.content_hash))
            .collect();
        done.unsettled += batch.len() - settled.len();
        let cut = cut_batch(Arc::clone(&cutter), settled).await?;
        cache.file_chunks(&cut).await?;
        done.cut += cut.len();
        progress.inc(batch.len() as u64);
    }
    Ok(done)
}

/// `docs` cut into chunks on every core, off the runtime, in the order
/// given.
async fn cut_batch(cutter: Arc<Cutter>, docs: Vec<IndexDoc>) -> Result<Vec<ChunkedDoc>> {
    tokio::task::spawn_blocking(move || {
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(docs.len().max(1));
        let next = AtomicUsize::new(0);
        let mut out: Vec<(usize, ChunkedDoc)> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(doc) = docs.get(i) else {
                                return Ok::<_, anyhow::Error>(mine);
                            };
                            let prompted = doc
                                .prompted(&**cutter)
                                .with_context(|| format!("cut document {}", doc.markdown_uuid))?;
                            mine.push((i, ChunkedDoc::from_prompted(doc, &prompted)));
                        }
                    })
                })
                .collect();
            let mut all = Vec::with_capacity(docs.len());
            for worker in workers {
                all.extend(worker.join().map_err(|_| anyhow!("cutting panicked"))??);
            }
            Ok::<_, anyhow::Error>(all)
        })?;
        out.sort_by_key(|(i, _)| *i);
        Ok(out.into_iter().map(|(_, doc)| doc).collect())
    })
    .await
    .context("cutting panicked")?
}

/// Each owed hash once, under the first document that names it: `markdown_uuid
/// → (the rendering it was cut from, its chunks to embed)`.
type OwedByDoc = BTreeMap<String, (String, Vec<FiledChunk>)>;

/// Every chunk of `group` filed in the cache with no vector, embedded and
/// cached. `open` is called only when one is owed here.
async fn embed_into<E: EmbedPrompt>(
    cache: &InferenceCache,
    root: &Path,
    group: &str,
    open: impl FnOnce() -> Result<E> + Send + 'static,
    on_modal: Option<Modal>,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let mut done = Done::default();
    let model = datalib_search_index::model_key();
    let mut by_doc: OwedByDoc = BTreeMap::new();
    let mut queued = HashSet::new();
    for (doc, cut_from, chunk) in cache.owed_chunks(&model, group).await? {
        if queued.insert(chunk.hash.clone()) {
            by_doc
                .entry(doc)
                .or_insert_with(|| (cut_from, Vec::new()))
                .1
                .push(chunk);
        }
    }
    if queued.is_empty() {
        return Ok(done);
    }
    done.owed = queued.len();
    progress.set_length(Some(queued.len() as u64));
    if let Some(on_modal) = on_modal {
        if modal::route(Some(on_modal.threshold), queued.len()) == Route::Modal {
            progress.set_message("search index: embedding on Modal");
            return embed_on_modal(
                cache,
                root,
                group,
                &by_doc,
                queued.len(),
                on_modal,
                progress,
                stop,
            )
            .await;
        }
    }
    progress.set_message("search index: embedding");

    let mut embedder = Some(
        tokio::task::spawn_blocking(open)
            .await
            .context("embedder open panicked")??,
    );
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut result = Ok(());
    let owing: Vec<String> = by_doc.keys().cloned().collect();
    'docs: for uuids in owing.chunks(DOCUMENT_BATCH) {
        for doc in draw_batch(root, group, uuids.to_vec()).await? {
            let Some(prompts) = prompts_for(&doc, &by_doc[doc.markdown_uuid.as_str()])? else {
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
    result.map(|()| done)
}

/// What the Modal driver said, besides vectors.
#[derive(Default)]
struct FromModal {
    embedded: usize,
    usage: modal::Usage,
    truncated: usize,
    failed: usize,
    first_error: Option<String>,
    fatal: Option<(String, bool)>,
}

/// Every owed chunk sent to Modal through the driver (`crate::modal`):
/// prompts in on its stdin as the documents are read, vectors out of its
/// stdout into the cache as each slice returns. A stop interrupts the
/// driver, which ends its Sandboxes; what was cached stays, and the next
/// run owes the rest. Of the remote work, only its counts are kept.
#[allow(clippy::too_many_arguments)]
async fn embed_on_modal(
    cache: &InferenceCache,
    root: &Path,
    group: &str,
    by_doc: &OwedByDoc,
    owed: usize,
    on_modal: Modal,
    progress: &Progress,
    stop: &StopFlag,
) -> Result<Done> {
    let threshold = on_modal.threshold;
    if !(on_modal.sign_in)().signed_in() {
        bail!(modal::not_signed_in(
            group,
            owed,
            threshold,
            "this machine is not signed in to Modal"
        ));
    }
    let mut driver = (on_modal.driver)()?;
    driver.options.slice = modal::slice_for(owed, driver.options.sandboxes, driver.options.slice);
    tracing::info!(
        owed,
        threshold,
        image = %driver.options.image,
        gpu = %driver.options.gpu,
        sandboxes = driver.options.sandboxes,
        slice = driver.options.slice,
        "embedding on Modal: {owed} chunks owed, at least `{}` of {threshold}",
        modal::PARAM
    );
    let mut child = driver
        .command()?
        .spawn()
        .with_context(|| format!("start the Modal driver with {}", driver.node.display()))?;
    let stdin = child.stdin.take().context("the driver's stdin")?;
    let stdout = child.stdout.take().context("the driver's stdout")?;
    let stderr = child.stderr.take().context("the driver's stderr")?;
    let started = Instant::now();
    let model = datalib_search_index::model_key();
    let options = driver.options.clone();

    let feed = async move {
        let mut stdin = stdin;
        let mut unsettled = 0;
        let mut prompt_bytes = 0;
        let owing: Vec<String> = by_doc.keys().cloned().collect();
        'docs: for uuids in owing.chunks(DOCUMENT_BATCH) {
            for doc in draw_batch(root, group, uuids.to_vec()).await? {
                if stop.requested() {
                    break 'docs;
                }
                let Some(prompts) = prompts_for(&doc, &by_doc[doc.markdown_uuid.as_str()])? else {
                    unsettled += 1;
                    continue;
                };
                for (hash, prompt) in prompts {
                    prompt_bytes += prompt.len();
                    stdin
                        .write_all(modal::request_line(&hash, &prompt).as_bytes())
                        .await
                        .context("write to the Modal driver")?;
                }
            }
        }
        stdin.shutdown().await.ok();
        Ok::<_, anyhow::Error>((unsettled, prompt_bytes))
    };
    let collect = async {
        let mut from = FromModal::default();
        let mut lines = BufReader::new(stdout).lines();
        let mut batch: Vec<(String, Vec<f32>)> = Vec::new();
        while let Some(line) = lines.next_line().await.context("read the Modal driver")? {
            match serde_json::from_str::<Reply>(&line)
                .with_context(|| format!("the Modal driver said {:.200}", line))?
            {
                Reply::Vector { hash, vector, cut } => {
                    from.truncated += usize::from(cut);
                    batch.push((hash, vector));
                }
                Reply::Failed { failed, error } => {
                    from.failed += failed.len();
                    from.first_error.get_or_insert(error);
                }
                Reply::Fatal { fatal, sign_in } => from.fatal = Some((fatal, sign_in)),
                Reply::Sandbox { sandbox_ms, ok } => {
                    from.usage.add(sandbox_ms, ok);
                    modal::report(progress, &from.usage, &options);
                }
            }
            if batch.len() >= MODAL_PUT_BATCH {
                cache.put(&model, &batch).await?;
                from.embedded += batch.len();
                progress.inc(batch.len() as u64);
                progress.metric("modal_chunks_total", &[], from.embedded as i64);
                batch.clear();
            }
        }
        cache.put(&model, &batch).await?;
        from.embedded += batch.len();
        progress.inc(batch.len() as u64);
        progress.metric("modal_chunks_total", &[], from.embedded as i64);
        Ok::<_, anyhow::Error>(from)
    };
    let said = async {
        let mut last = Vec::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(group, "{line}");
            last.push(line);
            if last.len() > 5 {
                last.remove(0);
            }
        }
        last
    };
    let watch = async {
        while !stop.requested() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        modal::interrupt(&child);
        std::future::pending::<()>().await
    };
    let (fed, collected, last) = tokio::select! {
        out = async { tokio::join!(feed, collect, said) } => out,
        () = watch => unreachable!("the watch never ends"),
    };
    let status = child.wait().await.context("wait for the Modal driver")?;
    let from = collected?;
    if let Some((fatal, sign_in)) = from.fatal {
        if sign_in {
            bail!(modal::not_signed_in(
                group,
                owed,
                threshold,
                &format!("Modal refused the sign-in ({fatal})")
            ));
        }
        bail!("the Modal driver could not start: {fatal}");
    }
    let done = Done {
        owed,
        embedded: from.embedded,
        truncated: from.truncated,
        embed_time: started.elapsed(),
        ..Done::default()
    };
    // The row's closing words on a run that ended well or was stopped. A
    // failed run says the same in its error, which the row shows instead.
    let spent = modal::summary(from.embedded, &from.usage, &options);
    tracing::info!(group, "{spent}");
    if stop.requested() {
        progress.set_message(&spent);
        return Ok(done);
    }
    let (unsettled, prompt_bytes) = fed?;
    if !status.success() {
        bail!(
            "the Modal driver exited {status}: {}. So far it {spent}",
            last.join(" | ")
        );
    }
    if from.failed > 0 {
        bail!(
            "{} of {owed} chunks could not be embedded on Modal ({}); the {} that were are \
             cached, and the next run sends the rest again. This run {spent}",
            from.failed,
            from.first_error.unwrap_or_default(),
            from.embedded
        );
    }
    progress.set_message(&spent);
    Ok(Done {
        unsettled,
        prompt_bytes,
        ..done
    })
}

/// `(hash, prompt)` for each of `owed`'s chunks of `doc`, made from the
/// spans filed for it with no tokenizer, or `None` when the document is
/// no longer the rendering they were cut from: the next embed cuts it
/// again.
fn prompts_for(
    doc: &IndexDoc,
    (cut_from, owed): &(String, Vec<FiledChunk>),
) -> Result<Option<Vec<(String, String)>>> {
    if doc.content_hash != *cut_from {
        return Ok(None);
    }
    let spans: Vec<_> = owed.iter().map(|f| f.chunk).collect();
    let prompted = doc.prompts(&spans)?;
    if prompted.iter().zip(owed).any(|(p, f)| p.hash != f.hash) {
        return Ok(None);
    }
    Ok(Some(
        prompted.into_iter().map(|p| (p.hash, p.prompt)).collect(),
    ))
}

/// Each prompt's vector, and how many had to be cut to fit the context.
async fn embed_batch<E: EmbedPrompt>(
    embedder: &mut Option<E>,
    batch: Vec<(String, String)>,
) -> Result<Embedded> {
    let mut e = embedder.take().context("the embedder is gone")?;
    let prompt_bytes = batch.iter().map(|(_, prompt)| prompt.len()).sum();
    let started = Instant::now();
    let (e, out) = tokio::task::spawn_blocking(move || {
        let (hashes, prompts): (Vec<String>, Vec<String>) = batch.into_iter().unzip();
        let out = e.embed_fitted_many(&prompts).map(|embedded| {
            let cut = embedded.iter().filter(|(_, truncated)| *truncated).count();
            let vectors = hashes.into_iter().zip(embedded.into_iter().map(|(v, _)| v));
            (vectors.collect(), cut)
        });
        (e, out)
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

/// The `unified_grid_and_search_index` step's sweep, after its keyword
/// passes: the cache forgets the chunks of every source it does not
/// search, then keeps only the vectors a filed chunk names. Answers how
/// many vectors went. Touches nothing that does not exist yet.
pub(crate) async fn sweep(root: &Path, searched: &[String]) -> Result<u64> {
    retire_separate_file(root)?;
    let cache_path = layout::inference_cache_db(root);
    if !cache_path.exists() {
        return Ok(0);
    }
    let cache = InferenceCache::open(&cache_path).await?;
    let gone = async {
        cache.keep_chunked_sources(searched).await?;
        cache.sweep(&datalib_search_index::model_key()).await
    }
    .await;
    cache.close().await;
    gone
}

/// Before the keyword passes: the chunks a build before this one filed in
/// the keyword index move into the cache, before the index's next open
/// empties them, so nothing is cut again and the sweep keeps the vectors
/// they name. Answers how many documents came over.
pub(crate) async fn carry_chunks(root: &Path) -> Result<usize> {
    datalib_search_index::carry_chunks_from_the_index(
        &layout::grid_index_db(root),
        &layout::inference_cache_db(root),
    )
    .await
}

/// The `unified_grid_and_search_index` step's half after its keyword passes: every document of a
/// source it does not search goes, whether the source left the config or
/// was put in `not_searched`. Returns how many went.
pub(crate) async fn keep_searched(root: &Path, searched: &[String]) -> Result<usize> {
    let index = Index::open(&layout::grid_index_db(root)).await?;
    let out = index.keep_sources(searched).await;
    index.close().await;
    out
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
    models_dir.join(datalib_runtime::models::EMBED_MODELS[0].cache_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::grid_index::RenderedMarkdown;
    use datalib_etl_render::section::{join, Section};
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;
    use datalib_search_index::chunk::four_bytes_a_token;

    const GROUP: &str = "ten-forward";

    /// A second source, beside `GROUP`.
    const OTHER: &str = "sickbay";

    fn render_root(root: &Path) -> PathBuf {
        render_root_of(root, GROUP)
    }

    fn render_root_of(root: &Path, group: &str) -> PathBuf {
        datalib_etl::layout::render_markdown_root(root, group)
    }

    /// A document as a renderer hands it over.
    fn rendered_doc(uuid: &str, sections: Vec<Section>) -> RenderedMarkdown {
        rendered_doc_of(GROUP, uuid, sections)
    }

    fn rendered_doc_of(group: &str, uuid: &str, sections: Vec<Section>) -> RenderedMarkdown {
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
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        RenderedMarkdown {
            searchable: true,
            content_hash: None,
            markdown_uuid: uuid.to_string(),
            source_id: group.to_string(),
            upstream_cursor: None,
            render_unit_key: None,
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
        commit_of(root, GROUP, docs, removed)
    }

    fn commit_of(root: &Path, group: &str, docs: &[RenderedMarkdown], removed: &[String]) {
        let store = IndexedMarkdownStore::open(&render_root_of(root, group)).unwrap();
        for d in docs {
            store.put_document(d).unwrap();
        }
        if !removed.is_empty() {
            store.remove_documents(removed).unwrap();
        }
        store.commit("test").unwrap();
        store.close();
    }

    async fn update(root: &Path) -> Done {
        update_of(root, GROUP).await
    }

    async fn update_of(root: &Path, group: &str) -> Done {
        update_keywords(root, group, &Progress::noop(), &StopFlag::new())
            .await
            .unwrap()
    }

    async fn indexed(root: &Path) -> BTreeMap<String, String> {
        indexed_of(root, GROUP).await
    }

    async fn indexed_of(root: &Path, group: &str) -> BTreeMap<String, String> {
        let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
        let out = index.indexed(group).await.unwrap();
        index.close().await;
        out
    }

    /// One document, `uuid`, saying `text`, committed under `group`.
    fn one_doc(root: &Path, group: &str, uuid: &str, text: &str) {
        let doc = rendered_doc_of(group, uuid, thread(text));
        commit_of(root, group, &[doc], &[]);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Ev {
        Length(Option<u64>),
        Inc(u64),
    }

    /// The bar's lengths and increments, in the order they were reported.
    #[derive(Default, Clone)]
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<Ev>>>);

    impl datalib_etl::progress::ProgressSink for Recorder {
        fn set_length(&self, total: Option<u64>) {
            self.0.lock().unwrap().push(Ev::Length(total));
        }
        fn inc(&self, delta: u64) {
            self.0.lock().unwrap().push(Ev::Inc(delta));
        }
    }

    impl Recorder {
        fn progress(&self) -> Progress {
            Progress::new(std::sync::Arc::new(self.clone()))
        }
        fn take(&self) -> Vec<Ev> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    /// A model that answers every prompt with the same vector, counting
    /// them.
    struct Fixed(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl EmbedPrompt for Fixed {
        fn embed_fitted_many(&mut self, prompts: &[String]) -> Result<Vec<(Vec<f32>, bool)>> {
            self.0
                .fetch_add(prompts.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(prompts.iter().map(|_| (vec![0.6, 0.8], false)).collect())
        }
    }

    /// How many of `group`'s documents Meaning does not find yet: not cut,
    /// or a chunk with no vector. What the index step reports.
    async fn unembedded(root: &Path, group: &str) -> usize {
        let held = datalib_search_index::holdings(root, &[group.to_string()])
            .await
            .unwrap();
        held[0].not_embedded
    }

    /// The chunker counting a token every four bytes.
    fn four_bytes() -> Result<Cutter> {
        Ok(Box::new(four_bytes_a_token))
    }

    /// A tokenizer that must not be opened: nothing needed cutting.
    fn never_cut() -> Result<Cutter> {
        bail!("the tokenizer was opened with nothing to cut")
    }

    /// A keyword pass is one source's: it indexes that source's documents,
    /// and removing one of them leaves every other source's as they were.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_keyword_pass_touches_only_its_own_source() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        one_doc(root, OTHER, "d-hypo", "a hypospray");

        assert_eq!(update(root).await.put, 1);
        assert!(indexed_of(root, OTHER).await.is_empty(), "not its source");
        assert_eq!(update_of(root, OTHER).await.put, 1);
        let other = indexed_of(root, OTHER).await;

        commit(root, &[], &["d-tea".to_string()]);
        assert_eq!(update(root).await.removed, 1);
        assert!(indexed(root).await.is_empty());
        assert_eq!(indexed_of(root, OTHER).await, other);
    }

    /// A source added to the config but not yet rendered has no render
    /// store on disk. Its keyword pass indexes nothing and fails nothing;
    /// it once failed the whole index step, which held every `embed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_with_no_render_store_yet_indexes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, OTHER, "d-hypo", "a hypospray");
        assert!(!datalib_etl_render::indexed_markdown::path_for(&render_root(root)).exists());

        assert_eq!(update(root).await, Done::default());
        assert!(indexed(root).await.is_empty());
        assert_eq!(update_of(root, OTHER).await.put, 1);
    }

    /// A stop asked for before a keyword pass begins puts nothing; the
    /// next pass, not stopped, does the work.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_keyword_pass_puts_nothing_and_the_next_one_catches_up() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        let stop = StopFlag::new();
        stop.request();
        let done = update_keywords(root, GROUP, &Progress::noop(), &stop)
            .await
            .unwrap();
        assert_eq!(done.put, 0);
        assert!(indexed(root).await.is_empty());
        assert_eq!(update(root).await.put, 1);
    }

    /// The Manage row's bar: its length is the documents owed, and it
    /// fills to that length. A pass over a store that has not moved since
    /// the last finished one does not read it, and draws no bar; the next
    /// commit is read again.
    #[tokio::test(flavor = "multi_thread")]
    async fn keyword_progress_counts_the_documents_owed() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let docs: Vec<RenderedMarkdown> = ["d-tea", "d-hush"]
            .into_iter()
            .map(|u| rendered_doc(u, thread(u)))
            .collect();
        commit(root, &docs, &[]);
        let bar = Recorder::default();
        let stop = StopFlag::new();
        update_keywords(root, GROUP, &bar.progress(), &stop)
            .await
            .unwrap();
        let events = bar.take();
        assert_eq!(events.first(), Some(&Ev::Length(Some(2))), "{events:?}");
        let filled: u64 = events
            .iter()
            .map(|e| match e {
                Ev::Inc(n) => *n,
                Ev::Length(_) => 0,
            })
            .sum();
        assert_eq!(filled, 2, "{events:?}");

        update_keywords(root, GROUP, &bar.progress(), &stop)
            .await
            .unwrap();
        assert_eq!(bar.take(), [], "the store has not moved");

        one_doc(root, GROUP, "d-quarters", "Captain's quarters");
        update_keywords(root, GROUP, &bar.progress(), &stop)
            .await
            .unwrap();
        assert_eq!(bar.take().first(), Some(&Ev::Length(Some(1))));
    }

    /// The `unified_grid_and_search_index` step searches every source it reads but those its
    /// `not_searched` lists: a listed source's documents leave the index,
    /// and come back once it is off the list, though its store has not
    /// moved since its last pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_grid_step_searches_every_source_it_reads_but_those_not_searched() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        one_doc(root, OTHER, "d-hypo", "a hypospray");
        let env = crate::source::StepEnv {
            step: "unified_grid_and_search_index/unified_grid_and_search_index".into(),
            group: "unified_grid_and_search_index".into(),
            group_type: None,
            diff_source_type: None,
            function: crate::function::Function::UnifiedGridAndSearchIndex,
            inputs: vec![
                format!("{GROUP}/render_markdown"),
                format!("{OTHER}/render_markdown"),
            ],
        };
        let run = |not_searched: Vec<String>| {
            let env = &env;
            async move {
                crate::unified_grid_and_search_index::run(
                    root,
                    env,
                    &crate::unified_grid_and_search_index::Params { not_searched },
                    Some("2026-01-01T00:00:00+00:00"),
                    &crate::events::Emitter::new("test".into()),
                    &StopFlag::new(),
                )
                .await
                .unwrap()
            }
        };
        run(Vec::new()).await;
        assert_eq!(indexed(root).await.len(), 1);
        assert_eq!(indexed_of(root, OTHER).await.len(), 1);

        run(vec![OTHER.to_string()]).await;
        assert_eq!(indexed(root).await.len(), 1);
        assert!(indexed_of(root, OTHER).await.is_empty());

        run(Vec::new()).await;
        assert_eq!(indexed_of(root, OTHER).await.len(), 1);
    }

    /// A keyword pass cut short, as after the index was emptied for a new
    /// shape, has not put every document back: the embed keeps the chunks
    /// of the ones it lacks, so the sweep keeps their vectors, and forgets
    /// them only once a pass has finished without them.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unfinished_keyword_pass_forgets_no_ones_chunks() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        one_doc(root, GROUP, "d-hush", "Shields up");
        update(root).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let embed = |cutter: fn() -> Result<Cutter>| {
            let local = calls.clone();
            async move {
                embed_with(
                    root,
                    GROUP,
                    cutter,
                    move || Ok(Fixed(local)),
                    None,
                    &Progress::noop(),
                    &StopFlag::new(),
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(embed(four_bytes).await.cut, 2);
        let cut = || async {
            let cache = InferenceCache::open(&layout::inference_cache_db(root))
                .await
                .unwrap();
            let out = cache.chunked(GROUP).await.unwrap();
            cache.close().await;
            out
        };
        let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
        index.set_read_at(GROUP, None).await.unwrap();
        index.remove(&["d-hush".to_string()]).await.unwrap();
        index.close().await;
        assert_eq!(embed(never_cut).await.removed, 0);
        assert_eq!(cut().await.len(), 2, "kept while the pass is unfinished");

        assert_eq!(update(root).await.put, 1, "the pass puts it back");
        let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
        index.remove(&["d-hush".to_string()]).await.unwrap();
        index.close().await;
        assert_eq!(embed(never_cut).await.removed, 1);
        assert_eq!(cut().await.keys().collect::<Vec<_>>(), ["d-tea"]);
    }

    /// The index step over these render inputs, as the runner runs it.
    async fn index_step(root: &Path, groups: &[&str]) -> Result<Vec<crate::events::OutputClaim>> {
        let env = crate::source::StepEnv {
            step: "unified_grid_and_search_index/unified_grid_and_search_index".into(),
            group: "unified_grid_and_search_index".into(),
            group_type: None,
            diff_source_type: None,
            function: crate::function::Function::UnifiedGridAndSearchIndex,
            inputs: groups
                .iter()
                .map(|g| format!("{g}/render_markdown"))
                .collect(),
        };
        crate::unified_grid_and_search_index::run(
            root,
            &env,
            &crate::unified_grid_and_search_index::Params::default(),
            Some("2026-01-01T00:00:00+00:00"),
            &crate::events::Emitter::new("test".into()),
            &StopFlag::new(),
        )
        .await
    }

    /// A source added to the config is an input of the index step before
    /// its first render has made a store. The step indexes the others and
    /// takes that one as holding nothing; it failed on opening the store
    /// that was not there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_not_rendered_yet_holds_nothing_and_fails_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        index_step(root, &[GROUP, "a-new-source"]).await.unwrap();
        assert_eq!(indexed(root).await.len(), 1);
        assert!(indexed_of(root, "a-new-source").await.is_empty());
    }

    /// A root a build before this one indexed, its chunks filed in the
    /// keyword index and their vectors cached: the index step moves the
    /// chunks beside the vectors before the index is emptied for its new
    /// shape, so the sweep keeps every vector and the documents are still
    /// embedded, with nothing cut or embedded again.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_builds_chunks_and_vectors_survive_the_upgrade() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        update(root).await;
        let doc = draw_batch(root, GROUP, vec!["d-tea".into()])
            .await
            .unwrap()
            .remove(0);
        let cut = doc.prompted(&four_bytes_a_token).unwrap();
        // What an older build left: its chunks in the keyword index's file,
        // which was in another shape, and their vectors in the cache.
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(layout::grid_index_db(root));
        let grid = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE search_chunks (doc_id INTEGER NOT NULL, seq INTEGER NOT NULL, \
             start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL, \
             first_section INTEGER NOT NULL, last_section INTEGER NOT NULL, \
             hash TEXT NOT NULL, PRIMARY KEY (doc_id, seq)) WITHOUT ROWID",
        )
        .execute(&grid)
        .await
        .unwrap();
        for (seq, p) in cut.iter().enumerate() {
            sqlx::query(
                "INSERT INTO search_chunks SELECT doc_id, ?, ?, ?, ?, ?, ? \
                   FROM search_documents WHERE markdown_uuid = 'd-tea'",
            )
            .bind(seq as i64)
            .bind(p.chunk.start as i64)
            .bind(p.chunk.end as i64)
            .bind(p.chunk.first_section as i64)
            .bind(p.chunk.last_section as i64)
            .bind(&p.hash)
            .execute(&grid)
            .await
            .unwrap();
        }
        sqlx::query(
            "UPDATE _datalib_meta SET value = 'an older shape' \
              WHERE key = 'search_index_schema_hash'",
        )
        .execute(&grid)
        .await
        .unwrap();
        grid.close().await;
        let cache = InferenceCache::open(&layout::inference_cache_db(root))
            .await
            .unwrap();
        let vectors: Vec<(String, Vec<f32>)> = cut
            .iter()
            .map(|p| (p.hash.clone(), vec![0.6, 0.8]))
            .collect();
        cache
            .put(&datalib_search_index::model_key(), &vectors)
            .await
            .unwrap();
        cache.close().await;

        index_step(root, &[GROUP]).await.unwrap();
        assert_eq!(cached(root).await.len(), cut.len(), "the sweep kept them");
        assert_eq!(unembedded(root, GROUP).await, 0);
        let done = embed_with(
            root,
            GROUP,
            never_cut,
            never_opened,
            None,
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(done, Done::default());
    }

    /// An embed is one source's: it cuts and embeds that source's
    /// documents and no other's, and with nothing owed it opens neither
    /// the tokenizer nor the model.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_embed_fills_only_its_own_source() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        one_doc(root, GROUP, "d-tea", "Tea, Earl Grey");
        one_doc(root, OTHER, "d-hypo", "a hypospray");
        update(root).await;
        update_of(root, OTHER).await;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let run = |group: &'static str, cutter: fn() -> Result<Cutter>| {
            let calls = calls.clone();
            async move {
                let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let flag = opened.clone();
                let open = move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(Fixed(calls))
                };
                let done = embed_with(
                    root,
                    group,
                    cutter,
                    open,
                    None,
                    &Progress::noop(),
                    &StopFlag::new(),
                )
                .await
                .unwrap();
                (done, opened.load(std::sync::atomic::Ordering::SeqCst))
            }
        };

        let (done, opened) = run(GROUP, four_bytes).await;
        assert!(opened);
        assert_eq!((done.cut, done.embedded), (1, 1));
        assert_eq!(unembedded(root, GROUP).await, 0);
        assert_eq!(unembedded(root, OTHER).await, 1, "not its source");

        let (done, _) = run(OTHER, four_bytes).await;
        assert_eq!(done.embedded, 1);
        assert_eq!(unembedded(root, OTHER).await, 0);

        let (done, opened) = run(GROUP, never_cut).await;
        assert_eq!(done, Done::default());
        assert!(!opened, "nothing owed, no model loaded");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// The keyword pass takes what the render store holds, by its content
    /// hash: a new document is put, a changed one put again, an unchanged
    /// one left, a removed one removed, and one kept for the grid alone
    /// never indexed.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_keyword_pass_follows_the_render_store() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let tea = rendered_doc("d-tea", thread("Tea, Earl Grey"));
        let hush = rendered_doc("d-hush", thread("Shields up"));
        let mut quiet = rendered_doc("d-quiet", thread("a repeat"));
        quiet.searchable = false;
        commit(root, &[tea.clone(), hush.clone(), quiet], &[]);

        assert_eq!(
            update(root).await,
            Done {
                put: 2,
                ..Done::default()
            }
        );
        let held = indexed(root).await;
        let text = join(&tea.sections);
        assert_eq!(
            held["d-tea"],
            format!(
                "{}.t{INDEX_TEXT_VERSION}",
                blake3::hash(text.as_bytes()).to_hex()
            )
        );
        assert!(!held.contains_key("d-quiet"));

        assert_eq!(update(root).await, Done::default(), "nothing changed");

        let tea2 = rendered_doc("d-tea", thread("Tea, hot"));
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
        let tea = rendered_doc("d-tea", thread("Tea"));
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
        let tea = rendered_doc("d-tea", thread("Tea, Earl Grey"));
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

    /// The index draws a document's text from what the render store keeps:
    /// an item is its envelope and its body, with none of the markup drawn
    /// around it, and a section kept whole is drawn as it is.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_draws_from_what_the_store_keeps() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let mut tea = rendered_doc("d-tea", Vec::new());
        tea.sections = vec![
            Section::frame("---\n---\n\n# Ten Forward\n\n".into()),
            Section::item(
                "m1",
                "msg".into(),
                datalib_etl_render::item::Item {
                    author: Some("Riker".into()),
                    ..datalib_etl_render::item::Item::new(
                        datalib_schema::sections::ItemForm::Message,
                        "Tea, Earl Grey".into(),
                    )
                },
            ),
            Section::unkeyed("Page 2: the replicator manual\n".into()),
        ];
        tea.rows[0].author = Some("Will Riker".into());
        tea.rows[0].uuid = "m1".into();
        tea.rows[0].created_at = Some("2026-03-03T14:05:00-08:00".into());
        commit(root, &[tea], &[]);

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

    /// A filed chunk is embedded only while the document is the rendering
    /// it was cut from and makes the prompt it was filed under; else the
    /// next embed cuts the document again.
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
        let drawn = doc.prompted(&four_bytes_a_token).unwrap();
        let filed = FiledChunk {
            chunk: drawn[0].chunk,
            hash: drawn[0].hash.clone(),
        };
        assert_eq!(
            prompts_for(&doc, &("h".into(), vec![filed.clone()])).unwrap(),
            Some(vec![(drawn[0].hash.clone(), drawn[0].prompt.clone())])
        );
        assert_eq!(
            prompts_for(&doc, &("another rendering".into(), vec![filed.clone()])).unwrap(),
            None
        );
        let misfiled = FiledChunk {
            hash: "not-a-chunk-of-it".into(),
            ..filed
        };
        assert_eq!(
            prompts_for(&doc, &("h".into(), vec![misfiled])).unwrap(),
            None
        );
    }

    /// Chunks filed and owed a vector are embedded with no tokenizer: the
    /// prompts are made from the spans filed. Here the first run cut the
    /// documents and its model failed, so the second owes only vectors.
    #[tokio::test(flavor = "multi_thread")]
    async fn filed_chunks_are_embedded_without_the_tokenizer() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let err = embed_with(
            root,
            GROUP,
            four_bytes,
            || -> Result<Fixed> { bail!("the model would not load") },
            None,
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("would not load"), "{err:#}");
        let calls = Arc::new(AtomicUsize::new(0));
        let local = calls.clone();
        let done = embed_with(
            root,
            GROUP,
            never_cut,
            move || Ok(Fixed(local)),
            None,
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!((done.cut, done.embedded), (0, 5));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(unembedded(root, GROUP).await, 0);
    }

    /// The driver over a runtime tree of the test's making: the pinned
    /// Node, and the stand-in for Modal's SDK where npm's `modal` would be,
    /// whose Sandboxes run `fake_embed_prompts.mjs` here.
    struct FakeModal {
        _dir: tempfile::TempDir,
        root: PathBuf,
        log: PathBuf,
        node: PathBuf,
        embed_prompts: PathBuf,
    }

    impl FakeModal {
        fn new() -> FakeModal {
            let r = runfiles::Runfiles::create().expect("runfiles");
            let file = |rel: &str| {
                let path = r.rlocation(rel).unwrap_or_else(|| panic!("{rel}"));
                assert!(path.is_file(), "{}", path.display());
                path
            };
            let node = file(&std::env::var("NODE_RLOC").expect("NODE_RLOC"));
            let sdk = file("_main/datalib/backend/datalib_step/src/modal/testdata/fake_sdk.mjs");
            let embed_prompts = file(
                "_main/datalib/backend/datalib_step/src/modal/testdata/fake_embed_prompts.mjs",
            );
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("runtime");
            let node_at = root.join(datalib_runtime::node_runtime::NODE_REL);
            std::fs::create_dir_all(node_at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&node, &node_at).unwrap();
            let entry = root
                .join("modal")
                .join(datalib_runtime::node_runtime::MODAL_SDK_VERSION)
                .join(datalib_runtime::node_runtime::MODAL_SDK_ENTRY_REL);
            std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
            std::fs::copy(&sdk, &entry).unwrap();
            let log = dir.path().join("sandboxes.jsonl");
            FakeModal {
                _dir: dir,
                root,
                log,
                node,
                embed_prompts,
            }
        }

        /// The step's own options, but for the command each Sandbox runs
        /// and slices small enough that a test makes several.
        fn modal(&self, threshold: u64, signed_in: bool, env: &[(&str, &str)]) -> Modal {
            self.modal_with(threshold, signed_in, env, 2)
        }

        fn modal_with(
            &self,
            threshold: u64,
            signed_in: bool,
            env: &[(&str, &str)],
            sandboxes: usize,
        ) -> Modal {
            let mut options = modal::Options::for_this_build();
            options.command = vec![
                self.node.display().to_string(),
                self.embed_prompts.display().to_string(),
            ];
            options.slice = 2;
            options.sandboxes = sandboxes;
            let root = self.root.clone();
            let mut env: Vec<(String, String)> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            env.push(("FAKE_MODAL_LOG".into(), self.log.display().to_string()));
            Modal {
                threshold,
                sign_in: Box::new(move || SignIn {
                    token_in_env: signed_in,
                    config_file: None,
                }),
                driver: Box::new(move || {
                    let mut driver = modal::Driver::in_tree(&root, options)?;
                    driver.env = env;
                    Ok(driver)
                }),
            }
        }

        /// One line per Sandbox the run made.
        fn sandboxes(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    /// What a run reported: each metric's newest value by name and labels,
    /// and its progress messages in order.
    #[derive(Default)]
    struct Reported {
        metrics: std::sync::Mutex<BTreeMap<String, i64>>,
        messages: std::sync::Mutex<Vec<String>>,
    }

    impl datalib_etl::progress::ProgressSink for Reported {
        fn set_message(&self, msg: &str) {
            self.messages.lock().unwrap().push(msg.to_string());
        }
        fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
            let labels: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let key = format!("{name}{{{}}}", labels.join(","));
            self.metrics.lock().unwrap().insert(key, value);
        }
    }

    impl Reported {
        fn metric(&self, key: &str) -> Option<i64> {
            self.metrics.lock().unwrap().get(key).copied()
        }
        fn last_message(&self) -> String {
            self.messages
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    /// Five documents of one chunk each, keyword-indexed and owing their
    /// vectors.
    async fn five_docs(root: &Path) {
        for i in 0..5 {
            one_doc(
                root,
                GROUP,
                &format!("d-{i}"),
                &format!("Log entry number {i}"),
            );
        }
        update(root).await;
        assert_eq!(unembedded(root, GROUP).await, 5);
    }

    /// A model that must not be loaded: the run went to Modal, or owed
    /// nothing.
    fn never_opened() -> Result<Fixed> {
        panic!("the local model was loaded for a run that owed it nothing")
    }

    /// Each cached vector, by hash.
    async fn cached(root: &Path) -> BTreeMap<String, Vec<f32>> {
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(layout::inference_cache_db(root))
            .read_only(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT hash, vector FROM vectors")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;
        rows.into_iter()
            .map(|(hash, blob)| {
                let floats = blob
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|w| f32::from_le_bytes(*w))
                    .collect();
                (hash, floats)
            })
            .collect()
    }

    /// Over the threshold, every owed chunk goes to Modal: one Sandbox per
    /// slice from this release's `-cuda` image on a T4, and each slice's
    /// vectors land in the cache under its prompt's hash, as a local embed
    /// would put them; the local model is never loaded. The next run owes
    /// nothing and makes no Sandbox.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_over_the_threshold_embeds_on_modal() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        let reported = Arc::new(Reported::default());

        let done = embed_with(
            root,
            GROUP,
            four_bytes,
            never_opened,
            Some(fake.modal(5, true, &[])),
            &Progress::new(reported.clone()),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.embedded, 5);
        assert!(done.prompt_bytes > 0);
        assert_eq!(unembedded(root, GROUP).await, 0);
        // fake_embed_prompts answers [prompt length, 1].
        for (hash, vector) in cached(root).await {
            assert_eq!(vector.len(), 2, "{hash}");
            assert_eq!(vector[1], 1.0, "{hash}");
        }
        let made = fake.sandboxes();
        assert_eq!(made.len(), 3, "5 prompts, 2 a slice: {made:?}");
        let sent: u64 = made.iter().map(|s| s["lines"].as_u64().unwrap()).sum();
        assert_eq!(sent, 5);
        for s in &made {
            assert_eq!(
                s["image"],
                format!(
                    "ghcr.io/imbue-ai/datalib:{}-cuda",
                    datalib_runtime::build_id::DATALIB_VERSION
                )
            );
            assert_eq!(s["gpu"], "T4");
            // No reservation: Modal's minimum, and whatever the run uses.
            assert!(s["cpu"].is_null() && s["memoryMiB"].is_null(), "{s}");
            assert_eq!(s["app"], "datalib-embed");
            // The Sandbox idles and the slice moves as files: its own
            // stdout is what lost answers on a GPU.
            assert_eq!(s["command"][0], "sleep", "{s}");
            let execs = s["execs"].as_array().unwrap();
            assert_eq!(execs.len(), 1, "{s}");
            let script = execs[0][2].as_str().unwrap();
            assert!(
                script.contains("< /tmp/datalib-embed/in.jsonl > /tmp/datalib-embed/out.jsonl"),
                "{s}"
            );
        }
        assert_eq!(reported.metric("modal_chunks_total{}"), Some(5));
        assert_eq!(reported.metric("modal_sandboxes_total{gpu=T4}"), Some(3));
        assert_eq!(
            reported.metric("modal_sandboxes_failed_total{gpu=T4}"),
            Some(0)
        );
        assert!(reported
            .metric("modal_sandbox_seconds_total{gpu=T4}")
            .is_some());
        assert!(reported
            .metric("modal_cost_estimate_micro_usd_total{gpu=T4}")
            .is_some_and(|c| c > 0));
        let said = reported.last_message();
        assert!(
            said.starts_with("embedded 5 chunks on Modal: 3 T4 Sandbox(es),")
                && said.contains("at list price"),
            "{said}"
        );

        let again = embed_with(
            root,
            GROUP,
            four_bytes,
            never_opened,
            Some(fake.modal(0, true, &[])),
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(again, Done::default());
        assert_eq!(fake.sandboxes().len(), 3, "nothing owed, nothing sent");
    }

    /// Under the threshold the run stays here and makes no Sandbox; with
    /// no threshold at all nothing ever leaves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_under_the_threshold_embeds_here() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let local = calls.clone();
        let done = embed_with(
            root,
            GROUP,
            four_bytes,
            move || Ok(Fixed(local)),
            Some(fake.modal(6, true, &[])),
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.embedded, 5);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);
        assert!(fake.sandboxes().is_empty());
    }

    /// Over the threshold and not signed in, the run fails before anything
    /// leaves, saying how to sign in or raise the threshold; it does not
    /// quietly embed here. A sign-in Modal itself refuses fails the same way.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_over_the_threshold_without_a_sign_in_fails_loudly() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        for (signed_in, env, says) in [
            (false, &[][..], "not signed in to Modal"),
            (
                true,
                &[("FAKE_MODAL_SIGNED_OUT", "1")][..],
                "Modal refused the sign-in (Profile is missing credentials",
            ),
        ] {
            let err = embed_with(
                root,
                GROUP,
                four_bytes,
                never_opened,
                Some(fake.modal(1, signed_in, env)),
                &Progress::noop(),
                &StopFlag::new(),
            )
            .await
            .unwrap_err();
            let err = format!("{err:#}");
            for want in [says, "modal setup", modal::PARAM, "owes 5 chunks"] {
                assert!(err.contains(want), "{want}: {err}");
            }
            assert_eq!(unembedded(root, GROUP).await, 5);
        }
        assert!(fake.sandboxes().is_empty());
    }

    /// A slice whose Sandbox fails is sent again; one that fails twice is
    /// reported, the step fails, and the vectors the other slices brought
    /// back are cached all the same, so the next run owes only the rest.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slice_that_fails_twice_is_owed_by_the_next_run() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        let reported = Arc::new(Reported::default());
        let done = embed_with(
            root,
            GROUP,
            four_bytes,
            never_opened,
            Some(fake.modal(0, true, &[("FAKE_MODAL_FAIL", "1")])),
            &Progress::new(reported.clone()),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.embedded, 5, "the first failure is sent again");
        assert_eq!(fake.sandboxes().len(), 4);
        // The failed attempt ran, so it is counted and priced.
        assert_eq!(reported.metric("modal_sandboxes_total{gpu=T4}"), Some(4));
        assert_eq!(
            reported.metric("modal_sandboxes_failed_total{gpu=T4}"),
            Some(1)
        );
        assert!(
            reported
                .last_message()
                .contains("4 T4 Sandbox(es), 1 of them failed"),
            "{}",
            reported.last_message()
        );

        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        let err = embed_with(
            root,
            GROUP,
            four_bytes,
            never_opened,
            // One at a time, so the first slice is the one that fails twice.
            Some(fake.modal_with(0, true, &[("FAKE_MODAL_FAIL", "2")], 1)),
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("2 of 5 chunks could not be embedded on Modal"),
            "{err}"
        );
        assert!(
            err.contains("This run embedded 3 chunks on Modal: 4 T4 Sandbox(es), 2 of them failed"),
            "the error says what the run spent: {err}"
        );
        assert_eq!(unembedded(root, GROUP).await, 2);
    }

    /// A Sandbox that hands back only part of a slice, with no error, as a
    /// T4's stdout did on Modal: the vectors that came are kept, and only
    /// the prompts still missing go out again. Every Sandbox here answers
    /// half its slice, so a slice of two is two Sandboxes, and all five
    /// chunks still arrive.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_answer_keeps_what_came_and_sends_only_the_rest() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        five_docs(root).await;
        let fake = FakeModal::new();
        let done = embed_with(
            root,
            GROUP,
            four_bytes,
            never_opened,
            Some(fake.modal_with(0, true, &[("FAKE_MODAL_SHORT", "99")], 1)),
            &Progress::noop(),
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.embedded, 5);
        assert_eq!(unembedded(root, GROUP).await, 0);
        let made = fake.sandboxes();
        let sent: Vec<u64> = made.iter().map(|s| s["lines"].as_u64().unwrap()).collect();
        assert_eq!(
            sent,
            [2, 1, 2, 1, 1],
            "each slice of two, then its one missing prompt"
        );
    }

    /// Many Sandboxes answering while the step is slow to read: each
    /// slice waiting on stdout once added a `drain` listener of its own,
    /// and past ten Node printed `MaxListenersExceededWarning` on every
    /// write, which the step logged as the driver's own lines.
    #[tokio::test(flavor = "multi_thread")]
    async fn many_slices_waiting_on_a_full_stdout_share_one_wait() {
        use tokio::io::AsyncReadExt;
        const SLICES: usize = 20;
        let fake = FakeModal::new();
        let modal = fake.modal_with(0, true, &[], SLICES);
        let mut driver = (modal.driver)().unwrap();
        driver.options.slice = 1;
        let mut child = driver.command().unwrap().spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        // A hash longer than a pipe holds, so every answer's write fills it.
        let long = "h".repeat(128 * 1024);
        for i in 0..SLICES {
            let line = modal::request_line(&format!("{i:02}{long}"), "tea");
            stdin.write_all(line.as_bytes()).await.unwrap();
        }
        drop(stdin);

        // Nothing reads stdout until every Sandbox has ended, so each
        // slice is waiting on it by then.
        let deadline = Instant::now() + Duration::from_secs(60);
        while fake.sandboxes().len() < SLICES {
            assert!(
                Instant::now() < deadline,
                "only {} of {SLICES} Sandboxes ended",
                fake.sandboxes().len()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        let mut err = String::new();
        stderr.read_to_string(&mut err).await.unwrap();
        assert!(child.wait().await.unwrap().success(), "{err}");

        let vectors = out.lines().filter(|l| l.contains("\"vector\"")).count();
        assert_eq!(vectors, SLICES, "every answer arrived");
        assert!(!err.contains("MaxListenersExceededWarning"), "{err}");
    }
}
