//! What the forge providers' downloads (github, gitlab) share. A forge
//! sync reads the account it runs as, lists the change requests — pull
//! requests or merge requests — the person is on through a handful of
//! searches, and fetches each one the store does not hold at the
//! version the listing named, with its comments. [`sync`] is that run;
//! a [`Forge`] is what one forge does differently.
//!
//! The listing is stored: one row per change request a search has
//! named, at the `updated_at` the search gave (`listed_change_requests`).
//! What each search looked at is a `coverage` span of `updated_at` per
//! scope, so a widened window or a search cut short is a gap, not a
//! cursor. What is held is the change-request table's sidecar
//! `held_version`, written in the one transaction that writes the
//! record and its children. Owed is the difference, fetched through
//! `datalib_etl_web::owed` (docs/dev/data_architecture_ingestion.md, "What is left to fetch").

pub mod client;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Loop, Outcome};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::Value;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

pub use client::{ForgeClient, ForgeError, Search, LATCHKEY_TIMEOUT, PER_PAGE};

/// The stored listing: every change request a search has named, keyed
/// as [`Forge::item_key`], at the newest `updated_at` any search gave
/// it. No sidecar: it is what upstream said, not something fetched.
pub const LISTED: &str = "listed_change_requests";
pub const LISTED_DDL: &str = "CREATE TABLE IF NOT EXISTS listed_change_requests (\
    id TEXT PRIMARY KEY, updated_at TEXT NULL)";

/// Requests in a row that came to nothing before the fetch loop gives
/// up on this run: a credential every change request refuses should not
/// cost one request per change request.
const FAILURE_BUDGET: usize = 25;

/// A change request a search listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The repository or project.
    pub container: String,
    pub number: u32,
    /// When the forge last saw it change, from the listing; empty when
    /// the listing did not say. Empty is a listing with no version: the
    /// change request only has to have been fetched.
    pub updated_at: String,
}

/// Where one search looks: change requests updated at or after `lo` and
/// at or before `hi`, each in the forge's own spelling of `updated_at`;
/// `None` is open at that end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bounds {
    pub lo: Option<String>,
    pub hi: Option<String>,
}

/// What fetching one change request came to.
#[derive(Debug)]
pub enum Answer<C> {
    Whole(C),
    /// What it could not do, one line each. Nothing of it is stored: it
    /// stays owed, and the next run asks for all of it again.
    Short(Vec<String>),
    /// The forge says it is not there: a deleted repository, or one this
    /// credential no longer reaches. It leaves the listing; a copy the
    /// store holds is kept, as the last one there was.
    Gone,
}

impl<C> Answer<C> {
    pub fn from_shortfalls(content: C, lines: Vec<String>) -> Self {
        if lines.is_empty() {
            Answer::Whole(content)
        } else {
            Answer::Short(lines)
        }
    }
}

#[async_trait]
pub trait Forge: Sync {
    type Summary: Default + Serialize + Send;
    /// One change request as fetched, held until its flush.
    type Content: Send + Sync;
    /// `PR` or `MR`, in logs.
    const ITEM: &'static str;
    /// What goes between container and number: `#` or `!`.
    const SIGIL: char;
    /// The table a change request's own record lands in; its sidecar
    /// holds the version the record satisfies.
    const ITEM_TABLE: &'static str;

    fn pool(&self) -> &SqlitePool;

    /// Where the account the run authenticates as is read.
    fn self_url(&self) -> String;

    async fn store_self(&self, me: &Value) -> Result<()>;

    /// The change requests of `scope` updated within `bounds`, newest
    /// first.
    async fn search(
        &self,
        client: &ForgeClient,
        scope: &str,
        me: &Value,
        bounds: &Bounds,
    ) -> Result<Search>;

    /// A search result as a change request; `None` for one that names
    /// none.
    fn listed(&self, item: &Value) -> Option<Listed>;

    /// A change request's id in [`Self::ITEM_TABLE`]: container, sigil,
    /// number, which [`split_item_key`] reads back.
    fn item_key(&self, container: &str, number: u32) -> String;

    /// `at` spelled as this forge spells `updated_at`, so a span's ends
    /// sort with the stamps the listing gives.
    fn stamp(&self, at: &IsoOffsetTimestamp) -> String;

