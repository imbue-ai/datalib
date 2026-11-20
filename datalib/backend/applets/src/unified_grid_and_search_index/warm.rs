//! Loads what a search by meaning needs before anyone searches: the embedding
//! model, every stored vector, and where the messages start in the documents
//! a person's own filter reaches. On a large library that is minutes the
//! first search would otherwise wait. After that it keeps them current as a
//! sync writes the index, so a search seldom finds the files moved since
//! they were read; reading again reads only what changed (`HeldVectors` in
//! `unified_grid_and_search_index::search_index`).

use std::time::Duration;

use datalib_unified_grid_and_search_index::hits::index_state::IndexStamp;

use super::models::ModelGate;
use super::{meaning_ranking, parse_query, with_people, Index, SearchFailure};

/// Set (to anything but empty) to skip the warming, so the model is loaded
/// by the first search instead. For a test harness that starts many
/// backends at once and searches none of them by meaning.
const NO_WARM_ENV: &str = "DATALIB_SEARCH_NO_WARM";

pub(super) fn turned_off() -> bool {
    std::env::var_os(NO_WARM_ENV).is_some_and(|v| !v.is_empty())
}

/// How often the index is looked at.
const TICK: Duration = Duration::from_secs(10);

/// The first reads every vector. The second reads the documents `from:me`
/// reaches, nearly every conversation, to learn where their messages start;
/// the first search with a person's filter would otherwise pay for it.
const WARMING_SEARCHES: [&str; 2] = ["warm up", "from:me warm up"];

pub(super) async fn keep_warm(s: Index, models: ModelGate) {
    if turned_off() {
        tracing::info!("not warming the search by meaning: {NO_WARM_ENV} is set");
        return;
    }
    // Waiting here, however long a first download takes, means a failure
    // below is a real one rather than the model still being on its way.
    if tokio::task::spawn_blocking(move || models.wait_while_provisioning())
        .await
        .is_err()
    {
        return;
    }
    let mut warmed = None;
    let mut ticks = tokio::time::interval(TICK);
    loop {
        ticks.tick().await;
        let now = watched(&s);
        if warmed.as_ref() != Some(&now) {
            warm_once(&s).await;
            warmed = Some(now);
        }
    }
}

/// How the files a search by meaning reads look on disk.
fn watched(s: &Index) -> Vec<IndexStamp> {
    let (index, cache) = datalib_unified_grid_and_search_index::search_index::stamps(&s.root);
    vec![index, cache]
}

/// A search that fails here costs only the wait this was to save, so it is
/// logged and not tried again until the index moves; the next real search
/// says what is wrong.
pub(super) async fn warm_once(s: &Index) {
    let started = std::time::Instant::now();
    for q in WARMING_SEARCHES {
        let parsed = parse_query(q);
        let Ok((parsed, _)) = with_people(&s.root, q, parsed).await else {
            continue;
        };
        match meaning_ranking(s, &parsed).await {
            Ok(_) | Err(SearchFailure::NoIndex) => {}
            Err(SearchFailure::Ranking(e) | SearchFailure::Index(e)) => {
                tracing::warn!(query = q, error = %e, "could not warm the search by meaning");
                return;
            }
        }
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "warmed the search by meaning"
    );
}
