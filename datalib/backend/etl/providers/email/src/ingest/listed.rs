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
//! an enumeration has listed to its end. `thread_members` (JMAP, Gmail)
//! has a row per message in the account, mirrored or not, naming its
//! thread: what a walk of the whole account and `history.list` say,
//! which is how whole threads are listed and how many messages Gmail
//! has in each is counted. The rule and why:
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
    THREAD_MEMBERS_DDL[0],
    THREAD_MEMBERS_DDL[1],
];

/// Keyed by upstream's message id; `thread_id` is the `threads` row's.
const THREAD_MEMBERS_DDL: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS thread_members (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS thread_members_by_thread ON thread_members(thread_id)",
];

/// The scope of the walk that fills `thread_members`. A label id has no
/// `#`, so it names no label.
pub const THREAD_INDEX: &str = "#threads";

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

/// Returns how many rows it wrote: for [`Named::Exists`], how many
/// messages were not listed before.
pub async fn list_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    ids: &[String],
    stamp: Option<&str>,
    named: Named,
) -> Result<u64> {
    let sql = match named {
        Named::Changed => {
            "INSERT INTO listed_messages (id, stamp) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET stamp = excluded.stamp"
        }
        Named::Exists => {
            "INSERT INTO listed_messages (id, stamp) VALUES (?, ?) ON CONFLICT(id) DO NOTHING"
        }
    };
    let mut wrote = 0;
    for id in ids {
        wrote += sqlx::query(sql)
            .bind(id)
            .bind(stamp)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list message {id}"))?
            .rows_affected();
    }
    Ok(wrote)
}

/// Record which thread each of `members` (upstream's message id, the
/// `threads` row's id) is in.
pub async fn member_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    members: &[(String, String)],
) -> Result<()> {
    for (id, thread_id) in members {
        sqlx::query(
            "INSERT INTO thread_members (id, thread_id) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET thread_id = excluded.thread_id",
        )
        .bind(id)
        .bind(thread_id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("record the thread of message {id}"))?;
    }
    Ok(())
}

pub async fn unmember_in_tx(tx: &mut Transaction<'_, Sqlite>, ids: &[String]) -> Result<()> {
    for id in ids {
        sqlx::query("DELETE FROM thread_members WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("forget the thread of message {id}"))?;
    }
    Ok(())
}

/// A walk of the whole account for [`THREAD_INDEX`] reached its end,
/// naming `named`: a message it did not name is in no thread, and the
/// index is whole. One transaction. Returns how many members went.
pub async fn close_thread_index(pool: &SqlitePool, named: &BTreeSet<String>) -> Result<usize> {
    let mut tx = pool.begin().await.context("begin thread index close tx")?;
    let held: Vec<String> = sqlx::query_scalar("SELECT id FROM thread_members")
        .fetch_all(&mut *tx)
        .await
        .context("select the thread members")?;
    let gone: Vec<String> = held
        .iter()
        .filter(|id| !named.contains(*id))
        .cloned()
        .collect();
    unmember_in_tx(&mut tx, &gone).await?;
    sqlx::query("INSERT INTO listed_whole (scope) VALUES (?) ON CONFLICT(scope) DO NOTHING")
        .bind(THREAD_INDEX)
        .execute(&mut *tx)
        .await
        .context("record the thread index as listed whole")?;
    tx.commit().await.context("commit thread index close tx")?;
    datalib_etl::prune::record("thread members", held.len(), gone.len());
    Ok(gone.len())
}

/// What a label filter keeps an email for, in the store's terms.
#[derive(Debug, Clone, Copy)]
pub enum Keeps<'a> {
    /// The Gmail API: it carries one of these label ids
    /// (`emails.label_ids`). Its listing is keyed by `gmail_message_id`.
    GmailLabels(&'a [String]),
    /// JMAP: it is filed under one of these mailboxes. Its listing is
    /// keyed by its own id.
    Mailboxes(&'a [String]),
}

impl Keeps<'_> {
    fn ids(&self) -> Result<String> {
        let (Keeps::GmailLabels(ids) | Keeps::Mailboxes(ids)) = self;
        Ok(serde_json::to_string(ids)?)
    }

    /// True for an email `e` the filter keeps, the ids bound as `?2`.
    fn kept_sql(&self) -> &'static str {
        match self {
            Keeps::GmailLabels(_) => {
                "EXISTS (SELECT 1 FROM json_each(e.label_ids)
                         WHERE value IN (SELECT value FROM json_each(?2)))"
            }
            Keeps::Mailboxes(_) => {
                "EXISTS (SELECT 1 FROM email_mailboxes em WHERE em.email_id = e.id
                         AND em.mailbox_id IN (SELECT value FROM json_each(?2)))"
            }
        }
    }

    /// Email `e`'s key in `listed_messages`; NULL where it has none.
    fn listing_key_sql(&self) -> &'static str {
        match self {
            Keeps::GmailLabels(_) => "e.gmail_message_id",
            Keeps::Mailboxes(_) => "e.id",
        }
    }

    /// True when what `e` carries is known: a row the Gmail API has not
    /// answered for yet (a Takeout's) has no `label_ids`.
    fn known_sql(&self) -> &'static str {
        match self {
            Keeps::GmailLabels(_) => "e.label_ids IS NOT NULL AND e.gmail_message_id IS NOT NULL",
            Keeps::Mailboxes(_) => "1",
        }
    }
}