    /// Whether the store holds any change request yet. An empty one
    /// searches without a floor, whatever the refresh window says.
    async fn any_stored(&self) -> Result<bool>;

    /// Fetch one change request and everything under it. `Err` is a
    /// give-up of the shared retry loop, or anything else that ends the
    /// run.
    async fn fetch_one(
        &self,
        client: &ForgeClient,
        container: &str,
        number: u32,
    ) -> Result<Answer<Self::Content>>;

    /// Write one fetched change request, its children, and the prune of
    /// the children its fresh lists no longer name, in `tx`. The loop
    /// then records the version it is held at in the same transaction.
    async fn store_one(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        container: &str,
        number: u32,
        content: &Self::Content,
        summary: &mut Self::Summary,
    ) -> Result<()>;

    /// Listed at a version the store already holds: not fetched.
    fn record_unchanged(&self, _summary: &mut Self::Summary, _count: usize) {}

    fn record_requests(&self, summary: &mut Self::Summary, requests: u64);
}

/// What one sync is asked to do.
pub struct SyncOptions<'a> {
    /// Discovery scopes, as the forge's search takes them.
    pub scopes: &'a [String],
    /// On a store with data, search only for change requests updated in
    /// the last N days; 0 is unbounded. A first search has no floor.
    pub refresh_window_days: u32,
    /// Cap on how many are fetched in one run (`None` = unbounded). The
    /// rest stay owed, and later runs fetch them before anything
    /// fetched before.
    pub max_items: Option<usize>,
    /// Change requests named directly. When any are, nothing is searched
    /// and only these are fetched, whatever the store holds.
    pub targets: &'a [(String, u32)],
    /// Search everything and fetch everything listed, for a full
    /// backfill.
    pub full_sync: bool,
    /// The run's pinned clock: the top of every search, and the point the
    /// refresh window is measured from.
    pub now: &'a IsoOffsetTimestamp,
    /// Raised when the step is asked to stop.
    pub stop: &'a StopFlag,
    pub sleep_between: Duration,
    pub progress: &'a Progress,
    /// Seals as change requests land, when the step driver hands one
    /// over.
    pub sealer: Option<&'a Sealer>,
    /// The run's knobs, as `sync_runs` records them.
    pub run_config: Value,
}

pub async fn sync<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    opts: SyncOptions<'_>,
) -> Result<F::Summary> {
    let stop = opts.stop.clone();
    let sealer = opts.sealer.cloned();
    run_problems::collecting_sealed(forge.pool(), &stop, sealer.as_ref(), |found| {
        sync_collecting(forge, client, opts, found)
    })
    .await
}

async fn sync_collecting<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    opts: SyncOptions<'_>,
    found: RunProblems,
) -> Result<F::Summary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let run = DownloadRun::start(forge.pool(), &opts.run_config).await?;
    let fetcher = ChangeRequests {
        forge,
        client,
        opts: &opts,
        summary: tokio::sync::Mutex::new(F::Summary::default()),
    };
    let result = work(forge, client, &opts, &found, &fetcher).await;
    let mut summary = fetcher.summary.into_inner();
    forge.record_requests(&mut summary, client.request_count());
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

