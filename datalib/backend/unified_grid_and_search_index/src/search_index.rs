//! Words and Meaning read from the search index (`datalib_search_index`,
//! its tables in the grid index's file): hits for landing, the other
//! terms and fusion to work on. A hit names its document; its path is
//! left for the page that shows it to look up.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use datalib_runtime::layout;
use datalib_search_index::{Held, HeldDoc, KeptRows, MessageHit, Reader};

use crate::hits::index_state::IndexStamp;
use crate::hits::lex;
use crate::hits::mapping::Place;
use crate::hits::vectors::{dot_and_norm, Freshness, Kept};
use crate::hits::SearchHit;

/// A ranked document and its hit.
pub type DocumentHit = (String, SearchHit);

async fn open(root: &Path) -> Result<Option<Reader>> {
    Reader::open(
        &layout::grid_index_db(root),
        &layout::inference_cache_db(root),
    )
    .await
}

/// The documents `words` find, of `sources`, within the rows `kept` keeps,
/// best first, at most `limit`, each placed at its message: those saying
/// the words in the order typed, then those with a message holding every
/// word, then pages holding them between their pieces (a page's heading and
/// its rows are parts of one thing, where a conversation's messages are
/// each said on their own). BM25 alone ranks a place that says the sentence
/// below short ones that merely hold each word. `None` when no step has
/// written the index.
pub async fn word_hits(
    root: &Path,
    words: &str,
    sources: Option<&[String]>,
    kept: Option<&KeptRows<'_>>,
    limit: usize,
) -> Result<Option<Vec<DocumentHit>>> {
    let Some(all) = lex::fts5_query(words) else {
        return Ok(Some(Vec::new()));
    };
    let Some(reader) = open(root).await? else {
        return Ok(None);
    };
    let found = async {
        let mut hits: Vec<DocumentHit> = Vec::new();
        for query in lex::fts5_phrase_query(words).iter().chain([&all]) {
            let found = reader.words(&query.expr, sources, kept, limit).await?;
            append_below(&mut hits, found);
        }
        if let Some(terms) = lex::fts5_page_terms(words) {
            let found = reader.page_words(&terms, sources, kept, limit).await?;
            append_below(&mut hits, found);
        }
        hits.truncate(limit);
        anyhow::Ok(hits)
    }
    .await;
    reader.close().await;
    found.map(Some)
}

/// `found`, a later query's hits, after `hits`: each document once, and no
/// later hit scored above one before it, so sorting by score keeps the
/// order the queries ranked them in.
fn append_below(hits: &mut Vec<DocumentHit>, found: Vec<MessageHit>) {
    let seen: HashSet<String> = hits.iter().map(|(d, _)| d.clone()).collect();
    let floor = hits.last().map(|(_, h)| h.score);
    hits.extend(
        found
            .into_iter()
            .filter(|h| !seen.contains(&h.markdown_uuid))
            .map(|h| {
                let score = floor.map_or(h.score, |f| h.score.min(f));
                let place = Place::Item { uuid: h.uuid };
                (h.markdown_uuid, hit(score, place))
            }),
    );
}

/// The vectors as they are now, or `None` when `held` was read at the same
/// generation and cache.
async fn read_unless_held(
    reader: &Reader,
    root: &Path,
    held: Option<&(ReadAt, Arc<Held>)>,
) -> Result<Option<(ReadAt, Arc<Held>)>> {
    let now = (
        reader.generation().await?,
        IndexStamp::of(&layout::inference_cache_db(root)),
    );
    if held.is_some_and(|(at, _)| *at == now) {
        return Ok(None);
    }
    let vectors = reader.held(&datalib_search_index::model_key()).await?;
    Ok(Some((now, Arc::new(vectors))))
}

fn hit(score: f64, place: Place) -> SearchHit {
    SearchHit {
        score,
        snippet: String::new(),
        place: Some(place),
    }
}

/// Every embedded chunk, read once and kept across searches: read again
/// when the index's generation or the cache has moved, unless a search
/// asks for what is held while a sync writes them.
#[derive(Default)]
pub struct HeldVectors {
    held: tokio::sync::Mutex<Option<(ReadAt, Arc<Held>)>>,
}

