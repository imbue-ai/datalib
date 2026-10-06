//! What upstream listed, what is held for it, and what is owed: the
//! state both API downloads (JMAP, Gmail) keep, and the queries over it.
//!
//! `listed_messages` has a row per message upstream named, keyed by
//! upstream's own id, with the token of the response that last named it
//! as changed. `fetched_messages` has a row per message we fetched: the
//! email row it produced and the token it was fetched for, written in
//! the transaction that writes the email. A message is owed when the two
//! disagree, and that is asked of the store each time, never stored.
//! `listed_whole` names the scopes (a mailbox, a label, or the account)
//! an enumeration has listed to its end. The rule and why:
//! docs/dev/plans/sync_state.md §2.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use super::db::{delete_emails_in_tx, refresh_email_joins};
use super::schema_raw::{EmailRow, ThreadRow};

pub const LISTED: &str = "listed_messages";

pub const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS listed_messages (id TEXT PRIMARY KEY, stamp TEXT NULL)",
    "CREATE TABLE IF NOT EXISTS fetched_messages (
        id TEXT PRIMARY KEY,
        email_id TEXT NULL,
        fetched_for TEXT NULL,
        unstorable_by TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS listed_whole (scope TEXT PRIMARY KEY)",
];

/// The scope of an enumeration no filter narrows.
pub const WHOLE_ACCOUNT: &str = "*";

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

/// A listed message to fetch, and the stamp the fetch will satisfy.
#[derive(Debug, Clone)]
pub struct Owed {
    pub id: String,
    pub stamp: Option<String>,
}

const NOT_HELD_AT_ITS_STAMP: &str = "SELECT l.id, l.stamp FROM listed_messages l
     LEFT JOIN fetched_messages f ON f.id = l.id
     LEFT JOIN emails e ON e.id = f.email_id
     WHERE (f.id IS NULL OR f.fetched_for IS NOT l.stamp";

/// Every listed message not held at its listed stamp, in id order.
pub async fn owed(pool: &SqlitePool) -> Result<Vec<Owed>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Audited: two constants.
        "{NOT_HELD_AT_ITS_STAMP}) ORDER BY l.id"
    )))
    .fetch_all(pool)
    .await
    .context("select the messages owed")?;
    Ok(rows
        .into_iter()
        .map(|(id, stamp)| Owed { id, stamp })
        .collect())
}

/// [`owed`] for a download whose fetch brings the body with the message
/// (Gmail), newest id first. Also owed: a held message whose `.eml` is
/// not stored and fits under `cap`, and one a build other than `build`
/// could not store. One this build could not store is not asked for
/// again until it is listed anew: its bytes do not change.
pub async fn owed_with_bodies(
    pool: &SqlitePool,
    build: &str,
    cap: Option<u64>,
) -> Result<Vec<Owed>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Audited: constants; the build and the cap are bound.
        "{NOT_HELD_AT_ITS_STAMP}
            OR (f.unstorable_by IS NOT NULL AND f.unstorable_by != ?1)
            OR (f.unstorable_by IS NULL AND e.id IS NOT NULL
                AND (?2 IS NULL OR e.size <= ?2)
                AND NOT EXISTS (SELECT 1 FROM email_blobs b
                                WHERE b.blob_id = e.blob_id AND b.blake3 IS NOT NULL)))
         ORDER BY l.id DESC"
    )))
    .bind(build)
    .bind(cap.map(|c| c as i64))
    .fetch_all(pool)
    .await
    .context("select the messages owed")?;
    Ok(rows
        .into_iter()
        .map(|(id, stamp)| Owed { id, stamp })
        .collect())
}

/// Whether upstream has listed anything.
pub async fn lists_any(pool: &SqlitePool) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM listed_messages)")
        .fetch_one(pool)
        .await
        .context("ask whether any message is listed")
}

/// Whether anything upstream listed has been fetched.
pub async fn holds_any(pool: &SqlitePool) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fetched_messages WHERE email_id IS NOT NULL)")
        .fetch_one(pool)
        .await
        .context("ask whether any message is held")
}

/// A fetched message: upstream's id for it, the stamp the fetch
/// satisfies, and the email row it produced.
pub struct Held {
    pub id: String,
    pub stamp: Option<String>,
    pub row: EmailRow,
}

/// Write fetched messages: the email rows and their joins, what is held
/// for each, and the thread rows they belong to.
pub async fn hold_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    held: Vec<Held>,
) -> Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    let mut rows = Vec::with_capacity(held.len());
    let mut threads = BTreeSet::new();
    for h in held {
        sqlx::query(
            "INSERT INTO fetched_messages (id, email_id, fetched_for, unstorable_by)
             VALUES (?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET email_id = excluded.email_id,
                fetched_for = excluded.fetched_for, unstorable_by = NULL",
        )
        .bind(&h.id)
        .bind(h.row.id())
        .bind(&h.stamp)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("hold message {}", h.id))?;
        forget_attempts_in_tx(tx, &h.id).await?;
        threads.insert((h.row.account_id.clone(), h.row.thread_id.clone()));
        rows.push(h.row);
    }
    bulk_upsert_in_tx(tx, &rows, now).await?;
    for row in &rows {
        refresh_email_joins(tx, row).await?;
    }
    rebuild_threads_in_tx(tx, now, &threads).await
}