/// The threads of `account_id` holding an email the filter keeps.
pub async fn kept_threads(
    pool: &SqlitePool,
    account_id: &str,
    keeps: Keeps<'_>,
) -> Result<std::collections::HashSet<String>> {
    // Audited: the clause is one of `Keeps`'s literals; values are bound.
    let sql = format!(
        "SELECT DISTINCT e.thread_id FROM emails e WHERE e.account_id = ?1 AND {}",
        keeps.kept_sql()
    );
    let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .bind(keeps.ids()?)
        .fetch_all(pool)
        .await
        .context("select the threads the filter keeps")?;
    Ok(rows.into_iter().collect())
}

/// List, at `stamp`, every member of a thread of `account_id` that holds
/// an email the filter keeps, and is not listed: the rest of each kept
/// message's thread. Returns how many it listed.
pub async fn list_kept_threads(
    pool: &SqlitePool,
    account_id: &str,
    keeps: Keeps<'_>,
    stamp: Option<&str>,
) -> Result<u64> {
    // Audited: the clause is one of `Keeps`'s literals; values are bound.
    let sql = format!(
        "INSERT INTO listed_messages (id, stamp)
         SELECT m.id, ?3 FROM thread_members m
         WHERE m.thread_id IN (SELECT e.thread_id FROM emails e WHERE e.account_id = ?1 AND {})
         AND NOT EXISTS (SELECT 1 FROM listed_messages l WHERE l.id = m.id)
         ON CONFLICT(id) DO NOTHING",
        keeps.kept_sql()
    );
    let listed = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .bind(keeps.ids()?)
        .bind(stamp)
        .execute(pool)
        .await
        .context("list the rest of the kept threads")?;
    Ok(listed.rows_affected())
}

/// A held thread of `account_id` none of whose emails the filter keeps
/// any more goes whole. A thread with an email still owed waits for it:
/// its labels may be what keeps the thread. Returns how many emails went.
pub async fn drop_unkept_threads(
    pool: &SqlitePool,
    now: &IsoOffsetTimestamp,
    account_id: &str,
    keeps: Keeps<'_>,
    email_id_of: EmailIdOf,
) -> Result<usize> {
    let key = keeps.listing_key_sql();
    // Audited: the clauses are `Keeps`'s literals; values are bound.
    let sql = format!(
        "SELECT e.thread_id, {key}, l.stamp, l.id IS NOT NULL
         FROM emails e LEFT JOIN listed_messages l ON l.id = {key}
         WHERE e.account_id = ?1 AND e.thread_id IN (
            SELECT e.thread_id FROM emails e WHERE e.account_id = ?1 GROUP BY e.thread_id
            HAVING min({known}) = 1 AND max({kept}) = 0)",
        known = keeps.known_sql(),
        kept = keeps.kept_sql(),
    );
    let rows: Vec<(String, String, Option<String>, bool)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(account_id)
            .bind(keeps.ids()?)
            .fetch_all(pool)
            .await
            .context("select the threads the filter no longer keeps")?;
    let listing: Vec<Listed> = rows
        .iter()
        .filter(|(_, _, _, listed)| *listed)
        .map(|(_, id, stamp, _)| Listed::new(id.clone(), stamp.clone()))
        .collect();
    let waiting: BTreeSet<String> = owed::owed(pool, LISTED, listing)
        .await?
        .into_iter()
        .map(|l| l.key)
        .collect();
    let waiting_threads: BTreeSet<&str> = rows
        .iter()
        .filter(|(_, id, _, _)| waiting.contains(id))
        .map(|(thread, ..)| thread.as_str())
        .collect();
    let gone: Vec<String> = rows
        .iter()
        .filter(|(thread, ..)| !waiting_threads.contains(thread.as_str()))
        .map(|(_, id, ..)| id.clone())
        .collect();
    if gone.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await.context("begin unkept threads tx")?;
    let n = forget_in_tx(&mut tx, now, &gone, email_id_of).await?;
    tx.commit().await.context("commit unkept threads tx")?;
    Ok(n)
}

