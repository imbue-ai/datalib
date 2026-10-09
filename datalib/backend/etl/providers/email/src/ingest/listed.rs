//! What upstream listed, and what the store writes for a batch the
//! fetch answered: the state both API downloads (JMAP, Gmail) keep.
//!
//! `listed_messages` has a row per message upstream named, keyed by
//! upstream's own id, with the token of the response that last named it
//! as changed. It is the one listing that has to be stored: a delta's
//! answer cannot be asked for again once the token has moved. What is
//! held for a message is `held_version` on its
//! `listed_messages_bookkeeping` row, which `datalib_etl_web::owed` writes
//! in the transaction that writes the email; what is owed is the
//! difference, asked of the store each time and never stored.
//! `listed_whole` names the scopes (a mailbox, a label, or the account)
//! an enumeration has listed to its end. The rule and why:
//! docs/dev/data_architecture_ingestion.md, "What is left to fetch".

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_etl_web::owed::{self, Listed};
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use super::db::{delete_emails_in_tx, refresh_email_joins};
use super::schema_raw::{EmailRow, ThreadRow};

pub const LISTED: &str = "listed_messages";

pub const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS listed_messages (id TEXT PRIMARY KEY, stamp TEXT NULL)",
    "CREATE TABLE IF NOT EXISTS listed_whole (scope TEXT PRIMARY KEY)",
];

/// The scope of an enumeration no filter narrows.
pub const WHOLE_ACCOUNT: &str = "*";

/// How the `emails` row for a message is keyed from upstream's id for
/// it: the id itself for JMAP, `GmailId`'s key for Gmail.
pub type EmailIdOf = fn(&str) -> String;

/// [`EmailIdOf`] for JMAP, whose `Email.id` is the row's id.
pub fn same_id(id: &str) -> String {
    id.to_string()
}

/// What naming a message says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// A delta named it: whatever is held for it is out of date.
    Changed,
    /// An enumeration named it: it exists. What is held for a message
    /// already listed stands, since the delta says when that changes.
    Exists,
}

pub async fn list_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    ids: &[String],
    stamp: Option<&str>,
    named: Named,
) -> Result<()> {
    let sql = match named {
        Named::Changed => {
            "INSERT INTO listed_messages (id, stamp) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET stamp = excluded.stamp"
        }
        Named::Exists => {
            "INSERT INTO listed_messages (id, stamp) VALUES (?, ?) ON CONFLICT(id) DO NOTHING"
        }
    };
    for id in ids {
        sqlx::query(sql)
            .bind(id)
            .bind(stamp)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list message {id}"))?;
    }
    Ok(())
}

/// Every listed message at its stamp, as `owed::owed` takes it: in id
/// order, or newest first.
pub async fn listing(pool: &SqlitePool, newest_first: bool) -> Result<Vec<Listed>> {
    let sql = if newest_first {
        "SELECT id, stamp FROM listed_messages ORDER BY id DESC"
    } else {
        "SELECT id, stamp FROM listed_messages ORDER BY id"
    };
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sql)
        .fetch_all(pool)
        .await
        .context("select the listed messages")?;
    Ok(rows
        .into_iter()
        .map(|(id, stamp)| Listed::new(id, stamp))
        .collect())
}

/// Whether upstream has listed anything.
pub async fn lists_any(pool: &SqlitePool) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM listed_messages)")
        .fetch_one(pool)
        .await
        .context("ask whether any message is listed")
}

/// Whether the store holds any email at all.
pub async fn holds_any(pool: &SqlitePool) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM emails)")
        .fetch_one(pool)
        .await
        .context("ask whether any email is held")
}