/// What held vectors were read at: the index's generation, and the
/// cache's file. The grid index's own writes move neither.
type ReadAt = (i64, IndexStamp);

type Stamps = (IndexStamp, IndexStamp);

/// When the file the index is in and the cache last changed on disk: what
/// says a sync is writing them. The grid index's own writes count too.
pub fn stamps(root: &Path) -> Stamps {
    (
        IndexStamp::of(&layout::grid_index_db(root)),
        IndexStamp::of(&layout::inference_cache_db(root)),
    )
}

impl HeldVectors {
    /// How many chunks are held.
    pub async fn held(&self) -> usize {
        self.held
            .lock()
            .await
            .as_ref()
            .map_or(0, |(_, h)| h.chunks.len())
    }

    async fn current(&self, root: &Path, fresh: Freshness) -> Result<Option<Arc<Held>>> {
        let mut held = self.held.lock().await;
        if let Some((_, vectors)) = held.as_ref().filter(|_| fresh == Freshness::AsHeld) {
            return Ok(Some(vectors.clone()));
        }
        let Some(reader) = open(root).await? else {
            *held = None;
            return Ok(None);
        };
        let read = read_unless_held(&reader, root, held.as_ref()).await;
        reader.close().await;
        if let Some(read) = read? {
            *held = Some(read);
        }
        Ok(held.as_ref().map(|(_, vectors)| vectors.clone()))
    }
    /// The documents with at least one chunk embedded. `None` when no step
    /// has written the index.
    pub async fn documents(
        &self,
        root: &Path,
        fresh: Freshness,
    ) -> Result<Option<HashSet<String>>> {
        Ok(self
            .current(root, fresh)
            .await?
            .map(|held| held.docs.iter().map(|d| d.markdown_uuid.clone()).collect()))
    }

    /// The `limit` documents nearest `query` by cosine, each at its nearest
    /// chunk, of `sources` (all when `None`) and of what `among` keeps of
    /// each (all when `None`). `None` when no step has written the index.
    pub async fn nearest(
        &self,
        root: &Path,
        query: &[f32],
        sources: Option<&[String]>,
        among: Option<&HashMap<String, Kept>>,
        limit: usize,
        fresh: Freshness,
    ) -> Result<Option<Vec<DocumentHit>>> {
        let Some(held) = self.current(root, fresh).await? else {
            return Ok(None);
        };
        if held.chunks.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if query.len() != held.dim {
            bail!(
                "the query's embedding has {} dimensions and the index's {}",
                query.len(),
                held.dim
            );
        }
        let (_, norm2) = dot_and_norm(query, query);
        if norm2 == 0.0 || !norm2.is_finite() {
            bail!("the query's embedding is all zeros");
        }
        let norm = norm2.sqrt();
        let sources: Option<HashSet<&str>> =
            sources.map(|s| s.iter().map(String::as_str).collect());
        let mut ranked: Vec<(&HeldDoc, f32, usize)> = held
            .docs
            .iter()
            .filter(|d| {
                sources
                    .as_ref()
                    .is_none_or(|s| s.contains(d.source_id.as_str()))
            })
            .filter_map(|d| {
                let kept = match among {
                    None => None,
                    Some(a) => match a.get(&d.markdown_uuid)? {
                        Kept::Whole => None,
                        Kept::Sections(s) => Some(s),
                    },
                };
                let best = d
                    .chunks
                    .clone()
                    .filter(|&c| kept.is_none_or(|k| held.covered(d, c).any(|u| k.contains(u))))
                    .map(|c| (dot_and_norm(held.vector(c), query).0 / norm, c))
                    .max_by(|a, b| a.0.total_cmp(&b.0))?;
                Some((d, best.0, best.1))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| a.0.markdown_uuid.cmp(&b.0.markdown_uuid))
        });
        ranked.truncate(limit);
        Ok(Some(
            ranked
                .into_iter()
                .map(|(doc, score, c)| {
                    let place = Place::Item {
                        uuid: held.covered(doc, c).next().map(str::to_string),
                    };
                    (doc.markdown_uuid.clone(), hit(f64::from(score), place))
                })
                .collect(),
        ))
    }
}