/// Rewrite every thread whose count of upstream messages the thread
/// index now says otherwise. Nothing while the index is not whole.
/// Returns how many threads it rewrote.
pub async fn recount_threads(pool: &SqlitePool, now: &IsoOffsetTimestamp) -> Result<usize> {
    let mut tx = pool.begin().await.context("begin recount tx")?;
    if !thread_index_whole(&mut tx).await? {
        tx.commit().await.context("commit recount tx")?;
        return Ok(0);
    }
    let stale: BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT account_id, id FROM threads t WHERE upstream_email_count IS NOT
            (SELECT nullif(count(*), 0) FROM thread_members m WHERE m.thread_id = t.id)",
    )
    .fetch_all(&mut *tx)
    .await
    .context("select the threads to recount")?
    .into_iter()
    .collect();
    rebuild_threads_in_tx(&mut tx, now, &stale).await?;
    tx.commit().await.context("commit recount tx")?;
    Ok(stale.len())
}

/// Whether a walk of the account has filled `thread_members` to its end.
pub async fn thread_index_is_whole(pool: &SqlitePool) -> Result<bool> {
    let mut tx = pool.begin().await.context("begin thread index read")?;
    let whole = thread_index_whole(&mut tx).await?;
    tx.commit().await.context("end thread index read")?;
    Ok(whole)
}

async fn thread_index_whole(tx: &mut Transaction<'_, Sqlite>) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM listed_whole WHERE scope = ?)")
        .bind(THREAD_INDEX)
        .fetch_one(&mut **tx)
        .await
        .context("ask whether the thread index is whole")
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
/// Its upstream count is the thread index's while that is whole, and
/// what it was before while not.
pub(super) async fn rebuild_threads_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    now: &IsoOffsetTimestamp,
    threads: &BTreeSet<(String, String)>,
) -> Result<()> {
    let mut rows = Vec::new();
    for (account_id, thread_id) in threads {
        let sql = if thread_index_whole(tx).await? {
            "SELECT nullif(count(*), 0) FROM thread_members WHERE thread_id = ?"
        } else {
            "SELECT upstream_email_count FROM threads WHERE id = ?"
        };
        let upstream: Option<i64> = sqlx::query_scalar(sql)
            .bind(thread_id)
            .fetch_optional(&mut **tx)
            .await
            .with_context(|| format!("count thread {thread_id}"))?
            .flatten();
        match thread_row_in_tx(tx, account_id, thread_id, upstream).await? {
            Some(row) => rows.push(row),
            None => {
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
            }
        }
    }
    bulk_upsert_in_tx(tx, &rows, now).await
}