/// Write what a batch came to: the email rows and their joins for the
/// messages that came, the listing and email rows of the messages that
/// are gone, and the thread rows of both. Returns how many emails went.
pub async fn write_batch_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    got: Vec<EmailRow>,
    gone: &[String],
    email_id_of: EmailIdOf,
) -> Result<usize> {
    let mut threads = BTreeSet::new();
    for row in &got {
        threads.insert((row.account_id.clone(), row.thread_id.clone()));
    }
    bulk_upsert_in_tx(tx, &got, now).await?;
    for row in &got {
        refresh_email_joins(tx, row).await?;
    }

    let mut emails = Vec::new();
    for id in gone {
        sqlx::query("DELETE FROM listed_messages WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("unlist message {id}"))?;
        let email_id = email_id_of(id);
        let held: Option<(String, String)> =
            sqlx::query_as("SELECT account_id, thread_id FROM emails WHERE id = ?")
                .bind(&email_id)
                .fetch_optional(&mut **tx)
                .await
                .with_context(|| format!("look up the email of message {id}"))?;
        if let Some(thread) = held {
            threads.insert(thread);
            emails.push(email_id);
        }
    }
    delete_emails_in_tx(tx, &emails).await?;
    rebuild_threads_in_tx(tx, now, &threads).await?;
    Ok(emails.len())
}

/// Upstream no longer has these messages: the listing, what is held and
/// the email rows go together. Returns how many emails went.
pub async fn forget_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    ids: &[String],
    email_id_of: EmailIdOf,
) -> Result<usize> {
    let gone = write_batch_in_tx(tx, now, Vec::new(), ids, email_id_of).await?;
    for id in ids {
        owed::forget(tx, LISTED, id).await?;
    }
    Ok(gone)
}

/// A thread row is its emails, oldest first: written from the email
/// rows in the transaction that changes them, so it is never behind.
async fn rebuild_threads_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    threads: &BTreeSet<(String, String)>,
) -> Result<()> {
    let mut rows = Vec::new();
    for (account_id, thread_id) in threads {
        let email_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM emails WHERE thread_id = ? AND account_id = ?
             ORDER BY coalesce(received_at, ''), id",
        )
        .bind(thread_id)
        .bind(account_id)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("read the emails of thread {thread_id}"))?;
        if email_ids.is_empty() {
            for sql in [
                "DELETE FROM threads WHERE id = ?",
                "DELETE FROM threads_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(thread_id)
                    .execute(&mut **tx)
                    .await
                    .with_context(|| format!("delete thread {thread_id}"))?;
            }
            continue;
        }
        rows.push(ThreadRow::from_jmap_payload(
            thread_id,
            account_id,
            &json!({ "id": thread_id, "emailIds": email_ids }),
        )?);
    }
    bulk_upsert_in_tx(tx, &rows, now).await
}

// ── tokens and scopes ───────────────────────────────────────────────

pub async fn save_token_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    token: &str,
) -> Result<()> {
    let stamp = datalib_time::split_stamp(token);
    sqlx::query(
        "INSERT INTO sync_scope_state (scope, last_seen_at_utc, tz_offset) VALUES (?, ?, ?)
         ON CONFLICT(scope) DO UPDATE SET last_seen_at_utc = excluded.last_seen_at_utc,
            tz_offset = excluded.tz_offset",
    )
    .bind(scope)
    .bind(&stamp.utc)
    .bind(&stamp.tz_offset)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("save the token {scope}"))?;
    Ok(())
}

/// The delta has nothing to continue from: no token, or one upstream can
/// no longer replay. So nothing is listed whole any more, and the delta
/// starts again from `token`, the account's state now. With `refetch`,
/// what changed while no delta was replaying is taken to be unknown:
/// every listed message is listed again and nothing held answers for
/// it. Without, what is held stands.
pub async fn start_over(
    pool: &SqlitePool,
    token_scope: &str,
    token: Option<&str>,
    refetch: bool,
) -> Result<()> {
    let mut tx = pool.begin().await.context("begin start-over tx")?;
    sqlx::query("DELETE FROM listed_whole")
        .execute(&mut *tx)
        .await
        .context("forget which scopes were listed whole")?;
    match token {
        Some(token) => save_token_in_tx(&mut tx, token_scope, token).await?,
        None => {
            sqlx::query("DELETE FROM sync_scope_state WHERE scope = ?")
                .bind(token_scope)
                .execute(&mut *tx)
                .await
                .context("forget the token")?;
        }
    }
    if refetch {
        let token = token.context("listing every message again needs a token to list it under")?;
        sqlx::query("UPDATE listed_messages SET stamp = ?")
            .bind(token)
            .execute(&mut *tx)
            .await
            .context("list every message again")?;
        sqlx::query("UPDATE listed_messages_bookkeeping SET held_version = NULL")
            .execute(&mut *tx)
            .await
            .context("let nothing held answer for its listing")?;
    }
    tx.commit().await.context("commit start-over tx")
}

