//! Problem rows a person muted: "I have seen this, stop telling me". The
//! store's shape, its reads and writes over a pool someone else opened,
//! and the Manage counts: the grid index's rows less the muted ones, with
//! the problems grid's query for exactly those rows. Opening the
//! store to write it is the `unified_document_index` applet's
//! (`applets/src/unified_document_index/mutes.rs`), so this crate links
//! no ingest machinery and `datalib-http` can read the store through it.
//! The design: `README.md` beside this file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl_macros::PortableTable;
use datalib_problems::{CountedBy, ScopeKind, Severity, Stage};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use strum::{EnumString, IntoStaticStr, VariantArray};

pub const APP_DIR: &str = "datalib_problem_mutes";
pub const STORE_FILE: &str = "mutes.doltlite_db";

pub fn store_path(data_root: &Path) -> PathBuf {
    datalib_runtime::layout::curated_dir(data_root)
        .join(APP_DIR)
        .join(STORE_FILE)
}

/// Who muted a row: a person, or an agent on their behalf, which names
/// itself.
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
pub enum MutedHow {
    Manual,
    Agent,
}

impl MutedHow {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

pub mod schema {
    use super::*;

    /// One muted problem, by the id `datalib_id::problem_id` mints from
    /// what names it. A problem that changes is a new id, and so unmuted.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, PortableTable)]
    #[portable_table(table = "mutes", primary_key = "problem_uuid")]
    pub struct MuteRow {
        #[col(sql = "TEXT")]
        pub problem_uuid: String,
        #[col(sql = "TEXT", enum)]
        pub muted_how: MutedHow,
        /// The agent that muted it, for a row a person did not.
        #[col(sql = "TEXT")]
        pub agent: Option<String>,
        #[col(sql = "TEXT")]
        pub muted_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: Option<String>,
    }
}

pub use schema::MuteRow;

pub const DDL: &[&str] = &[schema::DDL[0].1];

/// Every mute, by problem id. A store whose table is not there yet holds
/// none.
pub async fn read_all(pool: &SqlitePool) -> Result<HashMap<String, MuteRow>> {
    let rows = match sqlx::query(
        "SELECT problem_uuid, muted_how, agent, muted_at_utc, tz_offset FROM mutes",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) if datalib_pin::is_missing_table(&e, "mutes") => return Ok(HashMap::new()),
        Err(e) => return Err(e).context("read the mutes"),
    };
    let mut out = HashMap::new();
    for r in rows {
        let how: String = r.try_get("muted_how")?;
        let row = MuteRow {
            problem_uuid: r.try_get("problem_uuid")?,
            muted_how: MutedHow::parse(&how)
                .with_context(|| format!("mutes.muted_how: unknown spelling {how:?}"))?,
            agent: r.try_get("agent")?,
            muted_at_utc: r.try_get("muted_at_utc")?,
            tz_offset: r.try_get("tz_offset")?,
        };
        out.insert(row.problem_uuid.clone(), row);
    }
    Ok(out)
}

/// Mutes `ids`, keeping the stamp of any already muted: muting again
/// says nothing new. Writes on the pool's branch; the caller commits.
pub async fn mute(
    pool: &SqlitePool,
    ids: &[String],
    how: MutedHow,
    agent: Option<&str>,
    now: &datalib_time::StoredStamp,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO mutes (problem_uuid, muted_how, agent, muted_at_utc, tz_offset) \
         SELECT value, ?, ?, ?, ? FROM json_each(?) WHERE true \
         ON CONFLICT (problem_uuid) DO NOTHING",
    )
    .bind(how.as_str())
    .bind(agent)
    .bind(&now.utc)
    .bind(now.tz_offset.as_deref())
    .bind(serde_json::to_string(ids)?)
    .execute(pool)
    .await
    .context("mute")?;
    Ok(())
}

