//! What a download owes, and the one loop that fetches it.
//!
//! Upstream *lists* records, each at a version: an update time, a
//! newest-reply stamp, a hash of its listing entry, a date a day is
//! final from, or no version at all for a record that only has to
//! exist. What we *hold* is recorded beside the record, as the version
//! its content satisfies (`held_version` in the table's `_bookkeeping`
//! sidecar, with the stamp of the fetch that landed it). What is *owed*
//! is the difference, asked of the store each time and never stored:
//! docs/dev/data_architecture_ingestion.md, "What is left to fetch".
//!
//! The listing can come from anywhere: a delta's answer, an
//! enumeration's pages, a calendar, the rows of another table, or the
//! diff between two commits of a store. [`drain`] fetches what is owed,
//! a batch per request and as many requests at once as the provider
//! allows, writes what came back a flush at a time in one transaction
//! each, and records what did not come. A provider is what it lists,
//! how it fetches a batch, and how it stores one ([`Fetcher`]); how a
//! stop, a failure, a skip or a give-up is handled is here, once.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use sqlx::{Sqlite, SqlitePool, Transaction};

use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::RunProblems;
use datalib_etl::stop::StopFlag;

/// A record upstream lists, at the version it lists it at. `None` is a
/// listing with no version: the record only has to have been fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub version: Option<String>,
}

impl Listed {
    pub fn new(key: impl Into<String>, version: Option<impl Into<String>>) -> Self {
        Self {
            key: key.into(),
            version: version.map(Into::into),
        }
    }
}

/// What `table`'s sidecar says of a record: whether a fetch of it ever
/// landed, and the version the content satisfies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub fetched: bool,
    pub version: Option<String>,
}

impl Held {
    /// Whether this satisfies a listing at `version`. A record a fetch
    /// never landed for is not held, whatever its sidecar row says: a
    /// failed attempt leaves one too.
    pub fn satisfies(&self, version: &Option<String>) -> bool {
        self.fetched && self.version == *version
    }
}

/// Of `listed`, the records `table` does not hold at that version.
/// Order is the listing's.
pub async fn owed(pool: &SqlitePool, table: &str, listed: Vec<Listed>) -> Result<Vec<Listed>> {
    let held = held_versions(pool, table, listed.iter().map(|l| l.key.as_str())).await?;
    Ok(listed
        .into_iter()
        .filter(|l| !held.get(&l.key).is_some_and(|h| h.satisfies(&l.version)))
        .collect())
}

/// `key → held` for the keys of `table` that have a sidecar row.
pub async fn held_versions<'k>(
    pool: &SqlitePool,
    table: &str,
    keys: impl IntoIterator<Item = &'k str>,
) -> Result<HashMap<String, Held>> {
    let keys: Vec<&str> = keys.into_iter().collect();
    let mut out = HashMap::with_capacity(keys.len());
    for chunk in keys.chunks(datalib_etl::bulk::SQL_CHUNK) {
        // Audited: `table` is a provider's `&'static str`; the
        // placeholders are one `?` per key, every key bound.
        let sql = format!(
            "SELECT id, fetched_at_utc IS NOT NULL, held_version \
             FROM {table}_bookkeeping WHERE id IN ({})",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query_as::<_, (String, bool, Option<String>)>(sqlx::AssertSqlSafe(sql));
        for key in chunk {
            q = q.bind(*key);
        }
        let rows = q
            .fetch_all(pool)
            .await
            .with_context(|| format!("read what {table} holds"))?;
        out.extend(
            rows.into_iter()
                .map(|(id, fetched, version)| (id, Held { fetched, version })),
        );
    }
    Ok(out)
}

/// Record that `table`'s content for `key` now satisfies `version`: in
/// the transaction that wrote the content. Clears the record's error
/// and its fetch problem.
pub async fn hold(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    key: &str,
    version: Option<&str>,
) -> Result<()> {
    datalib_etl::doltlite_raw::record_object_attempt(tx, table, key, None).await?;
    set_held_version(tx, table, key, version).await
}

/// [`hold`] for a record whose sidecar its writer has already stamped,
/// first-seen: only the version moves, so an unchanged record changes
/// nothing.
pub async fn set_held_version(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    key: &str,
    version: Option<&str>,
) -> Result<()> {
    // Audited: `table` is a provider's `&'static str`; values bound.
    let sql = format!("UPDATE {table}_bookkeeping SET held_version = ? WHERE id = ?");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(version)
        .bind(key)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("hold {table}={key}"))?;
    Ok(())
}

