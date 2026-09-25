//! The `embedding_map` function: qmd's document embeddings laid out on a
//! plane by UMAP, written to `unified_index/embedding_map`. Each run
//! starts from the map the last one wrote, so documents already on it
//! stay where they were; a reset deletes the map and the next run lays
//! one out afresh. The layout is `datalib_embedding_map`; the file is
//! `datalib_unified_index::embedding_map`.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_embedding_map::{Corpus, Progress};
use datalib_unified_index::embedding_map::{self as map_file, EmbeddingMap, MapPoint, SeedCounts};

use crate::events::{Emitter, OutputClaim};

pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
        map_file::DIR
    )
}

pub async fn run(data_root: &Path, now: &str, emitter: &Emitter) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("reading the embeddings");
    let vectors = datalib_unified_index::qmd::vectors::read_document_vectors(data_root)
        .await?
        .unwrap_or_default();
    // A map that will not parse is a map this build cannot start from.
    // Saying so and starting afresh loses the old positions, which is the
    // one thing the file is for, so the step fails and a reset is the
    // way on.
    let prior = map_file::read(data_root)
        .context("read the previous map; reset this step to lay one out afresh")?
        .map(|m| m.positions())
        .unwrap_or_default();
    tracing::info!(
        documents = vectors.paths.len(),
        unembedded = vectors.unembedded,
        dim = vectors.dim,
        previous = prior.len(),
        "laying out the embedding map"
    );
    let corpus = Corpus {
        keys: vectors.paths,
        dim: vectors.dim,
        vectors: vectors.vectors,
    };
    let bar = progress.clone();
    let (corpus, layout) = tokio::task::spawn_blocking(move || {
        // The layout reports absolute epochs; the bar takes deltas.
        let reported = std::cell::Cell::new(0usize);
        let layout = datalib_embedding_map::layout(&corpus, &prior, &|p| match p {
            Progress::Neighbours => bar.set_message("finding each document's neighbours"),
            Progress::Manifold => bar.set_message("building the neighbourhood graph"),
            Progress::Epoch { done, total } => {
                if reported.get() == 0 {
                    bar.set_length(Some(total as u64));
                }
                bar.inc(done.saturating_sub(reported.replace(done)) as u64);
                bar.set_message(&format!("laying out: epoch {done} of {total}"));
            }
        });
        (corpus, layout)
    })
    .await
    .context("the layout panicked")?;
    let layout = layout.map_err(anyhow::Error::msg)?;

    let map = EmbeddingMap {
        made_at: now.to_string(),
        dim: corpus.dim,
        epochs: layout.epochs,
        seed: SeedCounts {
            kept: layout.seed.kept,
            near_neighbours: layout.seed.near_neighbours,
            fresh: layout.seed.fresh,
        },
        unembedded: vectors.unembedded,
        points: corpus
            .keys
            .into_iter()
            .zip(layout.points)
            .map(|(path, [x, y])| MapPoint { path, x, y })
            .collect(),
    };
    let bytes = map_file::write(data_root, &map)?;
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));

    for (name, n) in [
        ("documents", map.points.len()),
        ("unembedded", map.unembedded),
        ("kept", map.seed.kept),
        ("near_neighbours", map.seed.near_neighbours),
        ("fresh", map.seed.fresh),
    ] {
        progress.metric(name, &[], n as i64);
    }
    tracing::info!(
        documents = map.points.len(),
        kept = map.seed.kept,
        near_neighbours = map.seed.near_neighbours,
        fresh = map.seed.fresh,
        epochs = map.epochs,
        "the embedding map is written"
    );
    progress.finish(&format!("{} documents mapped", map.points.len()));
    Ok(vec![OutputClaim {
        path: out_rel(),
        version: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
        rows: None,
    }])
}

pub fn reset(data_root: &Path) -> Result<()> {
    map_file::remove(data_root)
}