/// Unmutes `ids`; one not muted is left alone. The caller commits.
pub async fn unmute(pool: &SqlitePool, ids: &[String]) -> Result<()> {
    sqlx::query("DELETE FROM mutes WHERE problem_uuid IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(ids)?)
        .execute(pool)
        .await
        .context("unmute")?;
    Ok(())
}

/// Every muted id as the store last published it, for a process that
/// does not write it: read-only, at `main`'s head. No store, or nothing
/// published yet, is no mutes.
pub async fn read_published(data_root: &Path) -> Result<HashMap<String, MuteRow>> {
    let path = store_path(data_root);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let head = {
        let pool = datalib_pin::open_reader(&path).await?;
        let head = datalib_pin::head(&pool).await;
        pool.close().await;
        head?
    };
    let Some(pin) = head else {
        return Ok(HashMap::new());
    };
    let pool = datalib_pin::open_at(&path, &pin).await?;
    let mutes = read_all(&pool).await;
    pool.close().await;
    mutes
}

/// Errors and warnings, as a count is kept. An `info` row is in no count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub errors: i64,
    pub warnings: i64,
}

impl Tally {
    fn add(&mut self, severity: Severity, n: i64) {
        match severity {
            Severity::Error => self.errors += n,
            Severity::Warning => self.warnings += n,
            Severity::Info => {}
        }
    }

    pub fn is_empty(self) -> bool {
        self == Tally::default()
    }
}

/// Whose count a row is in: a source's download or render, or
/// `(None, Index)` for the index's own rows, which the index's row counts
/// whatever source they are about.
pub type Counter = (Option<String>, CountedBy);

pub fn counter_of(source_id: &str, by: CountedBy) -> Counter {
    match by {
        CountedBy::Index => (None, CountedBy::Index),
        by => (Some(source_id.to_string()), by),
    }
}

/// One counter's rows in the index: those a person has not muted, which
/// are the number shown, and those they have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counted {
    pub shown: Tally,
    pub muted: Tally,
}

/// The index's error and warning rows, counted by [`Counter`], each
/// either muted or not by its id. The one place a Manage count comes
/// from, so it is the rows [`query`] opens and nothing else.
pub async fn count_in_index(
    index: &SqlitePool,
    muted: &[String],
) -> Result<HashMap<Counter, Counted>> {
    let rows = match sqlx::query(
        "SELECT source_id, stage, scope_kind, \
           CASE WHEN scope_key = ? THEN scope_key ELSE '' END AS own_key, severity, \
           problem_uuid IN (SELECT value FROM json_each(?)) AS muted, COUNT(*) AS n \
         FROM problems WHERE severity IN (?, ?) \
         GROUP BY source_id, stage, scope_kind, own_key, severity, muted",
    )
    .bind(datalib_problems::INDEX_OWN_SCOPE_KEY)
    .bind(serde_json::to_string(muted)?)
    .bind(Severity::Error.as_str())
    .bind(Severity::Warning.as_str())
    .fetch_all(index)
    .await
    {
        Ok(rows) => rows,
        Err(e) if datalib_pin::is_missing_table(&e, "problems") => return Ok(HashMap::new()),
        Err(e) => return Err(e).context("count the problems in the index"),
    };
    let mut out: HashMap<Counter, Counted> = HashMap::new();
    for r in rows {
        let word = |col: &str| -> Result<String> { Ok(r.try_get::<String, _>(col)?) };
        let (stage, severity, scope_kind) =
            (word("stage")?, word("severity")?, word("scope_kind")?);
        let (Some(stage), Some(severity), Some(scope_kind)) = (
            Stage::parse(&stage),
            Severity::parse(&severity),
            ScopeKind::parse(&scope_kind),
        ) else {
            anyhow::bail!(
                "problems: rows in words this build does not know \
                 ({stage:?}, {severity:?}, {scope_kind:?})"
            );
        };
        let by = CountedBy::of(stage, scope_kind, &word("own_key")?);
        let entry = out.entry(counter_of(&word("source_id")?, by)).or_default();
        let n: i64 = r.try_get("n")?;
        if r.try_get::<bool, _>("muted")? {
            entry.muted.add(severity, n);
        } else {
            entry.shown.add(severity, n);
        }
    }
    Ok(out)
}

