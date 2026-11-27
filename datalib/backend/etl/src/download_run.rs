//! [`DownloadRun`]: a download's row in `sync_runs`, with what it changed.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::doltlite_raw::{finish_run, has_dolt_extensions, start_run};
use crate::run_problems::RunProblems;
use crate::scope_state;
use datalib_sql::{sql, sql_id, Sql};

#[derive(Debug, Default, Clone, Serialize)]
pub struct RowDelta {
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
}

impl RowDelta {
    fn is_empty(&self) -> bool {
        self.added == 0 && self.modified == 0 && self.removed == 0
    }
}

/// A download's row in `sync_runs`, opened by [`start`](Self::start)
/// inside the run's problem collector ([`crate::run_problems::collecting`]).
/// The collector closes it after it has written the run's problems, so
/// the `deltas` it records count every row the run wrote, problems
/// included, however the seals fell; its status is what the collector's
/// body returned.
pub struct DownloadRun {
    found: RunProblems,
    run_id: i64,
}

impl DownloadRun {
    pub async fn start(found: &RunProblems, pool: &SqlitePool, config: &Value) -> Result<Self> {
        let head_at_start = crate::doltlite_raw::head_commit(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %format!("{e:#}"), "head read at run start failed");
                None
            });
        let run_id = start_run(pool, config).await?;
        let cursors_before = scope_state::snapshot(pool).await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "scope_state snapshot at start failed");
            HashMap::new()
        });
        found.open_run(OpenRun {
            run_id,
            started: std::time::Instant::now(),
            head_at_start,
            cursors_before,
            summary: Value::Object(Default::default()),
        })?;
        Ok(Self {
            found: found.clone(),
            run_id,
        })
    }

    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    /// What the row's summary says. A run that never gets here, because
    /// its body returned early, is closed with only the run's own fields.
    pub fn summarize<S: Serialize>(self, summary: &S) {
        self.found
            .summarize_run(serde_json::to_value(summary).unwrap_or(Value::Null));
    }
}

pub(crate) struct OpenRun {
    run_id: i64,
    started: std::time::Instant,
    /// HEAD before this run wrote anything, and the `from_ref` the
    /// `deltas` summary is measured against. It has to be the run's own
    /// starting point rather than the store's last commit: a provider that
    /// commits mid-run — so render can start on the batches that have
    /// landed — leaves those batches clean, and "since the last commit"
    /// then reports only the final one.
    head_at_start: Option<String>,
    /// `sync_scope_state` snapshot taken right after `start_run`;
    /// diffed against another snapshot at close so the resulting
    /// `cursors` summary records every scope that moved during this run.
    cursors_before: HashMap<String, String>,
    pub(crate) summary: Value,
}

impl OpenRun {
    pub(crate) async fn close<T>(self, pool: &SqlitePool, ended_early: bool, result: &Result<T>) {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let status = final_status(result.is_ok(), ended_early);
        let deltas = compute_deltas(pool, self.head_at_start.as_deref()).await;
        let cursors_after = scope_state::snapshot(pool).await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "scope_state snapshot at finish failed");
            HashMap::new()
        });
        let cursor_moves = scope_state::diff(self.cursors_before, cursors_after);
        let mut summary_json = self.summary;
        if let Value::Object(map) = &mut summary_json {
            map.insert("elapsed_ms".into(), Value::from(elapsed_ms));
            if let Some(d) = deltas {
                map.insert(
                    "deltas".into(),
                    serde_json::to_value(d).unwrap_or(Value::Null),
                );
            }
            let (listed, counted) = scope_state::summarize(cursor_moves);
            if !listed.is_empty() {
                map.insert(
                    "cursors".into(),
                    serde_json::to_value(&listed).unwrap_or(Value::Null),
                );
            }
            if !counted.is_empty() {
                map.insert(
                    "cursors_by_kind".into(),
                    serde_json::to_value(&counted).unwrap_or(Value::Null),
                );
            }
            if let Err(e) = result {
                map.insert("error".into(), Value::from(format!("{e:#}")));
            }
        }
        if let Err(e) = finish_run(pool, self.run_id, status, &summary_json).await {
            tracing::warn!(
                run_id = self.run_id,
                error = %format!("{e:#}"),
                "download_run: finish_run bookkeeping failed"
            );
        }
    }
}

