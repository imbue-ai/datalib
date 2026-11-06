//! What the person has read or muted, in the one shape every source with
//! read state shares: a narrow `read_marks` table in its raw store. Narrow
//! because a mark changes often and on its own, and a row is stored whole
//! (docs/dev/doltlite.md §"What a write costs"). A table of its own and
//! not a sidecar, because a mark is something a reader must see: a change
//! to it moves the store's version (`doltlite_raw::moves_version`).
//!
//! A source whose raw store is a straight copy of an app's database
//! (Apple Messages, WhatsApp) keeps its marks where the app keeps them;
//! its render builds the same [`ReadMark`]s from those columns.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use datalib_etl_macros::RawTable;
use sqlx::{Sqlite, SqlitePool, Transaction};

pub const TABLE: &str = "read_marks";

/// One mark: on a conversation or thread, the last item read
/// (`read_through`); on one item, whether it was read (`read`). Exactly
/// one of the two is set. `muted` says whether the person muted the item,
/// NULL where the source has not said.
#[derive(Debug, Clone, PartialEq, Eq, RawTable)]
#[raw_table(table = "read_marks")]
pub struct ReadMarkRow {
    /// What the mark is on, in the source's own key space.
    pub id: String,
    pub read_through: Option<String>,
    pub read: Option<i64>,
    pub muted: Option<i64>,
}

impl ReadMarkRow {
    pub fn through(id: impl Into<String>, last_read: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            read_through: Some(last_read.into()),
            read: None,
            muted: None,
        }
    }

    pub fn read(id: impl Into<String>, read: bool) -> Self {
        Self {
            id: id.into(),
            read_through: None,
            read: Some(i64::from(read)),
            muted: None,
        }
    }

    pub fn with_muted(self, muted: Option<bool>) -> Self {
        Self {
            muted: muted.map(i64::from),
            ..self
        }
    }

    pub fn mark(&self) -> Option<ReadMark> {
        match (&self.read_through, self.read) {
            (Some(through), _) => Some(ReadMark::Through(through.clone())),
            (None, Some(read)) => Some(ReadMark::Read(read != 0)),
            (None, None) => None,
        }
    }
}

/// A mark as a render uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadMark {
    /// Everything up to and including this item has been read.
    Through(String),
    /// This one item has, or has not, been read.
    Read(bool),
}

/// Write `rows`, each replacing the mark stored for its id.
pub async fn write(tx: &mut Transaction<'_, Sqlite>, rows: &[ReadMarkRow]) -> Result<()> {
    crate::bulk::bulk_upsert_entity_in_tx(tx, rows)
        .await
        .context("write read_marks")
}

/// Forget the marks on `ids`: what they were on is gone, or upstream no
/// longer says.
pub async fn forget(tx: &mut Transaction<'_, Sqlite>, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let ids = serde_json::to_string(ids).context("serialize read mark ids")?;
    sqlx::query("DELETE FROM read_marks WHERE id IN (SELECT value FROM json_each(?))")
        .bind(ids)
        .execute(&mut **tx)
        .await
        .context("forget read_marks")?;
    Ok(())
}

/// Every mark the store holds, by id. A store from before the table
/// holds none.
pub async fn load(pool: &SqlitePool) -> Result<BTreeMap<String, ReadMark>> {
    if !crate::doltlite_raw::table_exists(pool, TABLE).await? {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT id, read_through, read FROM read_marks")
            .fetch_all(pool)
            .await
            .context("load read_marks")?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, read_through, read)| {
            ReadMarkRow {
                id: id.clone(),
                read_through,
                read,
                muted: None,
            }
            .mark()
            .map(|m| (id, m))
        })
        .collect())
}

/// A mark that changed between two commits: what it was and what it is
/// now, `None` where there was or is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedMark {
    pub id: String,
    pub was: Option<ReadMark>,
    pub is: Option<ReadMark>,
}