async fn work<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    opts: &SyncOptions<'_>,
    found: &RunProblems,
    fetcher: &ChangeRequests<'_, F>,
) -> Result<()> {
    let pool = forge.pool();
    let (me, _) = client.get(&forge.self_url()).await?;
    if !me.is_object() {
        anyhow::bail!("{} returned non-object", forge.self_url());
    }
    forge.store_self(&me).await?;

    let to_fetch: Vec<owed::Listed> = if opts.targets.is_empty() {
        if let Some(gave_up) = discover(forge, client, &me, opts, found).await? {
            // No request after this one will fare better: the next run
            // searches again, and what this one listed is owed to it.
            found.push(gave_up);
            found.cut_short();
            return Ok(());
        }
        let listed = listing(pool).await?;
        let owed = if opts.full_sync {
            listed
        } else {
            let count = listed.len();
            let owed = owed::owed(pool, F::ITEM_TABLE, listed).await?;
            forge.record_unchanged(&mut *fetcher.summary.lock().await, count - owed.len());
            owed
        };
        never_fetched_first(pool, F::ITEM_TABLE, owed).await?
    } else {
        // Named directly: no search ran, so this run has no verdict on
        // any listing, and nothing holds a named change request back.
        found.cut_short();
        opts.targets
            .iter()
            .map(|(container, number)| {
                owed::Listed::new(forge.item_key(container, *number), None::<String>)
            })
            .collect()
    };
    let cap = opts.max_items.unwrap_or(usize::MAX);
    let left_over = to_fetch.len().saturating_sub(cap);
    let to_fetch: Vec<owed::Listed> = to_fetch.into_iter().take(cap).collect();
    tracing::info!(count = to_fetch.len(), left_over, "{}s to fetch", F::ITEM);
    opts.progress.set_length(Some(to_fetch.len() as u64));
    let l = Loop {
        pool,
        table: F::ITEM_TABLE,
        phase: "fetch",
        stop: opts.stop,
        found,
        sealer: opts.sealer,
        batch: 1,
        concurrency: 1,
        flush: 1,
        flush_bytes: 0,
        failures_in_a_row: FAILURE_BUDGET,
    };
    let drained = owed::drain(&l, to_fetch, fetcher).await?;
    if left_over > 0 {
        tracing::info!(
            left_over,
            fetched = drained.got,
            "{}s past this run's cap of {cap} stay owed to the next run",
            F::ITEM
        );
    }
    Ok(())
}

/// The listing as stored, by key.
async fn listing(pool: &SqlitePool) -> Result<Vec<owed::Listed>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, updated_at FROM listed_change_requests ORDER BY id")
            .fetch_all(pool)
            .await
            .context("read the listing")?;
    Ok(rows
        .into_iter()
        .map(|(id, updated_at)| owed::Listed::new(id, updated_at))
        .collect())
}