/// The scopes of `admitted` no enumeration has listed whole. A scope the
/// config no longer admits loses its row: its mail is no longer kept up,
/// so admitting it again must list it again.
pub async fn scopes_owed(pool: &SqlitePool, admitted: &BTreeSet<String>) -> Result<Vec<String>> {
    let held: Vec<String> = sqlx::query_scalar("SELECT scope FROM listed_whole")
        .fetch_all(pool)
        .await
        .context("select the scopes listed whole")?;
    for scope in held.iter().filter(|s| !admitted.contains(*s)) {
        sqlx::query("DELETE FROM listed_whole WHERE scope = ?")
            .bind(scope)
            .execute(pool)
            .await
            .with_context(|| format!("forget scope {scope}"))?;
    }
    Ok(admitted
        .iter()
        .filter(|s| !held.contains(s))
        .cloned()
        .collect())
}

/// An enumeration of `scopes` reached its end. With `named`, every id
/// it listed, it covered the whole account, so a listed message it did
/// not name is gone. One transaction: the scopes are listed whole
/// exactly when what they did not name has been deleted. Returns how
/// many emails went.
pub async fn close_enumeration(
    pool: &SqlitePool,
    now: &IsoOffsetTimestamp,
    scopes: &[String],
    named: Option<&BTreeSet<String>>,
    email_id_of: EmailIdOf,
) -> Result<usize> {
    let mut tx = pool.begin().await.context("begin enumeration close tx")?;
    let mut gone = 0;
    if let Some(named) = named {
        let listed: Vec<String> = sqlx::query_scalar("SELECT id FROM listed_messages")
            .fetch_all(&mut *tx)
            .await
            .context("select the listed messages")?;
        let unnamed: Vec<String> = listed
            .iter()
            .filter(|id| !named.contains(*id))
            .cloned()
            .collect();
        gone = forget_in_tx(&mut tx, now, &unnamed, email_id_of).await?;
        datalib_etl::prune::record("listed messages", listed.len(), unnamed.len());
    }
    for scope in scopes {
        sqlx::query("INSERT INTO listed_whole (scope) VALUES (?) ON CONFLICT(scope) DO NOTHING")
            .bind(scope)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("record scope {scope} as listed whole"))?;
    }
    tx.commit().await.context("commit enumeration close tx")?;
    Ok(gone)
}

// ── the rungs that bring an older store here ────────────────────────

/// The tables the second rung made, as it made them. A rung is a record
/// of the day it was written: the third rung reshapes what this one
/// leaves.
const SECOND_RUNG_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS listed_messages (id TEXT PRIMARY KEY, stamp TEXT NULL)",
    "CREATE TABLE IF NOT EXISTS fetched_messages (
        id TEXT PRIMARY KEY,
        email_id TEXT NULL,
        fetched_for TEXT NULL,
        unstorable_by TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS listed_whole (scope TEXT PRIMARY KEY)",
];

