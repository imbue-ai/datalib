//! Loads what a search by meaning needs before anyone searches: the embedding
//! model, every stored vector, and where the messages start in the documents
//! a person's own filter reaches. On a large library that is a quarter of a
//! minute the first search would otherwise wait, and the vectors go on being
//! held in memory, about a gigabyte on that library, whether or not anyone
//! searches. While a sync is writing the index a search answers from what
//! is held (`INDEX_SETTLE`); once the index has sat still, this reads what
//! the sync added, and a removed vector drops them all (`VectorCache`).

use std::time::Duration;

use datalib_unified_document_index::qmd::index_state::IndexStamp;
use datalib_unified_document_index::qmd::qmd_index_path;

use super::models::ModelGate;
use super::{meaning_ranking, parse_query, with_people, Index, SearchFailure, SearchReads};

/// How often the index is looked at, and how long it must then sit still:
/// an embed step commits as it goes, and each load costs tens of seconds of
/// disk and CPU that a running sync would rather have.
const TICK: Duration = Duration::from_secs(60);

/// The first reads every vector. The second reads the documents `from:me`
/// reaches, nearly every conversation, to learn where their messages start;
/// the first search with a person's filter would otherwise pay for it.
const WARMING_SEARCHES: [&str; 2] = ["warm up", "from:me warm up"];

/// Whether to load, given what the index looks like at each tick: at launch,
/// and after that when it differs from what was last loaded and has not moved
/// since the tick before.
struct Watch<S> {
    last_seen: Option<S>,
    loaded: Option<S>,
}

impl<S: PartialEq + Clone> Watch<S> {
    fn new() -> Self {
        Self {
            last_seen: None,
            loaded: None,
        }
    }

    fn should_load(&mut self, now: &S) -> bool {
        let launching = self.last_seen.is_none();
        let settled = self.last_seen.as_ref() == Some(now);
        self.last_seen = Some(now.clone());
        if self.loaded.as_ref() == Some(now) || !(launching || settled) {
            return false;
        }
        self.loaded = Some(now.clone());
        true
    }
}

pub(super) async fn keep_warm(s: Index, models: ModelGate) {
    // Waiting here, however long a first download takes, means a failure
    // below is a real one rather than the model still being on its way.
    if tokio::task::spawn_blocking(move || models.wait_while_provisioning())
        .await
        .is_err()
    {
        return;
    }
    let mut watch = Watch::new();
    let mut ticks = tokio::time::interval(TICK);
    loop {
        ticks.tick().await;
        if watch.should_load(&watched(&s)) {
            warm_once(&s).await;
        }
    }
}

/// How the files a search by meaning reads look on disk.
fn watched(s: &Index) -> Vec<IndexStamp> {
    match s.reads {
        SearchReads::Qmd => vec![IndexStamp::of(&qmd_index_path(&s.root))],
        SearchReads::SearchIndex => {
            let (index, cache) = datalib_unified_document_index::search_index::stamps(&s.root);
            vec![index, cache]
        }
    }
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
            Err(SearchFailure::Qmd(e) | SearchFailure::Index(e)) => {
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

#[cfg(test)]
mod tests {
    use super::Watch;

    #[test]
    fn it_loads_at_launch_and_not_again_until_the_index_moves() {
        let mut w = Watch::new();
        assert!(w.should_load(&1));
        assert!(!w.should_load(&1));
        assert!(!w.should_load(&1));
    }

    /// A sync commits as it goes: loading at each commit would throw the last
    /// load away, so a load waits for one tick with nothing new.
    #[test]
    fn it_waits_for_a_moving_index_to_settle() {
        let mut w = Watch::new();
        assert!(w.should_load(&1));
        assert!(!w.should_load(&2), "moved since the last tick");
        assert!(!w.should_load(&3), "still moving");
        assert!(w.should_load(&3), "sat still for a tick");
        assert!(!w.should_load(&3), "already loaded");
    }

    #[test]
    fn it_does_not_load_again_when_the_index_returns_to_what_is_loaded() {
        let mut w = Watch::new();
        assert!(w.should_load(&1));
        assert!(!w.should_load(&2));
        assert!(!w.should_load(&1));
        assert!(!w.should_load(&1));
    }
}
