//! Loads what a search by meaning needs before anyone searches: the embedding
//! model, every stored vector, and where the messages start in the documents
//! a person's own filter reaches. On a large library that is minutes the
//! first search would otherwise wait. After that it keeps them current as a
//! sync writes the index, so a search seldom finds the files moved since
//! they were read; reading again reads only what changed (`HeldVectors` in
//! `unified_grid_and_search_index::search_index`).

use std::path::Path;
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
    let mut warmed_at = None;
    let mut last: Option<Warming> = None;
    let mut ticks = tokio::time::interval(TICK);
    loop {
        ticks.tick().await;
        let now = watched(&s);
        if warmed_at.as_ref() != Some(&now) {
            let warming = warm_once(&s).await;
            say(&warming, last.as_ref());
            last = Some(warming);
            warmed_at = Some(now);
        }
    }
}

/// How the files a search by meaning reads look on disk.
fn watched(s: &Index) -> Vec<IndexStamp> {
    let (index, cache) = datalib_unified_grid_and_search_index::search_index::stamps(&s.root);
    vec![index, cache]
}

/// How a warming ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Warming {
    Done {
        elapsed_ms: u64,
    },
    /// The index is in another shape, as it is until the index step that an
    /// upgrade runs has built it again; why, in the refusal's words.
    Refused(String),
    Failed {
        query: &'static str,
        error: String,
    },
}

/// The index moves every few seconds while a step writes it, and each move
/// is tried: a failure is worth a line when it is new, not each time it is
/// met again.
fn worth_saying(now: &Warming, last: Option<&Warming>) -> bool {
    matches!(now, Warming::Done { .. }) || Some(now) != last
}

fn say(now: &Warming, last: Option<&Warming>) {
    if !worth_saying(now, last) {
        return;
    }
    match now {
        Warming::Done { elapsed_ms } => {
            tracing::info!(elapsed_ms, "warmed the search by meaning")
        }
        Warming::Refused(why) => tracing::info!(
            why = %why,
            "warming the search by meaning waits for the index to be brought up to date"
        ),
        Warming::Failed { query, error } => {
            tracing::warn!(query, error = %error, "could not warm the search by meaning")
        }
    }
}

/// A search that fails here costs only the wait this was to save; it is not
/// tried again until the index moves, and the next real search says what is
/// wrong.
pub(super) async fn warm_once(s: &Index) -> Warming {
    let started = std::time::Instant::now();
    if let Some(why) = refused(&s.root).await {
        return Warming::Refused(why);
    }
    for query in WARMING_SEARCHES {
        let parsed = parse_query(query);
        let Ok((parsed, _)) = with_people(&s.root, query, parsed).await else {
            continue;
        };
        match meaning_ranking(s, &parsed).await {
            Ok(_) | Err(SearchFailure::NoIndex) => {}
            Err(SearchFailure::Ranking(error) | SearchFailure::Index(error)) => {
                return Warming::Failed { query, error };
            }
        }
    }
    Warming::Done {
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

/// Why the index is refused for its shape; `None` when it is not, or
/// cannot be opened at all, which the searches then say.
async fn refused(root: &Path) -> Option<String> {
    match datalib_unified_grid_and_search_index::search_index::open(root).await {
        Ok(Some(reader)) => {
            reader.close().await;
            None
        }
        Ok(None) => None,
        Err(e) => datalib_store_meta::other_shape(&e).map(ToString::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards an upgrade's rebuild, which wrote the index every few seconds
    /// for twelve minutes and had the same refusal logged as a warning
    /// every ten.
    #[test]
    fn a_failure_met_again_is_said_once() {
        let refused = || Warming::Refused("another shape".into());
        let failed = |error: &str| Warming::Failed {
            query: "warm up",
            error: error.into(),
        };
        assert!(worth_saying(&refused(), None));
        assert!(!worth_saying(&refused(), Some(&refused())));
        assert!(worth_saying(&failed("locked"), Some(&refused())));
        assert!(!worth_saying(&failed("locked"), Some(&failed("locked"))));
        assert!(worth_saying(&failed("torn"), Some(&failed("locked"))));
        assert!(worth_saying(
            &refused(),
            Some(&Warming::Done { elapsed_ms: 1 })
        ));
    }
}