/// Upstream no longer has these messages, or a fetch found them outside
/// what the download mirrors: the listing, what is held and the email
/// rows go together. Returns how many emails went.
pub async fn forget_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    ids: &[String],
) -> Result<usize> {
    let mut emails = Vec::new();
    let mut threads = BTreeSet::new();
    for id in ids {
        let held: Option<(String, String, String)> = sqlx::query_as(
            "SELECT e.id, e.account_id, e.thread_id FROM fetched_messages f
             JOIN emails e ON e.id = f.email_id WHERE f.id = ?",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .with_context(|| format!("look up what is held for message {id}"))?;
        for sql in [
            "DELETE FROM fetched_messages WHERE id = ?",
            "DELETE FROM listed_messages WHERE id = ?",
        ] {
            sqlx::query(sql)
                .bind(id)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("forget message {id}"))?;
        }
        forget_attempts_in_tx(tx, id).await?;
        if let Some((email_id, account_id, thread_id)) = held {
            emails.push(email_id);
            threads.insert((account_id, thread_id));
        }
    }
    delete_emails_in_tx(tx, &emails).await?;
    rebuild_threads_in_tx(tx, now, &threads).await?;
    Ok(emails.len())
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

/// A fetch of these listed messages failed. They stay owed; this counts
/// the attempt and gives each a `problems` row, which the fetch that
/// works clears.
pub async fn record_failures(pool: &SqlitePool, ids: &[String], err: &str) -> Result<()> {
    let mut tx = pool.begin().await.context("begin fetch failure tx")?;
    for id in ids {
        dr::record_object_error(&mut tx, LISTED, id, err).await?;
    }
    tx.commit().await.context("commit fetch failure tx")
}

/// The message fetched and this build could not make a row of it. That
/// is an answer for its listed stamp: it is not asked for again by this
/// build until it is listed anew. An email an earlier fetch produced
/// stays.
pub async fn mark_unstorable(pool: &SqlitePool, owed: &Owed, build: &str, err: &str) -> Result<()> {
    let mut tx = pool.begin().await.context("begin unstorable tx")?;
    sqlx::query(
        "INSERT INTO fetched_messages (id, email_id, fetched_for, unstorable_by)
         VALUES (?, NULL, ?, ?)
         ON CONFLICT(id) DO UPDATE SET fetched_for = excluded.fetched_for,
            unstorable_by = excluded.unstorable_by",
    )
    .bind(&owed.id)
    .bind(&owed.stamp)
    .bind(build)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("mark message {} unstorable", owed.id))?;
    dr::record_object_error(&mut tx, LISTED, &owed.id, err).await?;
    tx.commit().await.context("commit unstorable tx")
}

async fn forget_attempts_in_tx(tx: &mut Transaction<'_, Sqlite>, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM listed_messages_bookkeeping WHERE id = ?")
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget the attempts on message {id}"))?;
    sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?")
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(format!("{LISTED}:{id}"))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget the problem of message {id}"))?;
    Ok(())
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
        sqlx::query("UPDATE fetched_messages SET fetched_for = NULL")
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
        gone = forget_in_tx(&mut tx, now, &unnamed).await?;
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

// ── the rung that brings an older store here ────────────────────────

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
    for ddl in DDL.iter().copied().chain([listing_bookkeeping.as_str()]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::full_ddl;
    use crate::ingest::RawDb;

    /// A store an older build wrote opens on the second rung with its
    /// tokens intact and nothing to fetch again: what `gmail_messages`
    /// mapped and what a JMAP account's state covered are listed and
    /// held, a Gmail message recorded as unfetched is owed, an mbox
    /// import's email is left alone, and the state the old cursors kept
    /// beside the tokens is gone.
    #[tokio::test]
    async fn the_second_rung_lists_what_an_older_store_holds() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("j.doltlite_db");
        {
            let mut ddl: Vec<String> = full_ddl()
                .into_iter()
                .filter(|sql| !sql.contains("listed_") && !sql.contains("fetched_messages"))
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
        let strings = |sql: &'static str| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_scalar::<_, String>(sql)
                    .fetch_all(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            strings("SELECT id FROM listed_messages ORDER BY id").await,
            ["18c9", "18cb", "M1"]
        );
        assert_eq!(
            strings("SELECT id || '=' || email_id FROM fetched_messages ORDER BY id").await,
            ["18c9=picard", "M1=M1"]
        );
        let owed: Vec<String> = owed(db.pool())
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(owed, ["18cb"]);
        assert_eq!(
            strings("SELECT scope FROM sync_scope_state ORDER BY scope").await,
            ["gmail:g@example.test:historyId", "jmap:A1:state:Email"]
        );
        assert!(strings("SELECT scope_key FROM problems").await.is_empty());
        assert!(
            strings("SELECT name FROM sqlite_master WHERE name = 'gmail_messages'")
                .await
                .is_empty()
        );
        db.close().await;
    }
}