/// Upstream no longer has `key`: its sidecar row and its fetch problem
/// go, in the transaction that removes its content.
pub async fn forget(tx: &mut Transaction<'_, Sqlite>, table: &str, key: &str) -> Result<()> {
    // Audited: `table` is a provider's `&'static str`; the key is bound.
    let sql = format!("DELETE FROM {table}_bookkeeping WHERE id = ?");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget {table}={key}"))?;
    datalib_etl::prune::forget_record_problems_in_tx(tx, table, key).await
}

/// What one fetch of one owed record came to.
#[derive(Debug)]
pub enum Outcome<T> {
    /// Fetched: `store` writes it, and the record is held at the
    /// version it was listed at.
    Got(T),
    /// Fetched, and part of it cannot be used (a file that will not
    /// decode, a body cut short): `store` writes what there is, the
    /// record is held at its version, and the loss is a warning on it
    /// with the reason it names. It is not asked for again until the
    /// listing moves.
    Unusable(T, datalib_problems::Reason, String),
    /// Upstream no longer has it: `store` removes it, and nothing holds
    /// it any more.
    Gone,
    /// It did not come. The record stays owed, with one more attempt and
    /// this as its problem.
    Failed(String),
    /// Not asked for, by a rule of ours (over the size limit): the record
    /// stays owed, and the rule is a warning on it, not a failure.
    Skipped(datalib_problems::Reason, String),
}

/// One owed record and what its fetch came to.
#[derive(Debug)]
pub struct Fetched<T> {
    pub listed: Listed,
    pub outcome: Outcome<T>,
}

/// Why a whole batch did not come back.
#[derive(Debug)]
pub enum BatchError {
    /// This batch failed; every record in it stays owed with one more
    /// attempt, and the loop goes on.
    Batch(anyhow::Error),
    /// Nothing after this will fare better (a refused credential, the
    /// retry loop giving up): the loop ends, as a `phase:` row, and the
    /// run goes on with what it has.
    Terminal(anyhow::Error),
    /// The run can do nothing useful: what was answered is written, and
    /// `drain` returns this as its error.
    Abort(anyhow::Error),
}

/// How a provider fetches and stores one kind of record. Both methods
/// take `&self`, because fetches may run at once: a count the provider
/// keeps is an atomic or a mutex.
#[async_trait]
pub trait Fetcher<T: Send>: Sync {
    /// Ask upstream for `batch` and say what each record came to. A
    /// record left out of the answer is read as failed.
    async fn fetch(&self, batch: Vec<Listed>) -> std::result::Result<Vec<Fetched<T>>, BatchError>;

    /// Write a flush's content in `tx`, which then also records what is
    /// held, gone, failed and skipped, and commits. Bytes for a blob CAS
    /// go in here first, one `put_many` for the flush: the CAS is its own
    /// file and commits itself, so the bytes are there before the rows
    /// that name them, and a kill between leaves only unnamed bytes.
    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<T>],
    ) -> Result<()>;

    /// Bytes a fetched record holds until its flush, for
    /// [`Loop::flush_bytes`]. Zero for a record too small to matter.
    fn weight(&self, _content: &T) -> usize {
        0
    }
}

/// What the loop is working for.
pub struct Loop<'a> {
    pub pool: &'a SqlitePool,
    /// The table whose sidecar holds the versions, and whose key the
    /// records are listed by.
    pub table: &'static str,
    /// Names the `phase:` row when the loop gives up.
    pub phase: &'a str,
    pub stop: &'a StopFlag,
    pub found: &'a RunProblems,
    pub sealer: Option<&'a Sealer>,
    /// Records per request.
    pub batch: usize,
    /// Fetches in flight at once.
    pub concurrency: usize,
    /// Records per transaction. A stop or a give-up writes what was
    /// answered so far whatever the count.
    pub flush: usize,
    /// Bytes of fetched content per transaction, by [`Fetcher::weight`],
    /// so a run of large records flushes before it fills memory; `0`
    /// for no bound.
    pub flush_bytes: usize,
    /// Requests in a row that came to nothing before the loop gives up
    /// on this run; `0` for never.
    pub failures_in_a_row: usize,
}

