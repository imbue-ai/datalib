//! The migration ladder: how a store's shape moves when `ADD COLUMN`
//! cannot get it there. A store owner declares its migrations in order;
//! `_datalib_meta.schema_version` says how many have run; an open runs
//! the rest, one at a time, each in its own transaction and — for a
//! doltlite store — its own commit. The tables every store shares climb
//! a second ladder the framework keeps, counted by
//! `shared_schema_version`. The reference is etl/README.md
//! §"The migration ladder"; the design record is
//! docs/dev/plans/completed/schema_migrations.md §3.3.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

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

/// Which of a store's two ladders: its owner's, or the framework's for
/// the tables every store shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ladder {
    Own,
    Shared,
}

impl Ladder {
    fn key(self) -> &'static str {
        match self {
            Ladder::Own => "schema_version",
            Ladder::Shared => "shared_schema_version",
        }
    }
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

/// Where one store's climb starts and ends, which a rung's report names
/// so a person can tell how much is left.
#[derive(Debug, Clone, Copy)]
pub struct Climb {
    pub ladder: Ladder,
    /// The version the store was at when the climb began.
    pub from: u32,
    pub top: u32,
    /// The store file's size, when the owner knows its path.
    pub store_bytes: Option<u64>,
}

impl Climb {
    /// What a person reads while `m` runs, in the log and on the step's
    /// row.
    pub fn describe(&self, m: &Migration) -> String {
        let what = match self.ladder {
            Ladder::Own => "the store",
            Ladder::Shared => "the store's shared tables",
        };
        let size = match self.store_bytes {
            Some(b) if b >= 1_000_000_000 => format!(" ({:.1} GB)", b as f64 / 1e9),
            Some(b) => format!(" ({} MB)", b / 1_000_000),
            None => String::new(),
        };
        format!(
            "migrating {what}{size} from version {} to {}, now version {}: {}",
            self.from, self.top, m.version, m.name
        )
    }
}

type Reporter = Box<dyn Fn(&str) + Send + Sync>;

/// Where a rung's reports go besides the log, and the rung running now. A
/// step process points the reporter at its progress message, so the
/// step's row says which rung, and which statement of it, is running; a
/// rung can take as long as the store is big, and the log says nothing
/// more until it ends.
static REPORTER: Mutex<Option<Reporter>> = Mutex::new(None);
static RUNG: Mutex<String> = Mutex::new(String::new());

pub fn report_to(reporter: impl Fn(&str) + Send + Sync + 'static) {
    *REPORTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(reporter));
}

fn report(msg: &str) {
    if let Some(reporter) = &*REPORTER.lock().unwrap_or_else(|e| e.into_inner()) {
        reporter(msg);
    }
}

/// One statement of a rung, and what it does in a few words.
pub struct Statement {
    pub what: &'static str,
    pub sql: &'static str,
}

/// Run a rung's statements in order, saying before each which it is: one
/// statement over every row of a big store can run for many minutes, and
/// a person watching should know which.
pub async fn run_statements(
    conn: &mut sqlx::SqliteConnection,
    statements: &[Statement],
) -> Result<()> {
    for (i, s) in statements.iter().enumerate() {
        let msg = format!("statement {} of {}: {}", i + 1, statements.len(), s.what);
        tracing::info!(statement = i + 1, of = statements.len(), "migrating: {msg}");
        let rung = RUNG.lock().unwrap_or_else(|e| e.into_inner()).clone();
        report(&format!("{rung}; {msg}"));
        sqlx::query(s.sql)
            .execute(&mut *conn)
            .await
            .with_context(|| msg.clone())?;
    }
    Ok(())
}

/// Run one rung: its body and the version bump, in one transaction.
/// The owner commits after, so a crash between the two leaves the
/// working set for the next open to discard and the rung to run again.
pub async fn apply(pool: &SqlitePool, m: &Migration, climb: &Climb) -> Result<()> {
    let ladder = climb.ladder;
    let said = climb.describe(m);
    tracing::info!(
        version = m.version,
        name = m.name,
        ladder = ladder.key(),
        from = climb.from,
        top = climb.top,
        store_bytes = climb.store_bytes,
        "{said}"
    );
    report(&said);
    said.clone_into(&mut RUNG.lock().unwrap_or_else(|e| e.into_inner()));
    let started = std::time::Instant::now();
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
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
         written_at_utc = excluded.written_at_utc, tz_offset = excluded.tz_offset",
    )
    .bind(ladder.key())
    .bind(m.version.to_string())
    .bind(&now)
    .bind(&tz_offset)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("write {}", ladder.key()))?;
    report(&format!("{said} (committing)"));
    tx.commit()
        .await
        .with_context(|| format!("commit migration v{}", m.version))?;
    tracing::info!(
        version = m.version,
        name = m.name,
        ladder = ladder.key(),
        ms = started.elapsed().as_millis() as u64,
        "migrated"
    );
    Ok(())
}

/// Where the store stands on `ladder`, `0` with no meta or no such row.
pub async fn stored_version(pool: &SqlitePool, ladder: Ladder) -> Result<u32> {
    Ok(crate::read(pool).await?.map_or(0, |m| match ladder {
        Ladder::Own => m.schema_version,
        Ladder::Shared => m.shared_schema_version,
    }))
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

    /// A rung that runs for many minutes says, before it starts and before
    /// each statement, where the climb is: from which version to which,
    /// how big the store is, and which statement of the rung is running
    /// (#1678: a 37-minute rung said only its name).
    #[tokio::test]
    async fn a_rung_reports_its_climb_and_each_statement() {
        let said = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = said.clone();
        report_to(move |msg| sink.lock().unwrap().push(msg.to_string()));

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(crate::DDL).execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE log (v INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let two_statements = Migration {
            version: 8,
            name: "read marks move",
            apply: |c| {
                Box::pin(async move {
                    run_statements(
                        c,
                        &[
                            Statement {
                                what: "first",
                                sql: "INSERT INTO log (v) VALUES (1)",
                            },
                            Statement {
                                what: "second",
                                sql: "INSERT INTO log (v) VALUES (2)",
                            },
                        ],
                    )
                    .await
                })
            },
        };
        let climb = Climb {
            ladder: Ladder::Own,
            from: 7,
            top: 9,
            store_bytes: Some(13_046_000_000),
        };
        apply(&pool, &two_statements, &climb).await.unwrap();
        pool.close().await;

        let start = "migrating the store (13.0 GB) from version 7 to 9, now version 8: \
                     read marks move";
        assert_eq!(
            *said.lock().unwrap(),
            [
                start.to_string(),
                format!("{start}; statement 1 of 2: first"),
                format!("{start}; statement 2 of 2: second"),
                format!("{start} (committing)"),
            ]
        );
    }

    #[test]
    fn a_climb_names_the_shared_tables_and_a_small_store_in_megabytes() {
        let climb = Climb {
            ladder: Ladder::Shared,
            from: 0,
            top: 2,
            store_bytes: Some(4_500_000),
        };
        assert_eq!(
            climb.describe(&rung(1)),
            "migrating the store's shared tables (4 MB) from version 0 to 2, now version 1: test"
        );
        let unsized_climb = Climb {
            store_bytes: None,
            ..climb
        };
        assert_eq!(
            unsized_climb.describe(&rung(2)),
            "migrating the store's shared tables from version 0 to 2, now version 2: test"
        );
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
