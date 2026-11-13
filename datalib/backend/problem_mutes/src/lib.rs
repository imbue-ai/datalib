//! Problem rows a person muted: "I have seen this, stop telling me". The
//! store's shape, its reads and writes over a pool someone else opened,
//! and the read of what is still muted in the grid index. Opening the
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

/// A muted problem the grid index still holds, with what says whose
/// count it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMute {
    pub source_id: String,
    pub counted_by: CountedBy,
    pub severity: Severity,
}

/// The muted problems among the index's rows now. A mute whose problem
/// is gone — fixed, or changed into another id — is not among them, so
/// it takes nothing off a count.
pub async fn live_in_index(index: &SqlitePool, ids: &[String]) -> Result<Vec<LiveMute>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = match sqlx::query(
        "SELECT source_id, stage, severity, scope_kind, scope_key FROM problems \
         WHERE problem_uuid IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(ids)?)
    .fetch_all(index)
    .await
    {
        Ok(rows) => rows,
        Err(e) if datalib_pin::is_missing_table(&e, "problems") => return Ok(Vec::new()),
        Err(e) => return Err(e).context("read the muted problems in the index"),
    };
    let mut out = Vec::new();
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
                "problems: a muted row in words this build does not know \
                 ({stage:?}, {severity:?}, {scope_kind:?})"
            );
        };
        out.push(LiveMute {
            source_id: word("source_id")?,
            counted_by: CountedBy::of(stage, scope_kind, &word("scope_key")?),
            severity,
        });
    }
    Ok(out)
}

/// [`live_in_index`] over the root's grid index, read-only, for the mutes
/// the store has published. No index, or no mutes, is none.
pub async fn live_mutes(data_root: &Path) -> Result<Vec<LiveMute>> {
    let mutes = read_published(data_root).await?;
    let index = datalib_runtime::layout::grid_index_db(data_root);
    if mutes.is_empty() || !index.exists() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = mutes.into_keys().collect();
    let pool = datalib_pin::open_reader(&index).await?;
    let live = live_in_index(&pool, &ids).await;
    pool.close().await;
    live
}

/// Errors and warnings muted, as a count is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Muted {
    pub errors: i64,
    pub warnings: i64,
}

/// The live mutes tallied by whose count they are in: `(source, counter)`
/// for a source's download and render, `(None, Index)` for the index's
/// own. An `info` row is in no count, so it takes nothing off one.
pub fn tally(live: &[LiveMute]) -> HashMap<(Option<String>, CountedBy), Muted> {
    let mut out: HashMap<(Option<String>, CountedBy), Muted> = HashMap::new();
    for m in live {
        let key = match m.counted_by {
            CountedBy::Index => (None, CountedBy::Index),
            by => (Some(m.source_id.clone()), by),
        };
        let entry = out.entry(key).or_default();
        match m.severity {
            Severity::Error => entry.errors += 1,
            Severity::Warning => entry.warnings += 1,
            Severity::Info => {}
        }
    }
    out.retain(|_, m| *m != Muted::default());
    out
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

    fn live(source: &str, by: CountedBy, severity: Severity) -> LiveMute {
        LiveMute {
            source_id: source.into(),
            counted_by: by,
            severity,
        }
    }

    #[test]
    fn mutes_are_tallied_by_whose_count_they_are_in() {
        let t = tally(&[
            live("enterprise", CountedBy::Download, Severity::Warning),
            live("enterprise", CountedBy::Render, Severity::Error),
            live("enterprise", CountedBy::Render, Severity::Warning),
            live("enterprise", CountedBy::Render, Severity::Info),
            live("defiant", CountedBy::Index, Severity::Error),
            live("defiant", CountedBy::Render, Severity::Info),
        ]);
        assert_eq!(
            t[&(Some("enterprise".into()), CountedBy::Download)],
            Muted {
                errors: 0,
                warnings: 1
            }
        );
        assert_eq!(
            t[&(Some("enterprise".into()), CountedBy::Render)],
            Muted {
                errors: 1,
                warnings: 1
            }
        );
        assert_eq!(
            t[&(None, CountedBy::Index)],
            Muted {
                errors: 1,
                warnings: 0
            }
        );
        assert_eq!(t.len(), 3, "an info row takes nothing off: {t:?}");
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

    /// Only a muted problem the index still holds is live, and each is
    /// counted against the step that found it.
    #[tokio::test]
    async fn only_a_muted_problem_still_in_the_index_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let pool = plain(&dir.path().join("index.sqlite")).await;
        assert!(live_in_index(&pool, &["x".into()])
            .await
            .unwrap()
            .is_empty());
        sqlx::query(datalib_problems::DDL[0].1)
            .execute(&pool)
            .await
            .unwrap();
        let fetch = ProblemRow::new(
            "enterprise",
            Stage::Fetch,
            Scope::Entity("logs:1"),
            None,
            Outcome::Dropped,
            Problem::record(Reason::FetchFailed, "subspace interference"),
            None,
        );
        let unreadable = ProblemRow::new(
            "defiant",
            Stage::Render,
            Scope::Entity(INDEX_OWN_SCOPE_KEY),
            None,
            Outcome::Dropped,
            Problem::record(Reason::RenderFailed, "older shape").severity(Severity::Warning),
            None,
        );
        for row in [&fetch, &unreadable] {
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
        let mut found = live_in_index(
            &pool,
            &[
                fetch.problem_uuid.clone(),
                unreadable.problem_uuid.clone(),
                "fixed-since".into(),
            ],
        )
        .await
        .unwrap();
        found.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        assert_eq!(
            found,
            [
                live("defiant", CountedBy::Index, Severity::Warning),
                live("enterprise", CountedBy::Download, Severity::Error),
            ]
        );
        pool.close().await;
    }

    /// A root with no store has no mutes, and making none is the reader's
    /// whole job: it must never create the file.
    #[tokio::test]
    async fn a_root_with_no_store_has_no_mutes_and_gets_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_published(dir.path()).await.unwrap().is_empty());
        assert!(live_mutes(dir.path()).await.unwrap().is_empty());
        assert!(!store_path(dir.path()).exists());
    }
}
