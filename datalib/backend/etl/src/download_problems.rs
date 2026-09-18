//! Problems a run has that are not about one record: a configured entry
//! the upstream does not have, a listing that did not come back as an
//! enumeration, a phase that failed wholesale.
//!
//! Distinct from the per-item transient failures `FetchSummary` counts as
//! `errors` and `record_object_error` pins to the record. These are about
//! the run's shape, so they are keyed by what failed rather than by a
//! record, and a run's set replaces the last one's whole: a corrected
//! config, or a listing that answers again, clears its row.
//!
//! Reporting one must not fail the run. A misspelling in a five-entry
//! list costs that entry and nothing else, the same way a config entry
//! the loader cannot use costs that entry and nothing else.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProblemReason {
    /// The upstream has nothing by that name.
    NotFound,
    /// It exists, but this credential cannot read it.
    Forbidden,
}

impl ProblemReason {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProblem {
    /// The config key that named it, e.g. `only_extract_labels`.
    pub setting: String,
    /// The value, spelled the way the config spelled it.
    pub value: String,
    pub reason: ProblemReason,
    /// What upstream said, or what the reader should do about it.
    pub detail: String,
}

impl DownloadProblem {
    pub fn not_found(setting: &str, value: &str, detail: impl Into<String>) -> Self {
        Self {
            setting: setting.to_string(),
            value: value.to_string(),
            reason: ProblemReason::NotFound,
            detail: detail.into(),
        }
    }

    pub fn forbidden(setting: &str, value: &str, detail: impl Into<String>) -> Self {
        Self {
            setting: setting.to_string(),
            value: value.to_string(),
            reason: ProblemReason::Forbidden,
            detail: detail.into(),
        }
    }
}

/// What a configured list of names resolved to.
#[derive(Debug)]
pub struct Resolution<T> {
    pub resolved: Vec<T>,
    pub problems: Vec<DownloadProblem>,
}

impl<T> Default for Resolution<T> {
    fn default() -> Self {
        Self {
            resolved: Vec::new(),
            problems: Vec::new(),
        }
    }
}

impl<T> Resolution<T> {
    /// Every configured entry missed.
    ///
    /// The caller has to decide what that means, because it depends on
    /// what an empty result does downstream. For a *filter* it is
    /// usually fatal: an empty filter means "everything", so falling
    /// through would mirror the whole account the config was narrowing.
    pub fn nothing_resolved(&self) -> bool {
        self.resolved.is_empty() && !self.problems.is_empty()
    }
}

/// Resolve a configured list against what upstream actually has,
/// keeping the entries that resolve and recording the ones that do not.
///
/// `lookup` returns `Err(detail)` for a miss, where `detail` says what
/// the reader should do — usually the list of valid names.
///
/// Never returns an error itself. One misspelling costs that entry, the
/// same way a config entry the loader cannot use costs that entry and
/// nothing else.
pub fn resolve_configured<T, F>(setting: &str, specs: &[String], mut lookup: F) -> Resolution<T>
where
    F: FnMut(&str) -> Result<T, String>,
{
    let mut out = Resolution::default();
    for spec in specs {
        match lookup(spec) {
            Ok(v) => out.resolved.push(v),
            Err(detail) => out
                .problems
                .push(DownloadProblem::not_found(setting, spec, detail)),
        }
    }
    out
}

/// One `warn!` per problem, in a shape every provider shares so a reader
/// grepping `download_problem` finds all of them — and one `problems`
/// row each in the raw store, keyed `config:<setting>:<value>`, which is
/// what reaches the screen. The rows are the whole truth every run: a
/// run's list replaces the last one's, so an entry the config no longer
/// names, or that upstream now has, is gone. Recording never fails the
/// run; a store that cannot take the rows is said and passed over.
pub async fn report(pool: &sqlx::SqlitePool, problems: &[DownloadProblem]) {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    for p in problems {
        tracing::warn!(
            event = "download_problem",
            setting = %p.setting,
            value = %p.value,
            reason = p.reason.as_str(),
            detail = %p.detail,
            "a configured entry does not exist upstream; continuing without it",
        );
    }
    let rows: Vec<(String, Outcome, Problem)> = problems
        .iter()
        .map(|p| {
            let reason = match p.reason {
                ProblemReason::NotFound => Reason::NotFound,
                ProblemReason::Forbidden => Reason::Forbidden,
            };
            (
                format!("{CONFIG_PREFIX}{}:{}", p.setting, p.value),
                Outcome::Dropped,
                Problem::field(&p.setting, reason, &p.detail).severity(Severity::Warning),
            )
        })
        .collect();
    if let Err(e) = replace_prefixed(pool, &[CONFIG_PREFIX], &rows).await {
        tracing::warn!(
            error = %format!("{e:#}"),
            "download_problem: could not record the configured entries that did not resolve; \
             the Manage row will not show them"
        );
    }
}

/// The sweep key's prefix of a configured entry's row: every row
/// [`report`] writes, and only those, so a run's report can replace the
/// last one's whole.
const CONFIG_PREFIX: &str = "config:";

/// What a run could not do as a whole. Each variant is a sweep-key
/// prefix, so [`report_run`] can replace the last run's rows of every
/// kind at once.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunProblemKind {
    /// A listing that did not come back as an enumeration — not an
    /// array, nothing at all, or a page walk that stopped on an error —
    /// so absence from it means nothing and the stored rows were left
    /// alone. What is stored is stale until it lists again.
    Listing,
    /// A whole phase of the run failed before it did its work; the
    /// other phases ran.
    Phase,
}