/// `owed` in the order a run should fetch it: what no fetch ever landed
/// for before what is held at an older version, and among those the
/// least tried first, so a capped run pays for new work before it
/// refreshes old, and one change request that fails every run does not
/// stand in front of the rest.
async fn never_fetched_first(
    pool: &SqlitePool,
    table: &str,
    owed: Vec<owed::Listed>,
) -> Result<Vec<owed::Listed>> {
    let mut tried: HashMap<String, (bool, i64)> = HashMap::with_capacity(owed.len());
    for chunk in owed.chunks(datalib_etl::bulk::SQL_CHUNK) {
        // Audited: `table` is a provider's `&'static str`; the
        // placeholders are one `?` per key, every key bound.
        let sql = format!(
            "SELECT id, fetched_at_utc IS NOT NULL, attempt_count \
             FROM {table}_bookkeeping WHERE id IN ({})",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query_as::<_, (String, bool, i64)>(sqlx::AssertSqlSafe(sql));
        for l in chunk {
            q = q.bind(&l.key);
        }
        let rows = q
            .fetch_all(pool)
            .await
            .with_context(|| format!("read what {table} has tried"))?;
        tried.extend(rows.into_iter().map(|(id, fetched, n)| (id, (fetched, n))));
    }
    let mut owed = owed;
    owed.sort_by_cached_key(|l| {
        let (fetched, attempts) = tried.get(&l.key).copied().unwrap_or((false, 0));
        (fetched, attempts, l.key.clone())
    });
    Ok(owed)
}

/// The `coverage` scope of one search scope.
pub fn coverage_scope(scope: &str) -> String {
    format!("search:{scope}")
}

/// The `updated_at` a search's span of coverage reaches up to: the run's
/// now, or the newest result when the forge's clock is ahead of it.
pub fn covered_hi<'a>(now: &'a str, newest_listed: Option<&'a str>) -> &'a str {
    match newest_listed {
        Some(newest) if newest > now => newest,
        _ => now,
    }
}

/// The span of `updated_at` a search of `gap` looked at whole: nothing
/// of an incomplete answer, and of a truncated one only down to the
/// oldest result it gave.
fn covered(gap: &Span, now: &str, search: &Search, listed: &[Listed]) -> Option<Span> {
    if search.incomplete {
        return None;
    }
    let stamps = listed
        .iter()
        .map(|l| l.updated_at.as_str())
        .filter(|s| !s.is_empty());
    let newest = stamps.clone().max();
    let hi = if gap.hi.as_str() >= now {
        covered_hi(now, newest).to_string()
    } else {
        gap.hi.clone()
    };
    let lo = if search.truncated {
        stamps.min()?.to_string()
    } else {
        gap.lo.clone()
    };
    Some(Span::new(lo, hi))
}

/// Where a gap is searched: open at the bottom when it starts at the
/// beginning of time, open at the top when it reaches the run's now, so
/// a change request updated after the run began is listed too.
fn bounds_of(gap: &Span, now: &str) -> Bounds {
    Bounds {
        lo: (!gap.lo.is_empty()).then(|| gap.lo.clone()),
        hi: (gap.hi.as_str() < now).then(|| gap.hi.clone()),
    }
}

/// `days` before `now`, or `None` when that is before the Unix epoch:
/// no forge has anything that old, and a year below 1 neither sorts as
/// a `coverage` bound nor is a date a search accepts.
fn refresh_floor(now: &IsoOffsetTimestamp, days: u32) -> Option<IsoOffsetTimestamp> {
    let at = now
        .inner()
        .checked_sub_signed(chrono::Duration::days(days.into()))?;
    (at.timestamp() >= 0).then(|| IsoOffsetTimestamp::from(at))
}

/// Every scope's searches, each gap of what it has not looked at in
/// turn, newest first. Each search's results and its span land in one
/// transaction. Returns the give-up that ended the searches, if one did.
async fn discover<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    me: &Value,
    opts: &SyncOptions<'_>,
    found: &RunProblems,
) -> Result<Option<RunProblem>> {
    let pool = forge.pool();
    let now = forge.stamp(opts.now);
    let from_the_start =
        opts.full_sync || opts.refresh_window_days == 0 || !forge.any_stored().await?;
    let floor = if from_the_start {
        String::new()
    } else {
        refresh_floor(opts.now, opts.refresh_window_days)
            .map(|at| forge.stamp(&at))
            .unwrap_or_default()
    };
    let wanted = Span::new(floor, now.clone());
    for scope in opts.scopes {
        if opts.stop.requested() {
            break;
        }
        let key = coverage_scope(scope);
        let held = if opts.full_sync {
            Vec::new()
        } else {
            coverage::held(pool, &key).await?
        };
        let name = format!("search {scope}");
        for gap in coverage::gaps(&wanted, &held).into_iter().rev() {
            if opts.stop.requested() {
                break;
            }
            let bounds = bounds_of(&gap, &now);
            tracing::info!(
                scope,
                from = bounds.lo.as_deref().unwrap_or(""),
                to = bounds.hi.as_deref().unwrap_or(""),
                "searching {}s",
                F::ITEM
            );
            let search = match forge.search(client, scope, me, &bounds).await {
                Ok(search) => search,
                // After a stop every request fails at once; that is not
                // something the search did.
                Err(_) if opts.stop.requested() => return Ok(None),
                Err(e) if is_give_up(&e) => {
                    return Ok(Some(RunProblem::phase(
                        "search",
                        format!(
                            "the retry loop gave up searching {scope} ({e:#}); \
                             nothing was fetched, and the next run searches again"
                        ),
                    )));
                }
                Err(e) => {
                    let refused = e
                        .downcast_ref::<ForgeError>()
                        .is_some_and(ForgeError::refused);
                    found.push(if refused {
                        RunProblem::forbidden(&name, format!("{e:#}"))
                    } else {
                        RunProblem::listing(&name, format!("{e:#}"))
                    });
                    break;
                }
            };
            let listed: Vec<Listed> = search
                .items
                .iter()
                .filter_map(|item| forge.listed(item))
                .collect();
            let span = covered(&gap, &now, &search, &listed);
            store_listing(forge, &key, &listed, span, opts.sealer).await?;
            if search.incomplete {
                found.listing(
                    &name,
                    format!(
                        "the forge answered an incomplete set of results, so nothing of this \
                         search counts as looked at; the next run asks again ({} listed)",
                        listed.len()
                    ),
                );
            } else if search.truncated {
                found.listing(
                    &name,
                    format!(
                        "the search has more results than it answers, so what was updated \
                         before the oldest of the {} it gave was not listed; the next run \
                         lists it",
                        listed.len()
                    ),
                );
            }
            tracing::info!(scope, count = listed.len(), "scope done");
        }
    }
    Ok(None)
}

