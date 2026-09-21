//! The migration ladder: how a store's shape moves when `ADD COLUMN`
//! cannot get it there. A store owner declares its migrations in order;
//! `_datalib_meta.schema_version` says how many have run; an open runs
//! the rest, one at a time, each in its own transaction and — for a
//! doltlite store — its own commit. The reference is etl/README.md
//! §"The migration ladder"; the design record is
//! docs/dev/plans/completed/schema_migrations.md §3.3.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// A migration's body: runs against a store at `version - 1`, inside
/// the transaction that also bumps the version. May read the old shape
/// through `dolt_at_<table>('HEAD')` and write the new one, and may
/// `DELETE FROM` a cursor table when the change alters what the cursor
/// means.
pub type Apply = for<'c> fn(
    &'c mut sqlx::SqliteConnection,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'c>>;

/// One rung. `version` is dense from 1 within a store's ladder.
pub struct Migration {
    pub version: u32,
    /// One line; the commit message.
    pub name: &'static str,
    pub apply: Apply,
}

/// The rungs above `current`, in order. A ladder is checked for being
/// dense and sorted here rather than trusted: a gap or a repeat is a
/// bug in the ladder, not a state of the store.
pub fn pending(ladder: &[Migration], current: u32) -> Result<Vec<&Migration>> {
    for (i, m) in ladder.iter().enumerate() {
        anyhow::ensure!(
            m.version == i as u32 + 1,
            "migration ladder is not dense from 1: rung {} is v{}",
            i,
            m.version
        );
    }
    Ok(ladder.iter().filter(|m| m.version > current).collect())
}

/// The ladder's top, which is the `schema_version` a store at the end
/// of it carries; `0` for no ladder.
pub fn top(ladder: &[Migration]) -> u32 {
    ladder.last().map_or(0, |m| m.version)
}

/// A store whose `schema_version` is above the ladder's top was
/// migrated by a build with a longer ladder; nothing here knows what
/// those rungs did.
#[derive(Debug)]
pub struct AheadOfLadder {
    pub stored: u32,
    pub top: u32,
}

impl std::fmt::Display for AheadOfLadder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the store is at schema version {} and this build's ladder ends at {}: \
             a newer build migrated it, and this one cannot know what changed",
            self.stored, self.top
        )
    }
}

impl std::error::Error for AheadOfLadder {}

/// Run one rung: its body and the version bump, in one transaction.
/// The owner commits after, so a crash between the two leaves the
/// working set for the next open to discard and the rung to run again.
pub async fn apply(pool: &SqlitePool, m: &Migration) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin migration v{}", m.version))?;
    (m.apply)(&mut tx)
        .await
        .with_context(|| format!("migration v{} ({})", m.version, m.name))?;
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    sqlx::query(
        "INSERT INTO _datalib_meta (key, value, written_at_utc, tz_offset) \
         VALUES ('schema_version', ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
         written_at_utc = excluded.written_at_utc, tz_offset = excluded.tz_offset",
    )
    .bind(m.version.to_string())
    .bind(&now)
    .bind(&tz_offset)
    .execute(&mut *tx)
    .await
    .context("write schema_version")?;
    tx.commit()
        .await
        .with_context(|| format!("commit migration v{}", m.version))?;
    tracing::info!(version = m.version, name = m.name, "migrated");
    Ok(())
}

/// The store's `schema_version`, `0` with no meta or no such row.
pub async fn stored_version(pool: &SqlitePool) -> Result<u32> {
    Ok(crate::read(pool).await?.map_or(0, |m| m.schema_version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rung(version: u32) -> Migration {
        Migration {
            version,
            name: "test",
            apply: |c| {
                Box::pin(async move {
                    sqlx::query("INSERT INTO log (v) VALUES (1)")
                        .execute(&mut *c)
                        .await?;
                    Ok(())
                })
            },
        }
    }

    /// The rungs above the stored version, and a ladder with a gap or a
    /// repeat is refused as a bug rather than run around.
    #[test]
    fn pending_is_the_rungs_above_the_stored_version() {
        let ladder = [rung(1), rung(2), rung(3)];
        let above = |current| {
            pending(&ladder, current)
                .unwrap()
                .iter()
                .map(|m| m.version)
                .collect::<Vec<_>>()
        };
        assert_eq!(above(0), vec![1, 2, 3]);
        assert_eq!(above(2), vec![3]);
        assert_eq!(above(3), Vec::<u32>::new());
        assert_eq!(top(&ladder), 3);
        assert_eq!(top(&[]), 0);
        assert!(pending(&[rung(1), rung(3)], 0).is_err());
        assert!(pending(&[rung(2)], 0).is_err());
    }
}