impl RunProblemKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    fn key_prefix(self) -> String {
        format!("{}:", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProblem {
    pub kind: RunProblemKind,
    /// The listing or phase, as the provider names it: `workouts`,
    /// `devices`, `weight`.
    pub name: String,
    /// What went wrong, in the words of the error.
    pub detail: String,
}

impl RunProblem {
    pub fn listing(name: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: RunProblemKind::Listing,
            name: name.to_string(),
            detail: detail.into(),
        }
    }

    pub fn phase(name: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: RunProblemKind::Phase,
            name: name.to_string(),
            detail: detail.into(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}{}", self.kind.key_prefix(), self.name)
    }
}

/// As [`report`], for what a run could not do as a whole: one `warn!`
/// per problem under `event = "run_problem"`, and one `problems` row
/// each keyed `listing:<name>` / `phase:<name>`. Every row of both kinds
/// is replaced each run — call it with an empty slice on a clean run so
/// the last run's rows go. An error, because the reader has nothing
/// current for that listing or phase; two problems on one key keep the
/// first's detail.
pub async fn report_run(pool: &sqlx::SqlitePool, problems: &[RunProblem]) {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    for p in problems {
        tracing::warn!(
            event = "run_problem",
            kind = p.kind.as_str(),
            name = %p.name,
            detail = %p.detail,
            "part of the run did not happen; what it would have written was left as it was",
        );
    }
    let mut seen = std::collections::HashSet::new();
    let rows: Vec<(String, Outcome, Problem)> = problems
        .iter()
        .filter(|p| seen.insert(p.key()))
        .map(|p| {
            (
                p.key(),
                Outcome::Dropped,
                Problem::record(Reason::FetchFailed, &p.detail).severity(Severity::Error),
            )
        })
        .collect();
    let prefixes: Vec<String> = RunProblemKind::VARIANTS
        .iter()
        .map(|k| k.key_prefix())
        .collect();
    let prefixes: Vec<&str> = prefixes.iter().map(String::as_str).collect();
    if let Err(e) = replace_prefixed(pool, &prefixes, &rows).await {
        tracing::warn!(
            error = %format!("{e:#}"),
            "run_problem: could not record what the run could not do; \
             the Manage row will not show it"
        );
    }
}

/// Delete every entity-scoped row whose key starts with one of
/// `prefixes`, then write `rows`, in one transaction. A key that was
/// there before keeps its `first_seen_at_utc`, so the screen can say
/// how long a listing has been failing.
async fn replace_prefixed(
    pool: &sqlx::SqlitePool,
    prefixes: &[&str],
    rows: &[(String, datalib_problems::Outcome, datalib_problems::Problem)],
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use datalib_problems::{ProblemRow, Scope, ScopeKind, Stage};
    use datalib_table::BulkUpsertable as _;
    let mut tx = pool.begin().await.context("begin")?;
    let mut first_seen: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for prefix in prefixes {
        // `INSTR(x, ?) = 1` rather than `LIKE`: `_` in a value is a
        // wildcard to LIKE.
        let earlier: Vec<(String, String)> = sqlx::query_as(
            "SELECT scope_key, first_seen_at_utc FROM problems \
             WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1",
        )
        .bind(ScopeKind::Entity.as_str())
        .bind(prefix)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("read the last run's {prefix} problems"))?;
        first_seen.extend(earlier);
        sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1")
            .bind(ScopeKind::Entity.as_str())
            .bind(prefix)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("clear the last run's {prefix} problems"))?;
    }
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    for (key, outcome, problem) in rows {
        let row = ProblemRow {
            first_seen_at_utc: first_seen.get(key).cloned().unwrap_or_else(|| now.clone()),
            last_seen_at_utc: now.clone(),
            tz_offset: Some(tz_offset.clone()),
            ..ProblemRow::new(
                "",
                Stage::Fetch,
                Scope::Entity(key),
                None,
                *outcome,
                problem.clone(),
                None,
            )
        };
        let sql = crate::bulk::insert_sql::<ProblemRow>();
        // Audited: `sql` is built from `ProblemRow`'s associated consts,
        // never from row data; all values bound.
        row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("record {key}"))?;
    }
    tx.commit().await.context("commit")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's report replaces the last one's: an entry the config no
    /// longer names, or that upstream now has, is gone the next run.
    #[tokio::test]
    async fn a_reports_rows_are_the_whole_truth_for_that_run() {
        let d = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(&d.path().join("p.doltlite_db"), &[])
            .await
            .unwrap();
        let keys = |pool: &sqlx::SqlitePool| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT scope_key FROM problems ORDER BY scope_key")
                    .fetch_all(&pool)
                    .await
                    .unwrap()
            }
        };
        report(
            &pool,
            &[
                DownloadProblem::not_found("only_labels", "Recieved", "no such label"),
                DownloadProblem::forbidden("channels", "C9", "private"),
            ],
        )
        .await;
        assert_eq!(
            keys(&pool).await,
            ["config:channels:C9", "config:only_labels:Recieved"]
        );
        report(
            &pool,
            &[DownloadProblem::not_found(
                "only_labels",
                "Recieved",
                "no such label",
            )],
        )
        .await;
        assert_eq!(keys(&pool).await, ["config:only_labels:Recieved"]);
        report(&pool, &[]).await;
        assert!(keys(&pool).await.is_empty());
        pool.close().await;
    }

    /// A run's listing and phase rows replace the last run's, both kinds
    /// at once, and leave the configured-entry rows alone. A key seen
    /// again keeps its `first_seen_at_utc`.
    #[tokio::test]
    async fn run_problems_replace_their_own_kinds_and_keep_first_seen() {
        let d = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(&d.path().join("p.doltlite_db"), &[])
            .await
            .unwrap();
        let rows = |pool: &sqlx::SqlitePool| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (String, String, String, String)>(
                    "SELECT scope_key, severity, reason, first_seen_at_utc FROM problems \
                     ORDER BY scope_key",
                )
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        report(
            &pool,
            &[DownloadProblem::not_found(
                "only_labels",
                "x",
                "no such label",
            )],
        )
        .await;
        report_run(
            &pool,
            &[
                RunProblem::listing("workouts", "HTTP 500"),
                RunProblem::phase("weight", "cursor"),
                RunProblem::listing("workouts", "a second failure on the same key"),
            ],
        )
        .await;
        let first = rows(&pool).await;
        assert_eq!(
            first
                .iter()
                .map(|r| (r.0.as_str(), r.1.as_str(), r.2.as_str()))
                .collect::<Vec<_>>(),
            [
                ("config:only_labels:x", "warning", "not_found"),
                ("listing:workouts", "error", "fetch_failed"),
                ("phase:weight", "error", "fetch_failed"),
            ]
        );
        let workouts_first_seen = first[1].3.clone();

        report_run(&pool, &[RunProblem::listing("workouts", "HTTP 502")]).await;
        let second = rows(&pool).await;
        assert_eq!(
            second.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["config:only_labels:x", "listing:workouts"],
            "the phase row went; the config row is not this report's to touch"
        );
        assert_eq!(
            second[1].3, workouts_first_seen,
            "a key seen again keeps when it was first seen"
        );

        report_run(&pool, &[]).await;
        assert_eq!(
            rows(&pool)
                .await
                .iter()
                .map(|r| r.0.as_str())
                .collect::<Vec<_>>(),
            ["config:only_labels:x"]
        );
        pool.close().await;
    }

    /// strum and serde are independent derives producing independent
    /// strings; the agreement is a real check, not a tautology.
    #[test]
    fn strum_and_serde_agree_on_every_variant() {
        for r in ProblemReason::VARIANTS {
            let serde = serde_json::to_string(r).unwrap();
            let serde = serde.trim_matches('"');
            assert_eq!(serde, r.as_str(), "{r:?}");
            assert_eq!(ProblemReason::parse(serde), Some(*r));
        }
        for k in RunProblemKind::VARIANTS {
            let serde = serde_json::to_string(k).unwrap();
            let serde = serde.trim_matches('"');
            assert_eq!(serde, k.as_str(), "{k:?}");
            assert_eq!(RunProblemKind::parse(serde), Some(*k));
        }
    }

    #[test]
    fn keeps_the_hits_and_records_the_misses() {
        let specs = vec!["a".to_string(), "nope".to_string(), "b".to_string()];
        let out = resolve_configured("things", &specs, |s| match s {
            "a" | "b" => Ok(s.to_uppercase()),
            _ => Err("known: a, b".to_string()),
        });
        assert_eq!(out.resolved, vec!["A", "B"]);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].value, "nope");
        assert_eq!(out.problems[0].setting, "things");
        assert!(!out.nothing_resolved());
    }

    /// The distinction the callers branch on. An empty configured list
    /// resolves to nothing and that is fine — it means "no filter". A
    /// list where every entry missed also resolves to nothing, and for a
    /// filter that would silently widen the scope to everything.
    #[test]
    fn tells_an_empty_config_apart_from_a_wholly_unresolvable_one() {
        let none = resolve_configured("things", &[], |s: &str| Ok::<_, String>(s.to_string()));
        assert!(
            !none.nothing_resolved(),
            "no filter configured is not a miss"
        );

        let specs = vec!["nope".to_string()];
        let all_missed = resolve_configured("things", &specs, |_| {
            Err::<String, _>("known: a".to_string())
        });
        assert!(all_missed.nothing_resolved());
        assert!(all_missed.resolved.is_empty());
    }

    #[test]
    fn an_unknown_spelling_is_none_rather_than_a_guess() {
        assert_eq!(ProblemReason::parse("teleported"), None);
    }
}
