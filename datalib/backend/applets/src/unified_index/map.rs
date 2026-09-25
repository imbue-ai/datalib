//! The embedding map, joined to the grid.
//!
//! `GET /embedding_map` — each point the `embedding_map` step placed, as
//! the document row it is, with the names the config gives its source.
//! `GET /embedding_map/matches?q=…` — the documents a filter matches,
//! in the grid's search-bar grammar, so a filter reads the same in both.
//! Two routes so a new filter costs a list of ids, not the whole map.

use std::collections::{HashMap, HashSet};

use axum::extract::{Query, State};
use axum::response::Json;
use datalib_unified_index::embedding_map::{self, SeedCounts};
use datalib_unified_index::qmd::mapping::norm_path;
use datalib_unified_index::query::parse_query;
use datalib_unified_index::repo::MapDocRow;
use datalib_unified_index::search::SearchRow;
use serde::{Deserialize, Serialize};

use super::{columns, run_qmd_search, Index};

/// How many qmd hits a free-text filter lights up. Retrieval is ranked,
/// so this is "the best matches", not "every document that mentions it".
const FREE_TEXT_HITS: usize = 500;

#[derive(Debug, Deserialize)]
pub struct MatchParams {
    pub q: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct MatchResponse {
    pub markdown_uuids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct MapResponse {
    /// False until the `embedding_map` step has written a map.
    pub present: bool,
    pub made_at: Option<String>,
    pub seed: Option<SeedCounts>,
    /// Documents qmd has not embedded, so not on the map.
    pub unembedded: usize,
    /// Points on the map with no document row in the grid: a document
    /// removed since the map was made, most likely.
    pub unplaced: usize,
    pub points: Vec<PointOut>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PointOut {
    pub markdown_uuid: String,
    pub x: f32,
    pub y: f32,
    pub title: String,
    /// The provider as the configured source's own mark names it (Gmail
    /// rather than Mail).
    pub provider: String,
    /// The source as the config names it.
    pub source: String,
    pub source_id: String,
    pub kind: String,
    pub created_at: Option<String>,
    pub account: String,
    pub channel: String,
}

pub async fn handler(State(s): State<Index>) -> Json<MapResponse> {
    let mut errors = Vec::new();
    let map = match embedding_map::read(&s.root) {
        Ok(Some(map)) => map,
        Ok(None) => return Json(MapResponse::default()),
        Err(e) => {
            return Json(MapResponse {
                errors: vec![format!("{e:#}")],
                ..Default::default()
            })
        }
    };
    let docs = match s.repo.document_rows().await {
        Ok(docs) => docs,
        Err(e) => {
            errors.push(format!("document rows: {e}"));
            Vec::new()
        }
    };
    let sources = columns::Sources::read(&s.root);
    let (points, unplaced) = join(&map.points, docs, &sources);
    Json(MapResponse {
        present: true,
        made_at: Some(map.made_at),
        seed: Some(map.seed),
        unembedded: map.unembedded,
        unplaced,
        points,
        errors,
    })
}

pub async fn matches_handler(
    State(s): State<Index>,
    Query(p): Query<MatchParams>,
) -> Json<MatchResponse> {
    let mut errors = Vec::new();
    let mut markdown_uuids: Vec<String> = matching(&s, &p.q.unwrap_or_default(), &mut errors)
        .await
        .into_iter()
        .collect();
    markdown_uuids.sort();
    Json(MatchResponse {
        markdown_uuids,
        errors,
    })
}

/// The documents `q` matches. Free text goes to qmd, as it does on the
/// grid, with its structured terms applied to the hits; structured
/// terms alone are a SQL filter over every row. A qmd failure matches
/// nothing and says why.
async fn matching(s: &Index, q: &str, errors: &mut Vec<String>) -> HashSet<String> {
    let parsed = parse_query(q);
    let found = if parsed.free_text.is_empty() {
        s.repo.matching_documents(&parsed).await
    } else {
        match run_qmd_search(&s.root, &s.repo, &s.qmd, &parsed, FREE_TEXT_HITS).await {
            Ok(rows) => Ok(markdowns(rows)),
            Err(e) => {
                errors.push(format!("free-text search failed: {e:#}"));
                Ok(HashSet::new())
            }
        }
    };
    found.unwrap_or_else(|e| {
        errors.push(format!("filter: {e}"));
        HashSet::new()
    })
}

fn markdowns(rows: Vec<SearchRow>) -> HashSet<String> {
    rows.into_iter().filter_map(|r| r.markdown_uuid).collect()
}

/// Each map point as its document row, by path; a point whose document
/// the grid does not have is counted, not drawn.
pub fn join(
    points: &[embedding_map::MapPoint],
    docs: Vec<MapDocRow>,
    sources: &columns::Sources,
) -> (Vec<PointOut>, usize) {
    let mut by_path: HashMap<String, MapDocRow> = docs
        .into_iter()
        .map(|d| (norm_path(&d.qmd_path), d))
        .collect();
    let mut unplaced = 0;
    let mut out = Vec::with_capacity(points.len());
    for p in points {
        let Some(doc) = by_path.remove(&norm_path(&p.path)) else {
            unplaced += 1;
            continue;
        };
        let mut row = SearchRow {
            provider: doc.provider.clone(),
            source: doc.source_label.clone(),
            source_id: doc.source_id.clone(),
            ..Default::default()
        };
        sources.resolve(&mut row);
        out.push(PointOut {
            markdown_uuid: doc.markdown_uuid,
            x: p.x,
            y: p.y,
            title: doc.title,
            // The configured type's label (Gmail, not Mail), as the Source
            // column's hover shows it.
            provider: row
                .source_ref
                .as_ref()
                .and_then(|i| i.detail.clone())
                .unwrap_or(doc.provider),
            source: row
                .source_ref
                .map(|i| i.label)
                .unwrap_or(doc.source_id.clone()),
            source_id: doc.source_id,
            kind: doc.kind,
            created_at: doc.created_at,
            account: doc.account,
            channel: doc.channel,
        });
    }
    (out, unplaced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_unified_index::embedding_map::MapPoint;

    fn doc(uuid: &str, path: &str) -> MapDocRow {
        MapDocRow {
            markdown_uuid: uuid.into(),
            qmd_path: path.into(),
            title: format!("title {uuid}"),
            provider: "slack".into(),
            source_label: "Slack".into(),
            source_id: path.split('/').next().unwrap().into(),
            kind: "Slack Thread".into(),
            created_at: None,
            account: String::new(),
            channel: String::new(),
        }
    }

    fn point(path: &str, x: f32) -> MapPoint {
        MapPoint {
            path: path.into(),
            x,
            y: -x,
        }
    }

    /// A point is its document by path, spelled the way the grid's
    /// hit mapping spells it; one with no document is counted, not
    /// drawn with a made-up identity.
    #[test]
    fn points_join_their_documents_by_path() {
        let points = [
            point("work/render_markdown/a.md", 1.0),
            point("work/render_markdown/Gone.md", 2.0),
            point("work/render_markdown/b_c.md", 3.0),
        ];
        let docs = vec![
            doc("A", "work/render_markdown/a.md"),
            doc("B", "work/render_markdown/b-c.md"),
            doc("Z", "work/render_markdown/not-on-the-map.md"),
        ];
        let (out, unplaced) = join(&points, docs, &columns::Sources::default());
        assert_eq!(unplaced, 1);
        let got: Vec<(&str, f32)> = out
            .iter()
            .map(|p| (p.markdown_uuid.as_str(), p.x))
            .collect();
        assert_eq!(got, vec![("A", 1.0), ("B", 3.0)]);
        assert_eq!(out[0].provider, "Slack");
        assert_eq!(out[0].source, "work");
    }
}