/// The marks whose reading changed between `from` and `to`; a mute alone
/// redraws nothing. Empty when either commit has no `read_marks` table: a
/// render reading across the rung that created it treats the marks it
/// starts from as unchanged, rather than every mark as new.
pub async fn moved(pool: &SqlitePool, from: &str, to: &str) -> Result<Vec<MovedMark>> {
    if !crate::doltlite_raw::table_exists(pool, TABLE).await? {
        return Ok(Vec::new());
    }
    let created_between: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dolt_diff_summary WHERE from_ref = ? AND to_ref = ? \
         AND to_table_name = ? AND diff_type = 'added'",
    )
    .bind(from)
    .bind(to)
    .bind(TABLE)
    .fetch_one(pool)
    .await
    .context("was read_marks created between the two commits")?;
    if created_between > 0 {
        return Ok(Vec::new());
    }
    type Diff = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let rows: Vec<Diff> = sqlx::query_as(
        "SELECT from_id, to_id, from_read_through, from_read, to_read_through, to_read \
         FROM dolt_diff_read_marks WHERE from_ref = ? AND to_ref = ? AND diff_type != 'unchanged'",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("dolt_diff_read_marks")?;
    let mark = |through: Option<String>, read: Option<i64>| {
        ReadMarkRow {
            id: String::new(),
            read_through: through,
            read,
            muted: None,
        }
        .mark()
    };
    Ok(rows
        .into_iter()
        .filter_map(|(from_id, to_id, ft, fr, tt, tr)| {
            Some(MovedMark {
                id: to_id.or(from_id)?,
                was: mark(ft, fr),
                is: mark(tt, tr),
            })
        })
        .filter(|moved| moved.was != moved.is)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doltlite_raw::{commit_run, head_commit, open};

    /// What a render reads: every mark, and which moved between two
    /// commits. Across the commit that created the table, none did, or a
    /// store's first render after its rung would redraw every document.
    #[tokio::test]
    async fn moved_names_what_changed_and_nothing_across_the_tables_creation() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("entities.doltlite_db");
        let pool = open(&path, &[]).await.unwrap();
        let before = head_commit(&pool).await.unwrap().unwrap();
        pool.close().await;

        let ddl = ReadMarkRow::all_ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = open(&path, &ddl).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        write(
            &mut tx,
            &[
                ReadMarkRow::through("C1", "1735689600.000100"),
                ReadMarkRow::read("m1", false),
                ReadMarkRow::read("m2", true),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        commit_run(&pool, "marks").await.unwrap();
        let created = head_commit(&pool).await.unwrap().unwrap();
        assert_eq!(moved(&pool, &before, &created).await.unwrap(), vec![]);

        let mut tx = pool.begin().await.unwrap();
        write(
            &mut tx,
            &[
                ReadMarkRow::through("C1", "1735689600.000200"),
                ReadMarkRow::read("m1", true),
            ],
        )
        .await
        .unwrap();
        forget(&mut tx, &["m2".to_string()]).await.unwrap();
        tx.commit().await.unwrap();
        commit_run(&pool, "read").await.unwrap();
        let after = head_commit(&pool).await.unwrap().unwrap();

        let mut got = moved(&pool, &created, &after).await.unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            got,
            vec![
                MovedMark {
                    id: "C1".into(),
                    was: Some(ReadMark::Through("1735689600.000100".into())),
                    is: Some(ReadMark::Through("1735689600.000200".into())),
                },
                MovedMark {
                    id: "m1".into(),
                    was: Some(ReadMark::Read(false)),
                    is: Some(ReadMark::Read(true)),
                },
                MovedMark {
                    id: "m2".into(),
                    was: Some(ReadMark::Read(true)),
                    is: None,
                },
            ]
        );
        let all = load(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["m1"], ReadMark::Read(true));
        pool.close().await;
    }

    /// Filling in who is muted touches every mark of a store once. Each
    /// mark moving would redraw every document, for a change no document
    /// shows.
    #[tokio::test]
    async fn a_mute_alone_is_not_a_moved_mark() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("entities.doltlite_db");
        let ddl = ReadMarkRow::all_ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = open(&path, &ddl).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        write(&mut tx, &[ReadMarkRow::read("m1", false)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        commit_run(&pool, "unread").await.unwrap();
        let before = head_commit(&pool).await.unwrap().unwrap();

        let mut tx = pool.begin().await.unwrap();
        write(
            &mut tx,
            &[ReadMarkRow::read("m1", false).with_muted(Some(true))],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        commit_run(&pool, "muted").await.unwrap();
        let after = head_commit(&pool).await.unwrap().unwrap();

        assert_eq!(moved(&pool, &before, &after).await.unwrap(), vec![]);
        let muted: Option<i64> = sqlx::query_scalar("SELECT muted FROM read_marks WHERE id = 'm1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(muted, Some(1));
        pool.close().await;
    }

    /// A mark is content: a read moves the store's version, so the render
    /// that draws it runs.
    #[tokio::test]
    async fn a_read_moves_the_version() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("entities.doltlite_db");
        let ddl = ReadMarkRow::all_ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = open(&path, &ddl).await.unwrap();
        let version = crate::doltlite_raw::content_commit(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        write(&mut tx, &[ReadMarkRow::through("C1", "1")])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        commit_run(&pool, "read").await.unwrap();
        assert_ne!(
            crate::doltlite_raw::content_commit(&pool).await.unwrap(),
            version
        );
        pool.close().await;
    }
}