/// `ok` is a claim a reader relies on: this run read everything it set
/// out to, so its start is how recent the mirror is.
fn final_status(succeeded: bool, ended_early: bool) -> &'static str {
    match (succeeded, ended_early) {
        (false, _) => "error",
        (true, true) => "partial",
        (true, false) => "ok",
    }
}

/// Per-table row counts for everything this run changed, diffing every
/// table in the store from `head_at_start` to the working set. Returns
/// `None` against stock libsqlite3 (no dolt extensions) or when the run
/// began with no HEAD to measure from; `Some({})` when nothing moved.
/// Individual table queries that fail are logged and skipped — we never
/// want a single bad virtual table read to drop the rest of the summary.
///
/// Every table, not just the ones `dolt_status` reports dirty: a table a
/// streaming provider finished and committed mid-run is clean by the time
/// we get here, and asking only about dirty tables loses it entirely.
async fn compute_deltas(
    pool: &SqlitePool,
    head_at_start: Option<&str>,
) -> Option<BTreeMap<String, RowDelta>> {
    if !has_dolt_extensions(pool).await {
        return None;
    }
    let Some(from_ref) = head_at_start else {
        tracing::warn!("no HEAD at run start; per-table deltas unavailable for this run");
        return None;
    };
    let tables: Vec<String> = match sql!(
        "etl.download_run.list_tables_for_deltas",
        "SELECT name FROM sqlite_master WHERE type = 'table'"
    )
    .query_scalar()
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "table list read failed");
            return None;
        }
    };
    let mut out: BTreeMap<String, RowDelta> = BTreeMap::new();
    for table in tables {
        // Guard against any table name that wouldn't be a safe
        // identifier in the dynamic `dolt_diff_<table>` query. Dolt's
        // own naming for these virtual tables matches the underlying
        // table's identifier rules — so an unsafe name here would mean
        // an unsafe table name made it past our DDL, which is a
        // separate bug, but we still skip rather than risk an
        // injection-flavored failure mode.
        if !is_safe_identifier(&table) {
            tracing::warn!(table = %table, "skipping delta for unsafe-identifier table name");
            continue;
        }
        let sql = Sql::dynamic(
            sql_id!("etl.download_run.count_table_diff_by_type"),
            format!(
                "SELECT diff_type, COUNT(*) FROM dolt_diff_{table} \
             WHERE from_ref = ?1 AND to_ref = 'WORKING' GROUP BY diff_type"
            ),
        );
        // Audited: `table` is interpolated into the `dolt_diff_<table>` vtab name,
        // and the `is_safe_identifier` guard above rejects anything that is not a
        // plain identifier before we get here. `from_ref` is bound.
        let rows: Vec<(String, i64)> = match sql.query_as().bind(from_ref).fetch_all(pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(table = %table, error = %format!("{e:#}"),
                    "dolt_diff_<table> read failed; delta dropped");
                continue;
            }
        };
        let mut d = RowDelta::default();
        for (kind, n) in rows {
            let n = n.max(0) as u64;
            match kind.as_str() {
                "added" => d.added = n,
                "modified" => d.modified = n,
                "removed" => d.removed = n,
                _ => {}
            }
        }
        if d.is_empty() {
            continue;
        }
        out.insert(table, d);
    }
    Some(out)
}

fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    // Through the canonical `open()` on a tempdir-backed file, so the test
    // takes the same path a download does.
    async fn fresh_pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extract_run_test.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[]).await.unwrap();
        (pool, dir)
    }

    async fn summary_deltas(pool: &SqlitePool, run_id: i64) -> serde_json::Map<String, Value> {
        let row: (String,) = test_sql!("SELECT summary FROM sync_runs WHERE run_id = ?")
            .query_as()
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let s: Value = serde_json::from_str(&row.0).unwrap();
        s.get("deltas")
            .and_then(|v| v.as_object())
            .expect("deltas object present in summary")
            .clone()
    }

    #[derive(Serialize)]
    struct DummySummary {
        new: usize,
        skipped: usize,
    }

    /// The newest row in `sync_runs`: its id, status and summary.
    async fn last_run(pool: &SqlitePool) -> (i64, String, Value) {
        let row: (i64, String, String) =
            test_sql!("SELECT run_id, status, summary FROM sync_runs ORDER BY run_id DESC LIMIT 1")
                .query_as()
                .fetch_one(pool)
                .await
                .unwrap();
        (row.0, row.1, serde_json::from_str(&row.2).unwrap())
    }

    /// #1606: `ok` promises the run read everything it set out to, and a
    /// run that ended early used to write it too, so the newest `ok` row
    /// said nothing about how recent the mirror was.
    #[tokio::test]
    async fn a_run_that_ended_early_is_partial_however_it_ended() {
        let (pool, _dir) = fresh_pool().await;
        let status_after = |end: fn(&crate::run_problems::RunProblems, &crate::stop::StopFlag)| {
            let pool = pool.clone();
            async move {
                let stop = crate::stop::StopFlag::new();
                let (store, ran) = (pool.clone(), stop.clone());
                crate::run_problems::collecting(&pool, &stop, |found| async move {
                    let run = DownloadRun::start(&found, &store, &json!({})).await?;
                    end(&found, &ran);
                    run.summarize(&DummySummary { new: 0, skipped: 0 });
                    Ok(())
                })
                .await
                .unwrap();
                last_run(&pool).await.1
            }
        };
        assert_eq!(status_after(|_, _| {}).await, "ok");
        assert_eq!(status_after(|_, stop| stop.request()).await, "partial");
        assert_eq!(status_after(|found, _| found.cut_short()).await, "partial");
        assert_eq!(
            status_after(|found, _| found.gave_up("stopped answering")).await,
            "partial"
        );
        assert_eq!(
            status_after(|_, stop| stop.give_up("stopped answering")).await,
            "partial"
        );
        assert_eq!(final_status(false, true), "error");
    }

    #[tokio::test]
    async fn ok_path_writes_ok_status_with_elapsed_and_summary_fields() {
        let (pool, _dir) = fresh_pool().await;
        let store = &pool;
        let stop = crate::stop::StopFlag::new();
        crate::run_problems::collecting(&pool, &stop, |found| async move {
            let run = DownloadRun::start(&found, store, &json!({"k": "v"})).await?;
            run.summarize(&DummySummary { new: 7, skipped: 3 });
            Ok(())
        })
        .await
        .unwrap();

        let (_, status, s) = last_run(&pool).await;
        assert_eq!(status, "ok");
        assert_eq!(s["new"], 7);
        assert_eq!(s["skipped"], 3);
        // elapsed_ms always merged in.
        assert!(s["elapsed_ms"].is_number());
        // No error field on the ok path.
        assert!(s.get("error").is_none());
    }

    #[tokio::test]
    async fn deltas_reflect_uncommitted_writes_when_dolt_extensions_present() {
        // Doltlite-only test: against stock libsqlite3 there are no
        // dolt_status / dolt_diff_<table> virtual tables, so
        // `deltas` would simply be absent (asserted in the ok-path
        // test above). Under bazel (where libsqlite3-sys is linked
        // against doltlite) it's the real thing.
        let (pool, _dir) = fresh_pool().await;
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        Sql::ddl(test_sql_id!(), crate::doltlite_raw::SYNC_SCOPE_STATE_DDL)
            .query()
            .execute(&pool)
            .await
            .unwrap();
        // Establish a baseline commit so subsequent inserts show up
        // as a clean "added since HEAD" delta — without this, dolt's
        // view of "what changed" includes the schema-creation step
        // itself and the result is less predictable for assertion.
        crate::doltlite_raw::commit_run(&pool, "baseline")
            .await
            .unwrap();

        let store = &pool;
        let stop = crate::stop::StopFlag::new();
        crate::run_problems::collecting(&pool, &stop, |found| async move {
            let run = DownloadRun::start(&found, store, &json!({"k": "v"})).await?;
            test_sql!("INSERT INTO sync_scope_state (scope, last_seen_at_utc) VALUES (?, ?)")
                .query()
                .bind("test_scope")
                .bind("2026-01-01T00:00:00Z")
                .execute(store)
                .await?;
            run.summarize(&DummySummary { new: 0, skipped: 0 });
            Ok(())
        })
        .await
        .unwrap();

        let run_id = last_run(&pool).await.0;
        let deltas = summary_deltas(&pool, run_id).await;
        // sync_scope_state: 1 row inserted since baseline.
        let scope_state = deltas
            .get("sync_scope_state")
            .unwrap_or_else(|| panic!("sync_scope_state missing in deltas: {deltas:?}"));
        assert_eq!(
            scope_state["added"], 1,
            "sync_scope_state should show 1 added row; got {scope_state}"
        );
        // sync_runs: DownloadRun::start added one row, its close UPDATEd
        // it. Net for dolt is one added row.
        let sync_runs = deltas
            .get("sync_runs")
            .unwrap_or_else(|| panic!("sync_runs missing in deltas: {deltas:?}"));
        assert_eq!(
            sync_runs["added"], 1,
            "sync_runs should show 1 added row; got {sync_runs}"
        );
    }

    /// A run killed before its row closed left it `running` for good, so
    /// a reader could not tell it from one in progress. A store has one
    /// writer, so the next run to start knows no other is running.
    #[tokio::test]
    async fn a_run_left_running_is_marked_interrupted_when_the_next_starts() {
        let (pool, _dir) = fresh_pool().await;
        // No collector closes a run opened on these, as none closes a
        // killed one.
        let killed = DownloadRun::start(&RunProblems::unwritten(), &pool, &json!({}))
            .await
            .unwrap()
            .run_id();
        let next = DownloadRun::start(&RunProblems::unwritten(), &pool, &json!({}))
            .await
            .unwrap()
            .run_id();
        let status = |id: i64| {
            test_sql!("SELECT status FROM sync_runs WHERE run_id = ?")
                .query_scalar::<String>()
                .bind(id)
                .fetch_one(&pool)
        };
        assert_eq!(status(killed).await.unwrap(), "interrupted");
        assert_eq!(status(next).await.unwrap(), "running");
    }

    #[tokio::test]
    async fn err_path_writes_error_status_and_keeps_partial_summary() {
        let (pool, _dir) = fresh_pool().await;
        let store = &pool;
        let stop = crate::stop::StopFlag::new();
        let ran = crate::run_problems::collecting(&pool, &stop, |found| async move {
            let run = DownloadRun::start(&found, store, &json!({"k": "v"})).await?;
            // Even on error, the partial summary the provider populated
            // before the failure is preserved — that's what makes
            // post-mortem analysis possible.
            run.summarize(&DummySummary { new: 4, skipped: 0 });
            Err::<(), _>(anyhow::anyhow!("upstream 503"))
        })
        .await;
        assert!(ran.is_err());

        let (_, status, s) = last_run(&pool).await;
        assert_eq!(status, "error");
        assert_eq!(s["new"], 4);
        assert_eq!(s["error"].as_str().unwrap(), "upstream 503");
    }

    /// A body that returned early with `?` never summarized its run, and
    /// its row is closed anyway, rather than left `running` until the next
    /// run marks it interrupted.
    #[tokio::test]
    async fn a_run_whose_body_bailed_is_closed_as_an_error() {
        let (pool, _dir) = fresh_pool().await;
        let store = &pool;
        let stop = crate::stop::StopFlag::new();
        let ran = crate::run_problems::collecting(&pool, &stop, |found| async move {
            let _run = DownloadRun::start(&found, store, &json!({})).await?;
            anyhow::bail!("the account listing failed");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await;
        assert!(ran.is_err());

        let (_, status, s) = last_run(&pool).await;
        assert_eq!(status, "error");
        assert_eq!(s["error"], "the account listing failed");
        assert!(s["elapsed_ms"].is_number());
    }

    #[tokio::test]
    async fn new_table_first_run_reports_added_rows_not_dropped_delta() {
        // A table created during a run and not yet committed must still
        // report its rows as added, not drop its delta with a warning, on
        // every provider's first sync.
        const NEW_TABLE_DDL: &str =
            "CREATE TABLE IF NOT EXISTS discussions (id TEXT PRIMARY KEY, payload TEXT)";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_table_first_run.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[NEW_TABLE_DDL])
            .await
            .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            // Stock libsqlite3 has no dolt_* virtual tables; nothing to test.
            return;
        }
        // Simulate a first-run download: insert rows into the just-created
        // table WITHOUT an intervening commit.
        for i in 0..3 {
            test_sql!("INSERT INTO discussions (id, payload) VALUES (?, ?)")
                .query()
                .bind(format!("d{i}"))
                .bind("{}")
                .execute(&pool)
                .await
                .unwrap();
        }
        let head = crate::doltlite_raw::head_commit(&pool).await.unwrap();
        let deltas = compute_deltas(&pool, head.as_deref())
            .await
            .expect("doltlite extensions present => Some(deltas)");
        let d = deltas.get("discussions").unwrap_or_else(|| {
            panic!("discussions delta was dropped instead of reported as added; got {deltas:?}")
        });
        assert_eq!(
            d.added, 3,
            "all 3 rows of the newly-created table should count as added; got {d:?}"
        );
    }

    /// Guards the streaming-download regression: a provider that commits
    /// part-way through a run — so the render step can start on what has
    /// landed — used to report only the rows written after that commit,
    /// because the deltas were measured from the store's last commit
    /// rather than from where the run began.
    #[tokio::test]
    async fn deltas_span_a_mid_run_commit() {
        const DDL: &str = "CREATE TABLE IF NOT EXISTS notes (id TEXT PRIMARY KEY, body TEXT)";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mid_run_commit.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[DDL]).await.unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }

        let store = &pool;
        let stop = crate::stop::StopFlag::new();
        crate::run_problems::collecting(&pool, &stop, |found| async move {
            let run = DownloadRun::start(&found, store, &json!({})).await?;
            for i in 0..4 {
                test_sql!("INSERT INTO notes (id, body) VALUES (?, ?)")
                    .query()
                    .bind(format!("first-{i}"))
                    .bind("{}")
                    .execute(store)
                    .await?;
            }
            // The batch handoff that makes this a streaming download.
            crate::doltlite_raw::commit_run(store, "batch 1").await?;
            for i in 0..3 {
                test_sql!("INSERT INTO notes (id, body) VALUES (?, ?)")
                    .query()
                    .bind(format!("second-{i}"))
                    .bind("{}")
                    .execute(store)
                    .await?;
            }
            run.summarize(&DummySummary { new: 7, skipped: 0 });
            Ok(())
        })
        .await
        .unwrap();

        let run_id = last_run(&pool).await.0;
        let deltas = summary_deltas(&pool, run_id).await;
        let notes = deltas
            .get("notes")
            .unwrap_or_else(|| panic!("notes missing in deltas: {deltas:?}"));
        assert_eq!(
            notes["added"], 7,
            "both batches should count, not just the one after the mid-run commit; got {notes}"
        );
    }

    /// A seal writes the problems a run has found so far, and the rest
    /// are written when the run ends. The deltas were counted before that
    /// last write, so a run's problems were in them only when a seal
    /// landed after they were found: one bake's golden named `problems`
    /// and the next did not. Counted after it, they are there either way.
    #[tokio::test]
    async fn a_runs_problems_are_in_its_deltas_however_the_seals_fell() {
        let (pool, _dir) = fresh_pool().await;
        assert!(
            crate::doltlite_raw::has_dolt_extensions(&pool).await,
            "this test measures deltas, which need the dolt extensions"
        );
        for sealed_after_the_problem in [true, false] {
            let store = &pool;
            let stop = crate::stop::StopFlag::new();
            crate::run_problems::collecting(&pool, &stop, |found| async move {
                let run = DownloadRun::start(&found, store, &json!({})).await?;
                found.listing(
                    &format!("channels sealed={sealed_after_the_problem}"),
                    "HTTP 500",
                );
                if sealed_after_the_problem {
                    found
                        .write_or_say(store, crate::run_problems::Ran::PartWay)
                        .await;
                    crate::doltlite_raw::commit_run(store, "a seal").await?;
                }
                run.summarize(&DummySummary { new: 0, skipped: 0 });
                Ok(())
            })
            .await
            .unwrap();
            let run_id = last_run(&pool).await.0;
            let deltas = summary_deltas(&pool, run_id).await;
            assert_eq!(
                deltas.get("problems").map(|d| &d["added"]),
                Some(&json!(1)),
                "sealed after the problem: {sealed_after_the_problem}; got {deltas:?}"
            );
            crate::doltlite_raw::commit_run(&pool, "between runs")
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn unchanged_tables_stay_out_of_the_deltas() {
        // Every table is diffed now, so a table nobody touched has to be
        // dropped explicitly or it pads every summary with zeroes.
        const DDL: &str = "CREATE TABLE IF NOT EXISTS quiet (id TEXT PRIMARY KEY)";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quiet_table.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[DDL]).await.unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        let head = crate::doltlite_raw::head_commit(&pool).await.unwrap();
        let deltas = compute_deltas(&pool, head.as_deref())
            .await
            .expect("doltlite extensions present => Some(deltas)");
        assert!(
            !deltas.contains_key("quiet"),
            "an untouched table should not appear at all; got {deltas:?}"
        );
    }
}