/// One search's results and the span it covered, in one transaction: a
/// change request listed before keeps the newest version any search
/// gave it.
async fn store_listing<F: Forge>(
    forge: &F,
    scope: &str,
    listed: &[Listed],
    covered: Option<Span>,
    sealer: Option<&Sealer>,
) -> Result<()> {
    let mut tx = forge.pool().begin().await.context("begin a listing")?;
    for l in listed {
        let key = forge.item_key(&l.container, l.number);
        let version = (!l.updated_at.is_empty()).then_some(l.updated_at.as_str());
        sqlx::query(
            "INSERT INTO listed_change_requests (id, updated_at) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at \
             WHERE excluded.updated_at IS NOT NULL \
               AND (listed_change_requests.updated_at IS NULL \
                    OR excluded.updated_at > listed_change_requests.updated_at)",
        )
        .bind(&key)
        .bind(version)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("list {key}"))?;
    }
    if let Some(span) = covered {
        coverage::cover(&mut tx, scope, span).await?;
    }
    tx.commit().await.context("commit a listing")?;
    if let Some(sealer) = sealer {
        sealer.wrote(listed.len() as u64).await;
    }
    Ok(())
}

/// Upstream no longer has `key`: it leaves the listing. A copy the
/// store holds stays, as the last one there was.
async fn forget_listed(tx: &mut Transaction<'_, Sqlite>, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM listed_change_requests WHERE id = ?")
        .bind(key)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("unlist {key}"))?;
    Ok(())
}

/// The fetch loop's view of a forge: one request per change request,
/// each stored with its children in a transaction of its own.
struct ChangeRequests<'a, F: Forge> {
    forge: &'a F,
    client: &'a ForgeClient,
    opts: &'a SyncOptions<'a>,
    summary: tokio::sync::Mutex<F::Summary>,
}