/// [`count_in_index`] over the root's grid index, read-only, with the
/// mutes the store has published. `None` when there is no index yet, so
/// nothing has been counted.
pub async fn counts(data_root: &Path) -> Result<Option<HashMap<Counter, Counted>>> {
    let index = datalib_runtime::layout::grid_index_db(data_root);
    if !index.exists() {
        return Ok(None);
    }
    let muted: Vec<String> = read_published(data_root).await?.into_keys().collect();
    let pool = datalib_pin::open_reader(&index).await?;
    let counted = count_in_index(&pool, &muted).await;
    pool.close().await;
    counted.map(Some)
}

/// What every query below ends in: a count leaves out info rows and
/// muted ones.
const COUNTED_ONLY: &str = "-severity:info muted:no";

/// The problems grid's query for exactly the rows a counter shows.
pub fn query(counter: &Counter) -> String {
    let own = datalib_problems::INDEX_OWN_SCOPE_KEY;
    match counter {
        (_, CountedBy::Index) => format!("scope:entity doc:{own} {COUNTED_ONLY}"),
        (Some(source), CountedBy::Download) => {
            format!("source:{source} stage:fetch -doc:{own} {COUNTED_ONLY}")
        }
        (Some(source), CountedBy::Render) => {
            format!("source:{source} -stage:fetch -doc:{own} {COUNTED_ONLY}")
        }
        (None, by) => unreachable!("a {by:?} count is always a source's"),
    }
}