async fn thread_row_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    thread_id: &str,
    upstream_email_count: Option<i64>,
) -> Result<Option<ThreadRow>> {
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
        return Ok(None);
    }
    let mut payload = json!({ "id": thread_id, "emailIds": email_ids });
    if let Some(count) = upstream_email_count {
        payload["upstreamEmailCount"] = json!(count);
    }
    Ok(Some(ThreadRow::from_jmap_payload(
        thread_id, account_id, &payload,
    )?))
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
/// not name is gone; with `rows_of` too, so is any email row of that
/// account it did not name, listed or not (a Takeout's). One
/// transaction: the scopes are listed whole exactly when what they did
/// not name has been deleted. Returns how many emails went.
pub async fn close_enumeration(
    pool: &SqlitePool,
    now: &IsoOffsetTimestamp,
    scopes: &[String],
    named: Option<&BTreeSet<String>>,
    rows_of: Option<&str>,
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
        if let Some(account_id) = rows_of {
            let named_rows: BTreeSet<String> = named.iter().map(|id| email_id_of(id)).collect();
            let rows: Vec<String> =
                sqlx::query_scalar("SELECT id FROM emails WHERE account_id = ?")
                    .bind(account_id)
                    .fetch_all(&mut *tx)
                    .await
                    .context("select the account's emails")?;
            let unnamed_rows: Vec<String> = rows
                .iter()
                .filter(|id| !named_rows.contains(*id))
                .cloned()
                .collect();
            gone += write_batch_in_tx(&mut tx, now, Vec::new(), &unnamed_rows, email_id_of).await?;
            datalib_etl::prune::record("unlisted emails", rows.len(), unnamed_rows.len());
        }
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

/// Builds before this rung compared `only_extract_labels` written as
/// Gmail's ids (`INBOX`) with the labels' names (`Inbox`), so a walk
/// listed a label's mail and the fetch then dropped every message, its
/// listing with it, while the label stayed listed whole. Forgetting
/// which labels were listed whole has the next run walk them again,
/// which lists what was dropped; what is held stays held. A JMAP store
/// is left alone.
pub async fn migrate_relist_gmail_scopes(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query(
        "DELETE FROM listed_whole WHERE EXISTS (SELECT 1 FROM sync_scope_state
            WHERE scope LIKE 'gmail:%:historyId')",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Gmail's own ids and labels go on each email the API answered for,
/// read from what its row already holds; a thread gets a column for how
/// many messages Gmail has in it, and the thread index its table, which
/// the next Gmail run fills with one walk of the account.
pub async fn migrate_gmail_ids_and_thread_counts(conn: &mut SqliteConnection) -> Result<()> {
    for (table, column, decl) in [
        ("emails", "gmail_message_id", "TEXT NULL"),
        ("emails", "gmail_thread_id", "TEXT NULL"),
        ("emails", "label_ids", "TEXT NULL"),
        ("threads", "upstream_email_count", "INTEGER NULL"),
    ] {
        let has: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = ?)")
                .bind(table)
                .bind(column)
                .fetch_one(&mut *conn)
                .await?;
        if !has {
            // Audited: names and declarations are the literals above.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {table} ADD COLUMN {column} {decl}"
            )))
            .execute(&mut *conn)
            .await?;
        }
    }
    for ddl in THREAD_MEMBERS_DDL {
        sqlx::query(ddl).execute(&mut *conn).await?;
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, account_id, json(payload) FROM emails WHERE payload ->> '$._source.via' = ?",
    )
    .bind(super::schema_raw::GMAIL_API_VIA)
    .fetch_all(&mut *conn)
    .await?;
    for (id, account_id, payload) in rows {
        let envelope: serde_json::Value = serde_json::from_str(&payload)
            .with_context(|| format!("parse the payload of email {id}"))?;
        let Some(row) = EmailRow::from_jmap_envelope(&account_id, &envelope) else {
            continue;
        };
        sqlx::query(
            "UPDATE emails SET gmail_message_id = ?, gmail_thread_id = ?, label_ids = ?
             WHERE id = ?",
        )
        .bind(&row.gmail_message_id)
        .bind(&row.gmail_thread_id)
        .bind(&row.label_ids)
        .bind(&id)
        .execute(&mut *conn)
        .await?;
    }
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

    /// The fifth rung has a Gmail store walk its labels again, and keeps
    /// what it holds; a JMAP store's mailboxes stay listed whole.
    #[tokio::test]
    async fn the_fifth_rung_walks_a_gmail_stores_labels_again() {
        let d = tempfile::tempdir().unwrap();
        for (token_scope, whole_after) in [
            ("gmail:g@example.test:historyId", &[][..]),
            ("jmap:A1:state:Email", &["mb-1"][..]),
        ] {
            let path = d.path().join(format!("{}.doltlite_db", &token_scope[..4]));
            {
                let ddl = full_ddl();
                let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
                let pool = dr::open(&path, &ddl).await.unwrap();
                for sql in [
                    "INSERT INTO listed_whole (scope) VALUES ('mb-1')",
                    "INSERT INTO listed_messages (id, stamp) VALUES ('picard', NULL)",
                    "INSERT INTO listed_messages_bookkeeping (id, attempt_count, fetched_at_utc)
                        VALUES ('picard', 0, '2369-04-14T00:00:00')",
                    "UPDATE _datalib_meta SET value = '4' WHERE key = 'schema_version'",
                ] {
                    sqlx::query(sql).execute(&pool).await.unwrap();
                }
                sqlx::query(
                    "INSERT INTO sync_scope_state (scope, last_seen_at_utc) VALUES (?, '7')",
                )
                .bind(token_scope)
                .execute(&pool)
                .await
                .unwrap();
                dr::commit_run(&pool, "the fourth rung's rows")
                    .await
                    .unwrap();
                pool.close().await;
            }

            let db = RawDb::open(&path).await.expect("the fifth rung carries it");
            assert_eq!(
                strings(&db, "SELECT scope FROM listed_whole").await,
                whole_after,
                "{token_scope}"
            );
            assert!(owed_ids(&db).await.is_empty(), "{token_scope}");
            db.close().await;
        }
    }

    /// The sixth rung puts Gmail's ids and labels on an email the API
    /// answered for, read from its payload, and leaves an mbox email's
    /// columns empty; the thread index is there to be walked.
    #[tokio::test]
    async fn the_sixth_rung_reads_gmails_ids_and_labels_off_each_email() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("g.doltlite_db");
        {
            let ddl = full_ddl();
            let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
            let pool = dr::open(&path, &ddl).await.unwrap();
            for sql in [
                "ALTER TABLE emails DROP COLUMN gmail_message_id",
                "ALTER TABLE emails DROP COLUMN gmail_thread_id",
                "ALTER TABLE emails DROP COLUMN label_ids",
                "ALTER TABLE threads DROP COLUMN upstream_email_count",
                "DROP TABLE thread_members",
                "INSERT INTO emails (id, payload, account_id, thread_id, blob_id) VALUES
                    ('18c9f2a1b2c3d901', jsonb('{
                        \"id\": \"18c9f2a1b2c3d901\", \"blobId\": \"B1\",
                        \"threadId\": \"1786225503531948289\",
                        \"mailboxIds\": {\"gmail:g@example.test:INBOX\": true},
                        \"keywords\": {\"$flagged\": true},
                        \"_source\": {\"via\": \"gmail.googleapis.com\",
                            \"gmailMessageId\": \"18c9f2a1b2c3d901\",
                            \"gmailThreadId\": \"18c9f2a1b2c3d901\"}}'),
                     'g@example.test', '1786225503531948289', 'B1'),
                    ('picard@enterprise.test', jsonb('{
                        \"id\": \"picard@enterprise.test\", \"blobId\": \"B2\",
                        \"threadId\": \"T2\", \"mailboxIds\": {}, \"keywords\": {}}'),
                     'takeout', 'T2', 'B2')",
                "INSERT INTO threads (id, payload, account_id, email_count) VALUES
                    ('1786225503531948289', jsonb('{}'), 'g@example.test', 1),
                    ('T2', jsonb('{}'), 'takeout', 1)",
                "UPDATE _datalib_meta SET value = '5' WHERE key = 'schema_version'",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            dr::commit_run(&pool, "the fifth rung's rows")
                .await
                .unwrap();
            pool.close().await;
        }

        let db = RawDb::open(&path).await.expect("the sixth rung carries it");
        assert_eq!(
            strings(
                &db,
                "SELECT id || '=' || coalesce(gmail_message_id, '-') || '/'
                    || coalesce(gmail_thread_id, '-') || '/' || coalesce(label_ids, '-')
                 FROM emails ORDER BY id"
            )
            .await,
            [
                r#"18c9f2a1b2c3d901=18c9f2a1b2c3d901/18c9f2a1b2c3d901/["INBOX","STARRED","UNREAD"]"#,
                "picard@enterprise.test=-/-/-",
            ]
        );
        assert!(strings(&db, "SELECT id FROM thread_members")
            .await
            .is_empty());
        db.close().await;
    }
}
