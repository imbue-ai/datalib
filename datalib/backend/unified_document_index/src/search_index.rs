//! Words and Meaning read from our own search index
//! (`datalib_search_index`) rather than qmd's: the same hits, each placed by
//! its bytes in the document, so landing, the other terms and fusion treat
//! them as they treat qmd's. A hit names its document; its path is left
//! for the page that shows it to look up.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use datalib_runtime::layout;
use datalib_search_index::{Held, Reader};

use crate::qmd::index_state::IndexStamp;
use crate::qmd::lex::Fts5Terms;
use crate::qmd::mapping::Place;
use crate::qmd::vectors::{dot_and_norm, Freshness, Kept};
use crate::qmd::QmdHit;

/// A ranked document and its hit.
pub type DocumentHit = (String, QmdHit);

async fn open(root: &Path) -> Result<Option<Reader>> {
    Reader::open(
        &layout::search_index_db(root),
        &layout::inference_cache_db(root),
    )
    .await
}

/// The documents holding every term of `terms`, of `sources` (all when
/// `None`) and `among` (all when `None`), best first, at most `limit`.
/// `None` when no step has written the index.
pub async fn word_hits(
    root: &Path,
    terms: &Fts5Terms,
    sources: Option<&[String]>,
    among: Option<&HashSet<String>>,
    limit: usize,
) -> Result<Option<Vec<DocumentHit>>> {
    let Some(reader) = open(root).await? else {
        return Ok(None);
    };
    let found = reader
        .words(&terms.required, &terms.excluded, sources, among, limit)
        .await;
    reader.close().await;
    Ok(Some(
        found?
            .into_iter()
            .map(|h| {
                let place = Place::Words {
                    first: terms.first.clone(),
                };
                (h.markdown_uuid, hit(h.score, place))
            })
            .collect(),
    ))
}

fn hit(score: f64, place: Place) -> QmdHit {
    QmdHit {
        path: String::new(),
        score,
        snippet: String::new(),
        lines: None,
        docid: String::new(),
        title: String::new(),
        place: Some(place),
    }
}

/// Every embedded chunk, read once and kept across searches: read again
/// when the index or the cache has changed, unless a search asks for what
/// is held while a sync writes them.
#[derive(Default)]
pub struct HeldVectors {
    held: tokio::sync::Mutex<Option<(Stamps, Arc<Held>)>>,
}

type Stamps = (IndexStamp, IndexStamp);

/// When the index and the cache last changed on disk.
pub fn stamps(root: &Path) -> Stamps {
    (
        IndexStamp::of(&layout::search_index_db(root)),
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
        let now = stamps(root);
        if let Some((at, vectors)) = held.as_ref() {
            if *at == now || fresh == Freshness::AsHeld {
                return Ok(Some(vectors.clone()));
            }
        }
        let Some(reader) = open(root).await? else {
            *held = None;
            return Ok(None);
        };
        let read = reader.held(&datalib_search_index::model_key()).await;
        reader.close().await;
        let vectors = Arc::new(read?);
        *held = Some((now, vectors.clone()));
        Ok(Some(vectors))
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
        let mut ranked: Vec<(&str, f32, usize)> = held
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
                Some((d.markdown_uuid.as_str(), best.0, best.1))
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked.truncate(limit);
        Ok(Some(
            ranked
                .into_iter()
                .map(|(doc, score, c)| {
                    let chunk = &held.chunks[c];
                    let place = Place::Bytes {
                        start: chunk.start,
                        end: chunk.end,
                    };
                    (doc.to_string(), hit(f64::from(score), place))
                })
                .collect(),
        ))
    }
}