/// How a [`drain`] ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub got: usize,
    pub gone: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Owed records the loop did not reach: it was stopped, gave up, or
    /// met a terminal error.
    pub left: usize,
    /// The terminal error the loop ended on, for a provider that has to
    /// decide what it means for the run.
    pub terminal: Option<String>,
}

/// Fetch `owed` through `f`. The loop ends early on a stop, after
/// `Loop::failures_in_a_row` fruitless requests, or on a terminal error;
/// the last two are a `phase:` row and `cut_short`. However it ends,
/// what was answered is written. An abort is returned after that.
pub async fn drain<T: Send, F: Fetcher<T>>(
    l: &Loop<'_>,
    owed: Vec<Listed>,
    f: &F,
) -> Result<Drained> {
    let mut done = Drained::default();
    let total = owed.len();
    let batch = l.batch.max(1);
    let flush = l.flush.max(1);
    let mut fruitless = 0usize;
    let mut pending: Vec<Fetched<T>> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut batches = owed.into_iter().peekable();
    let mut in_flight = FuturesUnordered::new();
    let mut ended: Option<End> = None;

    while ended.is_none() && (batches.peek().is_some() || !in_flight.is_empty()) {
        while in_flight.len() < l.concurrency.max(1) && batches.peek().is_some() {
            if l.stop.requested() {
                break;
            }
            let this: Vec<Listed> = batches.by_ref().take(batch).collect();
            in_flight.push(async move { (this.clone(), f.fetch(this).await) });
        }
        if in_flight.is_empty() {
            ended = Some(End::Stopped);
            break;
        }
        let Some((asked, answer)) = in_flight.next().await else {
            break;
        };
        match answer {
            Ok(answered) => {
                let answered = with_the_unanswered(asked, answered);
                if answered.iter().any(|a| a.outcome.is_fruit()) {
                    fruitless = 0;
                } else {
                    fruitless += 1;
                }
                pending_bytes += weigh(f, &answered);
                pending.extend(answered);
            }
            Err(_) if l.stop.requested() => {
                ended = Some(End::Stopped);
            }
            Err(BatchError::Batch(e)) => {
                let said = format!("{e:#}");
                pending.extend(asked.into_iter().map(|listed| Fetched {
                    listed,
                    outcome: Outcome::Failed(said.clone()),
                }));
                fruitless += 1;
            }
            Err(BatchError::Terminal(e)) => ended = Some(End::Terminal(format!("{e:#}"))),
            Err(BatchError::Abort(e)) => ended = Some(End::Abort(e)),
        }
        if ended.is_none() && l.failures_in_a_row > 0 && fruitless >= l.failures_in_a_row {
            let said = pending
                .iter()
                .rev()
                .find_map(|p| match &p.outcome {
                    Outcome::Failed(said) => Some(said.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            ended = Some(End::GaveUp(said));
        }
        let heavy = l.flush_bytes > 0 && pending_bytes >= l.flush_bytes;
        if pending.len() >= flush || heavy || ended.is_some() {
            write(l, f, &mut pending, &mut done).await?;
            pending_bytes = 0;
        }
    }
    // Requests still in flight at a stop or an end are dropped
    // unanswered; their records stay owed.
    drop(in_flight);
    write(l, f, &mut pending, &mut done).await?;
    done.left = total - done.got - done.gone - done.failed - done.skipped;
    match ended {
        None | Some(End::Stopped) => {}
        Some(End::GaveUp(said)) => give_up(
            l,
            &done,
            &format!("{} failed in a row", l.failures_in_a_row),
            &said,
        ),
        Some(End::Terminal(said)) => {
            give_up(l, &done, "a failure nothing after it would escape", &said);
            done.terminal = Some(said);
        }
        Some(End::Abort(e)) => return Err(e),
    }
    Ok(done)
}

enum End {
    Stopped,
    GaveUp(String),
    Terminal(String),
    Abort(anyhow::Error),
}

impl<T> Outcome<T> {
    fn is_fruit(&self) -> bool {
        matches!(
            self,
            Outcome::Got(_) | Outcome::Unusable(..) | Outcome::Gone
        )
    }
}

/// One transaction for everything answered so far: the provider's
/// content, then what each record is held at or failed with.
fn weigh<T: Send, F: Fetcher<T>>(f: &F, answered: &[Fetched<T>]) -> usize {
    answered
        .iter()
        .map(|a| match &a.outcome {
            Outcome::Got(content) | Outcome::Unusable(content, ..) => f.weight(content),
            _ => 0,
        })
        .sum()
}

async fn write<T: Send, F: Fetcher<T>>(
    l: &Loop<'_>,
    f: &F,
    pending: &mut Vec<Fetched<T>>,
    done: &mut Drained,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let answered = std::mem::take(pending);
    let mut tx = l.pool.begin().await.context("begin a flush")?;
    f.store(&mut tx, &answered).await?;
    for a in &answered {
        let key = a.listed.key.as_str();
        let version = a.listed.version.as_deref();
        match &a.outcome {
            Outcome::Got(_) => {
                hold(&mut tx, l.table, key, version).await?;
                done.got += 1;
            }
            Outcome::Unusable(_, reason, said) => {
                hold(&mut tx, l.table, key, version).await?;
                datalib_etl::doltlite_raw::record_object_unusable(
                    &mut tx, l.table, key, *reason, said,
                )
                .await?;
                done.got += 1;
            }
            Outcome::Gone => {
                forget(&mut tx, l.table, key).await?;
                done.gone += 1;
            }
            Outcome::Failed(said) => {
                datalib_etl::doltlite_raw::record_object_error(&mut tx, l.table, key, said).await?;
                done.failed += 1;
            }
            Outcome::Skipped(reason, said) => {
                datalib_etl::doltlite_raw::record_object_skipped(
                    &mut tx, l.table, key, *reason, said,
                )
                .await?;
                done.skipped += 1;
            }
        }
    }
    tx.commit().await.context("commit a flush")?;
    if let Some(sealer) = l.sealer {
        sealer.wrote(answered.len() as u64).await;
    }
    Ok(())
}

fn give_up(l: &Loop<'_>, done: &Drained, why: &str, said: &str) {
    // The problem's sample shows eighty characters: the cause first.
    l.found.phase(
        l.phase,
        format!(
            "{said}; stopped after {why}: {} fetched, {} left for the next run",
            done.got, done.left
        ),
    );
    l.found.cut_short();
}

/// Every record of `asked` with its outcome, the ones `fetch` said
/// nothing about as failed.
fn with_the_unanswered<T>(asked: Vec<Listed>, answered: Vec<Fetched<T>>) -> Vec<Fetched<T>> {
    let mut out = answered;
    for listed in asked {
        if !out.iter().any(|f| f.listed.key == listed.key) {
            out.push(Fetched {
                listed,
                outcome: Outcome::Failed("the fetch said nothing about it".to_string()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::Reason;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const T: &str = "things";

    async fn store_at(dir: &tempfile::TempDir) -> SqlitePool {
        let ddl = [
            "CREATE TABLE IF NOT EXISTS things (id TEXT PRIMARY KEY, body TEXT NULL)",
            &datalib_etl::doltlite_raw::bookkeeping_ddl_for(T),
        ];
        let ddl: Vec<&str> = ddl.iter().map(|s| &**s).collect();
        datalib_etl::doltlite_raw::open(&dir.path().join("o.doltlite_db"), &ddl)
            .await
            .unwrap()
    }

    fn listed(key: &str, version: Option<&str>) -> Listed {
        Listed::new(key, version)
    }

    async fn bodies(pool: &SqlitePool) -> Vec<(String, Option<String>)> {
        sqlx::query_as("SELECT id, body FROM things ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn problems(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    fn a_loop<'a>(
        pool: &'a SqlitePool,
        stop: &'a StopFlag,
        found: &'a RunProblems,
        batch: usize,
        budget: usize,
    ) -> Loop<'a> {
        Loop {
            pool,
            table: T,
            phase: "things",
            stop,
            found,
            sealer: None,
            batch,
            concurrency: 1,
            flush: batch,
            flush_bytes: 0,
            failures_in_a_row: budget,
        }
    }

    /// A fetcher scripted per key: what each record comes to, and how
    /// many requests it has made.
    struct Scripted {
        answers: Mutex<HashMap<String, Answer>>,
        requests: AtomicUsize,
        stop: Option<StopFlag>,
        batch_error: Option<fn() -> BatchError>,
    }

    #[derive(Clone)]
    enum Answer {
        Unusable(String, String),
        Gone,
        Failed(String),
        Skipped(String),
        Silent,
    }

    impl Scripted {
        fn all_got() -> Self {
            Self {
                answers: Mutex::new(HashMap::new()),
                requests: AtomicUsize::new(0),
                stop: None,
                batch_error: None,
            }
        }
        fn with(mut self, key: &str, answer: Answer) -> Self {
            self.answers
                .get_mut()
                .unwrap()
                .insert(key.to_string(), answer);
            self
        }
    }

    #[async_trait]
    impl Fetcher<String> for Scripted {
        async fn fetch(
            &self,
            batch: Vec<Listed>,
        ) -> std::result::Result<Vec<Fetched<String>>, BatchError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if let Some(stop) = &self.stop {
                stop.request();
            }
            if let Some(err) = self.batch_error {
                return Err(err());
            }
            let answers = self.answers.lock().unwrap();
            Ok(batch
                .into_iter()
                .filter_map(|listed| {
                    let outcome = match answers.get(&listed.key).cloned() {
                        None => Outcome::Got(format!("log of {}", listed.key)),
                        Some(Answer::Unusable(body, why)) => {
                            Outcome::Unusable(body, Reason::DeliberateLoss, why)
                        }
                        Some(Answer::Gone) => Outcome::Gone,
                        Some(Answer::Failed(why)) => Outcome::Failed(why),
                        Some(Answer::Skipped(why)) => Outcome::Skipped(Reason::OverSizeLimit, why),
                        Some(Answer::Silent) => return None,
                    };
                    Some(Fetched { listed, outcome })
                })
                .collect())
        }

        async fn store(
            &self,
            tx: &mut Transaction<'static, Sqlite>,
            batch: &[Fetched<String>],
        ) -> Result<()> {
            for f in batch {
                match &f.outcome {
                    Outcome::Got(body) | Outcome::Unusable(body, ..) => {
                        sqlx::query("INSERT INTO things (id, body) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET body = excluded.body")
                            .bind(&f.listed.key)
                            .bind(body)
                            .execute(&mut **tx)
                            .await?;
                    }
                    Outcome::Gone => {
                        sqlx::query("DELETE FROM things WHERE id = ?")
                            .bind(&f.listed.key)
                            .execute(&mut **tx)
                            .await?;
                    }
                    Outcome::Failed(_) | Outcome::Skipped(..) => {}
                }
            }
            Ok(())
        }
    }

    /// The whole rule in one place: a record is owed until its content
    /// is held at the version it is listed at, and again once the
    /// listing moves on.
    #[tokio::test]
    async fn a_record_is_owed_until_held_at_its_listed_version() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 10, 0);
        let listing = || {
            vec![
                listed("picard", Some("v1")),
                listed("riker", Some("v1")),
                listed("data", None),
            ]
        };
        let first = owed(&pool, T, listing()).await.unwrap();
        assert_eq!(first.len(), 3);
        let drained = drain(&l, first, &Scripted::all_got()).await.unwrap();
        assert_eq!(drained.got, 3);
        assert!(owed(&pool, T, listing()).await.unwrap().is_empty());

        let later = vec![
            listed("picard", Some("v2")),
            listed("riker", Some("v1")),
            listed("data", None),
        ];
        assert_eq!(
            owed(&pool, T, later).await.unwrap(),
            [listed("picard", Some("v2"))]
        );
        pool.close().await;
    }

    /// A record with no version only has to have been fetched. A failed
    /// attempt leaves a sidecar row too, and that row must not read as
    /// held: the record is owed again until a fetch lands. (Slack's
    /// files found this.)
    #[tokio::test]
    async fn a_record_that_failed_is_owed_again_whatever_its_version() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 10, 0);
        let listing = || vec![listed("picture", None), listed("log", Some("v1"))];
        drain(
            &l,
            listing(),
            &Scripted::all_got()
                .with("picture", Answer::Failed("HTTP 500".into()))
                .with("log", Answer::Failed("HTTP 500".into())),
        )
        .await
        .unwrap();
        assert_eq!(owed(&pool, T, listing()).await.unwrap(), listing());

        drain(&l, listing(), &Scripted::all_got()).await.unwrap();
        assert!(owed(&pool, T, listing()).await.unwrap().is_empty());
        assert!(problems(&pool).await.is_empty());
        pool.close().await;
    }

    /// Each record of a batch has its own outcome, all landing in the
    /// flush's one transaction: fetched, fetched but unusable (held, with
    /// a warning), gone (content and sidecar go), failed (owed, with an
    /// error), skipped by a rule (owed, with a warning), and silently
    /// unanswered (a failure).
    #[tokio::test]
    async fn each_record_of_a_batch_has_its_own_outcome() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 10, 0);
        let listing: Vec<Listed> = ["picard", "riker", "worf", "data", "troi", "crusher"]
            .iter()
            .map(|k| listed(k, Some("v1")))
            .collect();
        let drained = drain(
            &l,
            listing.clone(),
            &Scripted::all_got()
                .with("riker", Answer::Failed("HTTP 500".into()))
                .with("worf", Answer::Gone)
                .with("data", Answer::Silent)
                .with(
                    "troi",
                    Answer::Unusable("half a log".into(), "cut short".into()),
                )
                .with("crusher", Answer::Skipped("over the size limit".into())),
        )
        .await
        .unwrap();
        assert_eq!(
            drained,
            Drained {
                got: 2,
                gone: 1,
                failed: 2,
                skipped: 1,
                left: 0,
                terminal: None
            }
        );
        assert_eq!(
            bodies(&pool).await,
            [
                ("crusher".to_string(), None),
                ("data".to_string(), None),
                ("picard".to_string(), Some("log of picard".to_string())),
                ("riker".to_string(), None),
                ("troi".to_string(), Some("half a log".to_string())),
            ],
            "a failed or skipped record keeps the id-only stub every fetch problem has"
        );
        assert_eq!(
            owed(&pool, T, listing.clone())
                .await
                .unwrap()
                .iter()
                .map(|l| l.key.as_str())
                .collect::<Vec<_>>(),
            ["riker", "worf", "data", "crusher"]
        );
        assert_eq!(
            problems(&pool).await,
            [
                ("things:crusher".to_string(), "warning".to_string()),
                ("things:data".to_string(), "error".to_string()),
                ("things:riker".to_string(), "error".to_string()),
                ("things:troi".to_string(), "warning".to_string()),
            ]
        );
        pool.close().await;
    }

    /// Fetching and writing have their own sizes: one request per record
    /// and many records per transaction. A stop lands between requests,
    /// and what was answered before it is written, not thrown away with
    /// the flush under way.
    #[tokio::test]
    async fn a_stop_keeps_what_was_answered_before_it() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let mut l = a_loop(&pool, &stop, &found, 1, 0);
        l.flush = 100;
        let listing: Vec<Listed> = ["a", "b", "c", "d"]
            .iter()
            .map(|k| listed(k, Some("v1")))
            .collect();
        let mut f = Scripted::all_got();
        f.stop = Some(stop.clone());
        let drained = drain(&l, listing.clone(), &f).await.unwrap();
        assert_eq!(
            drained.got, 1,
            "the request that saw the stop still answered"
        );
        assert_eq!(drained.left, 3);
        assert_eq!(f.requests.load(Ordering::SeqCst), 1);
        assert_eq!(bodies(&pool).await.len(), 1);
        assert_eq!(owed(&pool, T, listing).await.unwrap().len(), 3);
        assert!(found.run_problems().is_empty());
        pool.close().await;
    }

    /// Fruitless requests in a row end the run's loop with one `phase:`
    /// row and leave the rest owed; a terminal error does the same at
    /// once and is handed back; an abort writes what came and returns
    /// the error.
    #[tokio::test]
    async fn the_loop_gives_up_as_a_phase_row_and_leaves_the_rest_owed() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let listing: Vec<Listed> = ["a", "b", "c", "d"]
            .iter()
            .map(|k| listed(k, Some("v1")))
            .collect();

        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 1, 2);
        let mut f = Scripted::all_got();
        f.batch_error = Some(|| BatchError::Batch(anyhow::anyhow!("HTTP 503")));
        let drained = drain(&l, listing.clone(), &f).await.unwrap();
        assert_eq!((drained.failed, drained.left), (2, 2));
        let phases = found.run_problems();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].key(), "phase:things");
        assert!(phases[0].detail.contains("2 left"), "{}", phases[0].detail);
        assert_eq!(problems(&pool).await.len(), 2);

        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 1, 0);
        let mut f = Scripted::all_got();
        f.batch_error = Some(|| BatchError::Terminal(anyhow::anyhow!("HTTP 401")));
        let drained = drain(&l, listing.clone(), &f).await.unwrap();
        assert_eq!(drained.left, 4);
        assert!(drained.terminal.unwrap().contains("HTTP 401"));
        assert_eq!(found.run_problems().len(), 1);

        let found = RunProblems::unwritten();
        let l = a_loop(&pool, &stop, &found, 1, 0);
        let mut f = Scripted::all_got();
        f.batch_error = Some(|| BatchError::Abort(anyhow::anyhow!("the store is read-only")));
        let err = drain(&l, listing, &f).await.unwrap_err();
        assert!(format!("{err:#}").contains("read-only"));
        pool.close().await;
    }

    /// Several requests at once, written as they come; the result is the
    /// same as one at a time.
    #[tokio::test]
    async fn requests_may_run_at_once() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let mut l = a_loop(&pool, &stop, &found, 1, 0);
        l.concurrency = 4;
        l.flush = 3;
        let listing: Vec<Listed> = (0..10)
            .map(|i| listed(&format!("k{i:02}"), Some("v1")))
            .collect();
        let f = Scripted::all_got().with("k05", Answer::Failed("HTTP 500".into()));
        let drained = drain(&l, listing.clone(), &f).await.unwrap();
        assert_eq!((drained.got, drained.failed), (9, 1));
        assert_eq!(f.requests.load(Ordering::SeqCst), 10);
        assert_eq!(
            owed(&pool, T, listing).await.unwrap(),
            [listed("k05", Some("v1"))]
        );
        pool.close().await;
    }

    /// A fetcher that says each record weighs its key's length, and
    /// counts the flushes it was asked to store.
    struct Heavy {
        flushes: AtomicUsize,
        sizes: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl Fetcher<String> for Heavy {
        async fn fetch(
            &self,
            batch: Vec<Listed>,
        ) -> std::result::Result<Vec<Fetched<String>>, BatchError> {
            Ok(batch
                .into_iter()
                .map(|listed| Fetched {
                    outcome: Outcome::Got(listed.key.clone()),
                    listed,
                })
                .collect())
        }
        async fn store(
            &self,
            _tx: &mut Transaction<'static, Sqlite>,
            batch: &[Fetched<String>],
        ) -> Result<()> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            self.sizes.lock().unwrap().push(batch.len());
            Ok(())
        }
        fn weight(&self, content: &String) -> usize {
            content.len()
        }
    }

    /// Large records flush by bytes before the record count is reached,
    /// so a run of big bodies never waits for `flush` of them.
    #[tokio::test]
    async fn heavy_records_flush_by_bytes() {
        let d = tempfile::tempdir().unwrap();
        let pool = store_at(&d).await;
        let stop = StopFlag::new();
        let found = RunProblems::unwritten();
        let mut l = a_loop(&pool, &stop, &found, 1, 0);
        l.flush = 100;
        l.flush_bytes = 10;
        // Each key is 4 bytes: a flush fills at the third record.
        let listing: Vec<Listed> = (0..7)
            .map(|i| listed(&format!("k{i:03}"), Some("v1")))
            .collect();
        let f = Heavy {
            flushes: AtomicUsize::new(0),
            sizes: Mutex::new(Vec::new()),
        };
        let drained = drain(&l, listing.clone(), &f).await.unwrap();
        assert_eq!(drained.got, 7);
        assert_eq!(*f.sizes.lock().unwrap(), [3, 3, 1]);
        assert_eq!(f.flushes.load(Ordering::SeqCst), 3);
        assert!(owed(&pool, T, listing).await.unwrap().is_empty());
        pool.close().await;
    }
}