#[async_trait]
impl<F: Forge> Fetcher<F::Content> for ChangeRequests<'_, F> {
    async fn fetch(
        &self,
        batch: Vec<owed::Listed>,
    ) -> std::result::Result<Vec<Fetched<F::Content>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some((container, number)) = split_item_key(&listed.key, F::SIGIL) else {
                out.push(Fetched {
                    outcome: Outcome::Failed(format!("{} names no {}", listed.key, F::ITEM)),
                    listed,
                });
                continue;
            };
            self.opts
                .progress
                .set_message(&format!("{container}{}{number}", F::SIGIL));
            let answer = self.forge.fetch_one(self.client, &container, number).await;
            // After a stop every request fails at once; that is not
            // something the change request did.
            let stopped = self.opts.stop.requested();
            let outcome = match answer {
                Ok(Answer::Whole(content)) => Outcome::Got(content),
                Ok(Answer::Gone) => Outcome::Gone,
                Ok(Answer::Short(_)) | Err(_) if stopped => {
                    return Err(BatchError::Batch(anyhow::anyhow!("stopped")));
                }
                Ok(Answer::Short(lines)) => Outcome::Failed(lines.join("; ")),
                Err(e) if is_give_up(&e) => return Err(BatchError::Terminal(e)),
                Err(e) => return Err(BatchError::Abort(e)),
            };
            self.opts.progress.inc(1);
            out.push(Fetched { listed, outcome });
            if self.opts.sleep_between > Duration::ZERO {
                tokio::time::sleep(self.opts.sleep_between).await;
            }
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<F::Content>],
    ) -> Result<()> {
        let mut summary = self.summary.lock().await;
        for f in batch {
            match &f.outcome {
                Outcome::Got(content) | Outcome::Unusable(content, ..) => {
                    let Some((container, number)) = split_item_key(&f.listed.key, F::SIGIL) else {
                        continue;
                    };
                    self.forge
                        .store_one(tx, &container, number, content, &mut summary)
                        .await?;
                }
                Outcome::Gone => forget_listed(tx, &f.listed.key).await?,
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        Ok(())
    }
}

fn is_give_up(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ForgeError>()
        .is_some_and(ForgeError::gave_up)
}

/// `(container, number)` back out of an [`Forge::item_key`].
pub fn split_item_key(id: &str, sigil: char) -> Option<(String, u32)> {
    let (container, number) = id.rsplit_once(sigil)?;
    Some((container.to_string(), number.parse().ok()?))
}

/// Rung 1 of a forge store's ladder (etl/README.md §"The migration
/// ladder"). A change request was held by having a payload and no
/// `last_error`; each becomes a `held_version` in its sidecar, at the
/// `updated_at` its record carries. Every record is listed at that
/// stamp, and one only ever tried — a sidecar with no record, since the
/// record's columns cannot be null — at none, so what was not fetched
/// whole is owed. Each scope's cursor becomes the span from the
/// beginning of time up to it, so the next run searches from there, and
/// the rows a cap wrote as skipped go: owed needs no row.
pub async fn rung_listed_minus_held(
    conn: &mut SqliteConnection,
    table: &'static str,
    stamp: fn(&IsoOffsetTimestamp) -> String,
) -> Result<()> {
    let has = |table: &'static str| {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
        )
        .bind(table)
    };
    for ddl in [LISTED_DDL, coverage::DDL] {
        sqlx::query(ddl).execute(&mut *conn).await?;
    }
    let has_column: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = 'held_version')",
    )
    .bind(format!("{table}_bookkeeping"))
    .fetch_one(&mut *conn)
    .await?;
    if !has_column {
        // Audited: `table` is a provider's `&'static str`.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {table}_bookkeeping ADD COLUMN held_version TEXT NULL"
        )))
        .execute(&mut *conn)
        .await?;
    }
    // Audited: both interpolate only `table`.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table}_bookkeeping SET held_version = \
            (SELECT t.updated_at FROM {table} t WHERE t.id = {table}_bookkeeping.id) \
         WHERE fetched_at_utc IS NOT NULL AND last_error IS NULL \
           AND id IN (SELECT id FROM {table} WHERE payload IS NOT NULL)"
    )))
    .execute(&mut *conn)
    .await?;
    for sql in [
        format!(
            "INSERT OR IGNORE INTO listed_change_requests (id, updated_at) \
             SELECT id, updated_at FROM {table}"
        ),
        format!(
            "INSERT OR IGNORE INTO listed_change_requests (id, updated_at) \
             SELECT id, NULL FROM {table}_bookkeeping"
        ),
    ] {
        // Audited: only `table` is interpolated.
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await?;
    }
    if has("problems").fetch_one(&mut *conn).await? {
        sqlx::query(
            "DELETE FROM problems WHERE scope_kind = ? AND stage = ? AND reason = ? \
             AND scope_key LIKE ? || ':%'",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(datalib_problems::Stage::Fetch.as_str())
        .bind(datalib_problems::Reason::OverSizeLimit.as_str())
        .bind(table)
        .execute(&mut *conn)
        .await?;
    }
    if has("sync_scope_state").fetch_one(&mut *conn).await? {
        let cursors: Vec<(String, String)> =
            sqlx::query_as("SELECT scope, last_seen_at_utc FROM sync_scope_state")
                .fetch_all(&mut *conn)
                .await?;
        for (scope, at) in cursors {
            let at = datalib_time::parse_strict(&at)
                .with_context(|| format!("the cursor of {scope}: {at:?}"))?;
            sqlx::query("INSERT OR REPLACE INTO coverage (scope, lo, hi) VALUES (?, '', ?)")
                .bind(coverage_scope(&scope))
                .bind(stamp(&at))
                .execute(&mut *conn)
                .await?;
        }
        sqlx::query("DELETE FROM sync_scope_state")
            .execute(&mut *conn)
            .await?;
    }
    if has("sync_scope_config").fetch_one(&mut *conn).await? {
        sqlx::query("DELETE FROM sync_scope_config")
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// A change request's own record, or what [`Forge::fetch_one`] comes to
/// without one. `Err` when the shared retry loop gave up: no request
/// after this one will fare better, so the run ends.
pub async fn get_change_request<C>(
    client: &ForgeClient,
    url: &str,
) -> Result<std::result::Result<Value, Answer<C>>> {
    match client.get(url).await {
        Ok((v, _)) if v.is_object() => Ok(Ok(v)),
        Ok(_) => Ok(Err(Answer::Short(vec![format!(
            "{url} returned something other than an object"
        )]))),
        Err(e) if e.gave_up() => Err(e.into()),
        Err(e) if e.gone() => Ok(Err(Answer::Gone)),
        Err(e) => Ok(Err(Answer::Short(vec![e.to_string()]))),
    }
}

/// Walk a change request's whole list of one kind of child, or say why
/// it could not. An empty list from a failed request is
/// indistinguishable from "all deleted", and pruning on it would wipe
/// every comment the change request has: on an inner `Err` the caller
/// neither stores nor prunes, and reports the line. The outer `Err` is a
/// give-up of the shared retry loop, which ends the run.
pub async fn walk_children(
    client: &ForgeClient,
    url: &str,
    what: &str,
) -> Result<std::result::Result<Vec<Value>, String>> {
    match client.paginate(url).await {
        Ok(children) => Ok(Ok(children)),
        Err(e) if e.gave_up() => Err(e.into()),
        Err(e) => Ok(Err(format!("could not list its {what}: {e}"))),
    }
}

/// Delete `table`'s rows under one change request that its fresh,
/// complete listing did not name, in the transaction that stores the
/// listing. Scoped to that change request: the endpoint enumerated its
/// children and nothing else.
pub async fn prune_children(
    tx: &mut Transaction<'_, Sqlite>,
    table: &'static str,
    scope: &[(&str, &str)],
    keep: &HashSet<String>,
) -> Result<usize> {
    let gone = datalib_etl::prune::prune_scope_in_tx(tx, table, scope, keep).await?;
    if !gone.is_empty() {
        tracing::info!(
            event = "forge_children_pruned",
            table,
            scope = ?scope,
            removed = gone.len(),
            "the forge no longer lists these; deleting our copies",
        );
    }
    Ok(gone.len())
}

/// The account the store was synced as: its one `self_identity` row.
pub async fn load_self_identity(pool: &SqlitePool) -> Result<Option<Value>> {
    use sqlx::Row as _;
    let row = sqlx::query(
        "SELECT json(payload) AS payload FROM self_identity \
         WHERE payload IS NOT NULL ORDER BY id LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .context("select self_identity")?;
    let Some(row) = row else { return Ok(None) };
    let payload: Option<String> = row.try_get("payload").ok();
    Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
}

/// A loaded row's `payload` column, parsed; `None` for a row the load
/// steps over.
pub fn row_payload(row: &sqlx::sqlite::SqliteRow) -> Option<Value> {
    use sqlx::Row as _;
    let payload: String = row.try_get("payload").ok()?;
    serde_json::from_str(&payload).ok()
}

/// A payload's string field, owned. For the promoted columns of a raw row.
pub fn opt_str(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// A payload's numeric `id`, as the text a raw row keys on. `what` names
/// the payload in the error.
pub fn numeric_id(payload: &Value, what: &str) -> Result<String> {
    payload
        .get("id")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .ok_or_else(|| anyhow::anyhow!("{what} missing id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window of 1,000,000 days put the floor in the year -712, which
    /// went to the search as a date; one of `u32::MAX` days panicked in
    /// the subtraction.
    #[test]
    fn a_refresh_window_reaching_before_the_epoch_has_no_floor() {
        let now = datalib_time::parse_strict("2369-04-01T00:00:00+00:00").unwrap();
        for days in [1_000_000, u32::MAX] {
            assert!(refresh_floor(&now, days).is_none(), "{days}");
        }
        let floor = refresh_floor(&now, 30).unwrap();
        assert_eq!(floor.inner().to_rfc3339(), "2369-03-02T00:00:00+00:00");
    }

    fn listed(number: u32, updated_at: &str) -> Listed {
        Listed {
            container: "starfleet/enterprise".to_string(),
            number,
            updated_at: updated_at.to_string(),
        }
    }

    fn search(items: usize, truncated: bool, incomplete: bool) -> Search {
        Search {
            items: vec![Value::Null; items],
            truncated,
            incomplete,
        }
    }

    const NOW: &str = "2369-04-15T00:00:00Z";
    const T1: &str = "2369-04-10T00:00:00Z";
    const T2: &str = "2369-04-12T00:00:00Z";
    const T3: &str = "2369-04-14T00:00:00Z";

    /// The top gap is open above and reaches the run's now, or the
    /// newest result when the forge's clock is ahead; a gap below is
    /// searched between its ends and covered between them.
    #[test]
    fn a_whole_search_covers_the_gap_it_was_asked_for() {
        let top = Span::new(T2, NOW);
        assert_eq!(
            bounds_of(&top, NOW),
            Bounds {
                lo: Some(T2.into()),
                hi: None
            }
        );
        assert_eq!(
            covered(
                &top,
                NOW,
                &search(2, false, false),
                &[listed(3, T3), listed(2, T2)]
            ),
            Some(Span::new(T2, NOW))
        );
        let later = "2369-04-15T10:00:00Z";
        assert_eq!(
            covered(&top, NOW, &search(1, false, false), &[listed(4, later)]),
            Some(Span::new(T2, later)),
            "a result updated after the run began is looked at too"
        );
        let below = Span::new("", T2);
        assert_eq!(
            bounds_of(&below, NOW),
            Bounds {
                lo: None,
                hi: Some(T2.into())
            }
        );
        assert_eq!(
            covered(&below, NOW, &search(0, false, false), &[]),
            Some(Span::new("", T2)),
            "an empty answer looked at the whole gap"
        );
    }

    /// GitHub answers at most 1000 results, newest first: the search
    /// looked at nothing older than the oldest it gave. An incomplete
    /// answer looked at nothing it can vouch for.
    #[test]
    fn a_truncated_search_covers_only_down_to_its_oldest_result() {
        let top = Span::new("", NOW);
        assert_eq!(
            covered(
                &top,
                NOW,
                &search(2, true, false),
                &[listed(3, T3), listed(2, T2)]
            ),
            Some(Span::new(T2, NOW))
        );
        assert_eq!(
            covered(
                &top,
                NOW,
                &search(2, true, false),
                &[listed(3, ""), listed(2, "")]
            ),
            None,
            "results without a stamp say nothing about where the search stopped"
        );
        assert_eq!(
            covered(
                &top,
                NOW,
                &search(2, false, true),
                &[listed(3, T3), listed(2, T2)]
            ),
            None
        );
    }

    #[test]
    fn an_item_key_splits_back_at_its_last_sigil() {
        assert_eq!(
            split_item_key("starfleet/enterprise#1701", '#'),
            Some(("starfleet/enterprise".to_string(), 1701))
        );
        assert_eq!(
            split_item_key("starfleet/enterprise!1701", '!'),
            Some(("starfleet/enterprise".to_string(), 1701))
        );
        assert_eq!(split_item_key("starfleet/enterprise", '#'), None);
        assert_eq!(split_item_key("starfleet/enterprise#NCC", '#'), None);
    }

    /// Two listing rows, one held at its stamp and one fetched at an
    /// older one, and one never fetched: the never-fetched goes first,
    /// however it sorts, so a capped run pays for new work before
    /// refreshing old.
    #[tokio::test]
    async fn what_was_never_fetched_is_owed_first() {
        const T: &str = "pull_requests";
        let d = tempfile::tempdir().unwrap();
        let ddl = [
            "CREATE TABLE IF NOT EXISTS pull_requests (id TEXT PRIMARY KEY, payload TEXT NULL, updated_at TEXT NULL)",
            &datalib_etl::doltlite_raw::bookkeeping_ddl_for(T),
            LISTED_DDL,
        ];
        let ddl: Vec<&str> = ddl.iter().map(|s| &**s).collect();
        let pool = datalib_etl::doltlite_raw::open(&d.path().join("f.doltlite_db"), &ddl)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        for (id, at) in [("o/r#1", T2), ("o/r#2", T3), ("o/r#3", T1), ("o/r#4", T1)] {
            sqlx::query("INSERT INTO listed_change_requests (id, updated_at) VALUES (?, ?)")
                .bind(id)
                .bind(at)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        owed::hold(&mut tx, T, "o/r#1", Some(T2)).await.unwrap();
        owed::hold(&mut tx, T, "o/r#2", Some(T1)).await.unwrap();
        datalib_etl::doltlite_raw::record_object_error(&mut tx, T, "o/r#3", "HTTP 500")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let listed = listing(&pool).await.unwrap();
        assert_eq!(listed.len(), 4);
        let owed = owed::owed(&pool, T, listed).await.unwrap();
        let ordered = never_fetched_first(&pool, T, owed).await.unwrap();
        let keys: Vec<&str> = ordered.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, ["o/r#4", "o/r#3", "o/r#2"]);
        pool.close().await;
    }
}