/// The query for the rows a group's counters show together: one
/// counter's, or a source's download and render as one. `None` for any
/// other mix, which no one query names.
pub fn group_query(counters: &[Counter]) -> Option<String> {
    let mut counters: Vec<&Counter> = counters.iter().collect();
    counters.sort_by_key(|(s, by)| (s.clone(), *by as u8));
    counters.dedup();
    match counters.as_slice() {
        [] => None,
        [one] => Some(query(one)),
        [(Some(a), CountedBy::Download), (Some(b), CountedBy::Render)] if a == b => Some(format!(
            "source:{a} -doc:{} {COUNTED_ONLY}",
            datalib_problems::INDEX_OWN_SCOPE_KEY
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::{Outcome, Problem, ProblemRow, Reason, Scope, INDEX_OWN_SCOPE_KEY};

    /// strum writes the store, serde the wire: two spellings of one value
    /// would read a stored mute back as unknown.
    #[test]
    fn muted_how_spells_the_same_both_ways() {
        for &v in MutedHow::VARIANTS {
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                format!("\"{}\"", v.as_str())
            );
            assert_eq!(MutedHow::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn a_group_query_names_one_counter_or_one_source() {
        let c = |s: Option<&str>, by| (s.map(str::to_string), by);
        let download = c(Some("enterprise"), CountedBy::Download);
        let render = c(Some("enterprise"), CountedBy::Render);
        assert_eq!(
            group_query(std::slice::from_ref(&render)),
            Some(query(&render))
        );
        assert_eq!(
            group_query(&[render.clone(), download.clone()]).as_deref(),
            Some("source:enterprise -doc:render_store -severity:info muted:no")
        );
        assert_eq!(
            group_query(&[download, c(Some("defiant"), CountedBy::Render)]),
            None
        );
        assert_eq!(group_query(&[]), None);
    }

    async fn plain(path: &Path) -> SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .unwrap()
    }

    /// Muting again keeps the first stamp, unmuting removes only what it
    /// names, and a store with no table yet holds no mutes.
    #[tokio::test]
    async fn mute_and_unmute_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = plain(&dir.path().join("m.sqlite")).await;
        assert!(read_all(&pool).await.unwrap().is_empty());
        sqlx::query(DDL[0]).execute(&pool).await.unwrap();
        let first = datalib_time::split_stamp("2370-01-01T00:00:00+02:00");
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        mute(&pool, &ids(&["a", "b"]), MutedHow::Manual, None, &first)
            .await
            .unwrap();
        let later = datalib_time::split_stamp("2370-02-01T00:00:00+02:00");
        mute(
            &pool,
            &ids(&["b", "c"]),
            MutedHow::Agent,
            Some("data"),
            &later,
        )
        .await
        .unwrap();
        let all = read_all(&pool).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all["b"].muted_at_utc, first.utc,
            "muting again says nothing new"
        );
        assert_eq!(all["b"].muted_how, MutedHow::Manual);
        assert_eq!(all["c"].agent.as_deref(), Some("data"));
        assert_eq!(all["a"].tz_offset.as_deref(), Some("+02:00"));
        unmute(&pool, &ids(&["a", "z"])).await.unwrap();
        let mut left: Vec<String> = read_all(&pool).await.unwrap().into_keys().collect();
        left.sort();
        assert_eq!(left, ["b", "c"]);
        pool.close().await;
    }

    /// Each row is counted against the step that found it, a muted one
    /// apart, an info row nowhere, and a mute of a row the index no
    /// longer holds counts nothing.
    #[tokio::test]
    async fn rows_are_counted_by_whose_they_are_and_whether_muted() {
        let dir = tempfile::tempdir().unwrap();
        let pool = plain(&dir.path().join("index.sqlite")).await;
        assert!(count_in_index(&pool, &[]).await.unwrap().is_empty());
        sqlx::query(datalib_problems::DDL[0].1)
            .execute(&pool)
            .await
            .unwrap();
        let row = |source: &str, stage, scope, severity, sample: &str| {
            ProblemRow::new(
                source,
                stage,
                scope,
                None,
                Outcome::Dropped,
                Problem::record(Reason::RenderFailed, sample).severity(severity),
                None,
            )
        };
        let fetch = row(
            "enterprise",
            Stage::Fetch,
            Scope::Entity("logs:1"),
            Severity::Error,
            "a",
        );
        let parse = row(
            "enterprise",
            Stage::Parse,
            Scope::Entity("logs:2"),
            Severity::Warning,
            "b",
        );
        let info = row(
            "enterprise",
            Stage::Render,
            Scope::Markdown("md-1"),
            Severity::Info,
            "c",
        );
        let unreadable = row(
            "defiant",
            Stage::Render,
            Scope::Entity(INDEX_OWN_SCOPE_KEY),
            Severity::Warning,
            "d",
        );
        for row in [&fetch, &parse, &info, &unreadable] {
            sqlx::query(
                "INSERT INTO problems (problem_uuid, source_id, stage, severity, outcome, \
                 reason, scope_kind, scope_key, sample, first_seen_at_utc, changed_at_utc) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '', '')",
            )
            .bind(&row.problem_uuid)
            .bind(&row.source_id)
            .bind(row.stage.as_str())
            .bind(row.severity.as_str())
            .bind(row.outcome.as_str())
            .bind(row.reason.as_str())
            .bind(row.scope_kind.as_str())
            .bind(&row.scope_key)
            .bind(&row.sample)
            .execute(&pool)
            .await
            .unwrap();
        }
        let counted = count_in_index(&pool, &[parse.problem_uuid.clone(), "fixed-since".into()])
            .await
            .unwrap();
        let t = |errors, warnings| Tally { errors, warnings };
        assert_eq!(
            counted[&counter_of("enterprise", CountedBy::Download)],
            Counted {
                shown: t(1, 0),
                muted: t(0, 0)
            }
        );
        assert_eq!(
            counted[&counter_of("enterprise", CountedBy::Render)],
            Counted {
                shown: t(0, 0),
                muted: t(0, 1)
            }
        );
        assert_eq!(
            counted[&(None, CountedBy::Index)],
            Counted {
                shown: t(0, 1),
                muted: t(0, 0)
            }
        );
        assert_eq!(counted.len(), 3, "{counted:?}");
        pool.close().await;
    }

    /// A root with no store has no mutes, and making none is the reader's
    /// whole job: it must never create the file.
    #[tokio::test]
    async fn a_root_with_no_store_has_no_mutes_and_gets_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_published(dir.path()).await.unwrap().is_empty());
        assert_eq!(counts(dir.path()).await.unwrap(), None);
        assert!(!store_path(dir.path()).exists());
    }
}