/// Fills the listing from what an older store holds, so its tokens stay
/// good and nothing is fetched again: every message `gmail_messages`
/// mapped, and every email of an account with a JMAP state, is listed
/// and held under no stamp. A Gmail message an earlier run recorded as
/// unfetched is listed and not held, which makes it owed.
pub async fn migrate_from_cursors(conn: &mut SqliteConnection) -> Result<()> {
    let has = |table: &'static str| {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
        )
        .bind(table)
    };
    let listing_bookkeeping = dr::bookkeeping_ddl_for(LISTED);
    for ddl in SECOND_RUNG_DDL
        .iter()
        .copied()
        .chain([listing_bookkeeping.as_str()])
    {
        // Audited: this module's own DDL.
        sqlx::query(sqlx::AssertSqlSafe(ddl.to_string()))
            .execute(&mut *conn)
            .await?;
    }
    let mut steps: Vec<&str> = Vec::new();
    if has("gmail_messages").fetch_one(&mut *conn).await? {
        steps.extend([
            "INSERT INTO fetched_messages (id, email_id) SELECT gmail_id, email_id FROM gmail_messages",
            "INSERT INTO listed_messages (id) SELECT gmail_id FROM gmail_messages",
            "DROP TABLE gmail_messages",
        ]);
    }
    if has("problems").fetch_one(&mut *conn).await? {
        steps.extend([
            "INSERT OR IGNORE INTO listed_messages (id)
             SELECT substr(scope_key, length('record:gmail_messages:') + 1) FROM problems
             WHERE instr(scope_key, 'record:gmail_messages:') = 1",
            "DELETE FROM problems WHERE instr(scope_key, 'record:gmail_messages:') = 1",
        ]);
    }
    if has("sync_scope_state").fetch_one(&mut *conn).await? {
        steps.extend([
            "INSERT OR IGNORE INTO fetched_messages (id, email_id)
             SELECT e.id, e.id FROM emails e WHERE EXISTS (SELECT 1 FROM sync_scope_state s
                WHERE s.scope = 'jmap:' || e.account_id || ':state:Email')",
            "INSERT OR IGNORE INTO listed_messages (id)
             SELECT e.id FROM emails e WHERE EXISTS (SELECT 1 FROM sync_scope_state s
                WHERE s.scope = 'jmap:' || e.account_id || ':state:Email')",
            "DELETE FROM sync_scope_state
             WHERE scope LIKE 'jmap:%:state:Thread' OR scope LIKE 'gmail:%:unstorable:%'",
        ]);
    }
    if has("sync_scope_config").fetch_one(&mut *conn).await? {
        steps.push(
            "DELETE FROM sync_scope_config WHERE scope IN ('jmap:download', 'gmail:download')",
        );
    }
    for sql in steps {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Moves what `fetched_messages` held into the listing's sidecar: every
/// message it mapped to an email is held at the stamp it was fetched
/// for, as of when its email was fetched, with the attempts its sidecar
/// row already counts. A message a build could not store is held at
/// nothing, so it is owed once more.
pub async fn migrate_held_into_the_sidecar(conn: &mut SqliteConnection) -> Result<()> {
    let has_held_version: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('listed_messages_bookkeeping')
         WHERE name = 'held_version')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !has_held_version {
        sqlx::query("ALTER TABLE listed_messages_bookkeeping ADD COLUMN held_version TEXT NULL")
            .execute(&mut *conn)
            .await?;
    }
    let (now, _) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
    sqlx::query(
        "INSERT INTO listed_messages_bookkeeping (id, attempt_count, held_version, fetched_at_utc)
         SELECT f.id, 0, f.fetched_for, coalesce(e.fetched_at_utc, ?1)
         FROM fetched_messages f LEFT JOIN emails_bookkeeping e ON e.id = f.email_id
         WHERE f.email_id IS NOT NULL
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version,
            fetched_at_utc = excluded.fetched_at_utc",
    )
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DROP TABLE fetched_messages")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::full_ddl;
    use crate::ingest::RawDb;

    fn strings(db: &RawDb, sql: &'static str) -> impl std::future::Future<Output = Vec<String>> {
        let pool = db.pool().clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(&pool)
                .await
                .unwrap()
        }
    }

    async fn owed_ids(db: &RawDb) -> Vec<String> {
        owed::owed(db.pool(), LISTED, listing(db.pool(), false).await.unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.key)
            .collect()
    }

    /// A store an older build wrote climbs the whole ladder with its
    /// tokens intact and nothing to fetch again: what `gmail_messages`
    /// mapped and what a JMAP account's state covered are listed and
    /// held, a Gmail message recorded as unfetched is owed, an mbox
    /// import's email is left alone, and the state the old cursors kept
    /// beside the tokens is gone.
    #[tokio::test]
    async fn the_ladder_lists_what_an_older_store_holds() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("j.doltlite_db");
        {
            let mut ddl: Vec<String> = full_ddl()
                .into_iter()
                .filter(|sql| !sql.contains("listed_"))
                .collect();
            ddl.push(
                "CREATE TABLE gmail_messages (gmail_id TEXT PRIMARY KEY, \
                 email_id TEXT NOT NULL, thread_id TEXT NOT NULL)"
                    .to_string(),
            );
            let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
            let pool = dr::open(&path, &ddl).await.unwrap();
            for sql in [
                // The columns the first rung drops.
                "ALTER TABLE mailboxes ADD COLUMN total_emails INTEGER NULL",
                "ALTER TABLE mailboxes ADD COLUMN unread_emails INTEGER NULL",
                "INSERT INTO emails (id, payload, account_id, thread_id, blob_id) VALUES
                    ('M1', jsonb('{}'), 'A1', 'T1', 'B1'),
                    ('picard', jsonb('{}'), 'g@example.test', 'T2', 'B2'),
                    ('from-takeout', jsonb('{}'), 'takeout', 'T3', 'B3')",
                "INSERT INTO gmail_messages VALUES ('18c9', 'picard', 'T2')",
                "INSERT INTO sync_scope_state (scope, last_seen_at_utc) VALUES
                    ('jmap:A1:state:Email', 'email-7'),
                    ('jmap:A1:state:Thread', 'thread-7'),
                    ('gmail:g@example.test:historyId', '9001'),
                    ('gmail:g@example.test:unstorable:18ca', 'an older build')",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            let stop = datalib_etl::stop::StopFlag::new();
            datalib_etl::run_problems::collecting(&pool, &stop, |found| async move {
                found.record_failed("gmail_messages", "18cb", "HTTP 400");
                Ok(())
            })
            .await
            .unwrap();
            dr::commit_run(&pool, "an older build's rows")
                .await
                .unwrap();
            pool.close().await;
        }

        let db = RawDb::open(&path).await.expect("the rungs carry it");
        assert_eq!(
            strings(&db, "SELECT id FROM listed_messages ORDER BY id").await,
            ["18c9", "18cb", "M1"]
        );
        assert_eq!(
            strings(
                &db,
                "SELECT id || '=' || coalesce(held_version, '') FROM listed_messages_bookkeeping ORDER BY id"
            )
            .await,
            ["18c9=", "M1="],
            "held under no stamp, as listed under none"
        );
        assert_eq!(owed_ids(&db).await, ["18cb"]);
        assert_eq!(
            strings(&db, "SELECT scope FROM sync_scope_state ORDER BY scope").await,
            ["gmail:g@example.test:historyId", "jmap:A1:state:Email"]
        );
        assert!(strings(&db, "SELECT scope_key FROM problems")
            .await
            .is_empty());
        assert!(strings(
            &db,
            "SELECT name FROM sqlite_master WHERE name IN ('gmail_messages', 'fetched_messages')"
        )
        .await
        .is_empty());
        db.close().await;
    }

    /// A store at the second rung opens on the third with what it held
    /// in the sidecar: a message held at its stamp stays held, one held
    /// at an older stamp keeps the attempts counted since, one a build
    /// could not store and one never fetched are owed, and a sidecar
    /// written before it had `held_version` gets the column.
    #[tokio::test]
    async fn the_third_rung_carries_what_was_held_into_the_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("j.doltlite_db");
        {
            let mut ddl: Vec<String> = full_ddl()
                .into_iter()
                .filter(|sql| !sql.contains("listed_messages_bookkeeping"))
                .collect();
            ddl.extend(
                [
                    "CREATE TABLE listed_messages_bookkeeping (id TEXT PRIMARY KEY, \
                     fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, \
                     last_attempt_at_utc TEXT NULL, last_error TEXT NULL, \
                     volatile_payload TEXT NULL, tz_offset TEXT NULL)",
                    SECOND_RUNG_DDL[1],
                ]
                .map(str::to_string),
            );
            let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
            let pool = dr::open(&path, &ddl).await.unwrap();
            for sql in [
                "INSERT INTO listed_messages (id, stamp) VALUES
                    ('data', 'h2'), ('picard', 'h2'), ('riker', 'h2'), ('worf', 'h2')",
                "INSERT INTO fetched_messages (id, email_id, fetched_for, unstorable_by) VALUES
                    ('picard', 'picard', 'h2', NULL),
                    ('riker', 'riker', 'h1', NULL),
                    ('worf', NULL, 'h2', 'an older build')",
                "INSERT INTO listed_messages_bookkeeping (id, attempt_count, last_error)
                    VALUES ('riker', 3, 'HTTP 500')",
                "UPDATE _datalib_meta SET value = '2' WHERE key = 'schema_version'",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            dr::commit_run(&pool, "the second rung's rows")
                .await
                .unwrap();
            pool.close().await;
        }

        let db = RawDb::open(&path).await.expect("the third rung carries it");
        assert_eq!(
            strings(
                &db,
                "SELECT id || '=' || coalesce(held_version, '') || '/' || attempt_count
                 FROM listed_messages_bookkeeping ORDER BY id"
            )
            .await,
            ["picard=h2/0", "riker=h1/3"]
        );
        assert_eq!(owed_ids(&db).await, ["data", "riker", "worf"]);
        assert!(strings(
            &db,
            "SELECT name FROM sqlite_master WHERE name = 'fetched_messages'"
        )
        .await
        .is_empty());
        db.close().await;
    }
}
