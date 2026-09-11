//! WhatsApp render — thin adapter over
//! [`datalib_etl_chat_common::render::render_all`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;

use datalib_etl::doltlite_raw;
use datalib_etl::progress::Progress;
use datalib_etl::render_cursor;
use datalib_etl_chat_common::{
    render::{RenderProfile, RenderSummary, ENTITY_KIND_CONVERSATION},
    NormalizedChat,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::providers::Provider;

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing WhatsApp doc rebuilt. v6: the raw store
/// is msgstore mirrored table for table, and the render cursor of a
/// store written the old way (`wa_*` tables) names a commit nothing can
/// diff against.
pub const RENDER_VERSION: u32 = 6;

const SOURCE_LABEL: &str = "WhatsApp";

fn profile() -> RenderProfile {
    RenderProfile {
        when_ts_precision: datalib_etl_chat_common::WhenTsPrecision::Seconds,
        provider: Provider::Whatsapp,
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "WhatsApp Chat".to_string(),
        message_kind: "WhatsApp Message".to_string(),
        reaction_kind: "WhatsApp Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

/// Render every chat. `raw_dir` is the source's `input_path` — same
/// value render's `parse` walks — used here only to find the
/// doltlite db for the dolt_diff render-cursor scan. Attachment bytes
/// arrive pre-loaded in `blobs_by_chat`, which parse hydrates from the
/// sibling CAS in the same call that built `chats`.
#[allow(clippy::too_many_arguments)]
pub fn render_all(
    chats: &[NormalizedChat],
    blobs_by_chat: &HashMap<String, BlobBundle>,
    raw_dir: &Path,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    _prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    // Chat JIDs the diff named that no `chat` row still carries. The
    // scan happens in here rather than in `parse`, so the caller learns
    // about them the same way it learns about documents.
    on_chat_gone: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<RenderSummary> {
    // Incremental gate: if a render cursor exists at the root of this
    // source's render directory, ask doltlite which chats changed
    // between that hash and HEAD. Skip the rest. Cold start (no cursor)
    // or no doltlite db on disk renders every chat.
    let cursor_path = render_cursor::cursor_path(out_dir, source_id);
    let prior = render_cursor::read_for_params(&cursor_path, &render_cursor::no_params())?;
    let db_path = doltlite_raw::db_path_for(raw_dir);

    let (filtered_owned, new_head): (Option<Vec<NormalizedChat>>, Option<String>) = if db_path
        .exists()
    {
        let last = prior.as_ref().map(|c| c.last_rendered_hash.as_str());
        let scan = tokio::task::block_in_place(|| match tokio::runtime::Handle::try_current() {
            Ok(h) => h.block_on(scan_diff(&db_path, last)),
            Err(_) => tokio::runtime::Runtime::new()?.block_on(scan_diff(&db_path, last)),
        })?;
        let filtered = scan.changed.as_ref().map(|c| {
            chats
                .iter()
                .filter(|chat| c.live.contains(&chat.id))
                .cloned()
                .collect::<Vec<_>>()
        });
        tracing::info!(
            source = source_id,
            scan_elapsed_ms = scan.elapsed.map(|d| d.as_millis() as u64),
            changed_chats = scan
                .changed
                .as_ref()
                .map(|c| c.live.len() as i64)
                .unwrap_or(-1),
            gone_chats = scan
                .changed
                .as_ref()
                .map(|c| c.gone.len() as i64)
                .unwrap_or(-1),
            cold_start = scan.changed.is_none(),
            "[render] whatsapp dolt_diff scan"
        );
        // Every backup is a full snapshot (the mirror drops and refills),
        // so a chat the diff named that HEAD no longer carries is one the
        // phone deleted.
        if let Some(c) = &scan.changed {
            for jid in &c.gone {
                on_chat_gone(jid)?;
            }
        }
        (filtered, scan.head)
    } else {
        (None, None)
    };
    let to_render: &[NormalizedChat] = filtered_owned.as_deref().unwrap_or(chats);

    let empty_fingerprints: HashMap<String, String> = HashMap::new();
    let summary = datalib_etl_chat_common::render::render_all(
        &profile(),
        to_render,
        out_dir,
        source_id,
        blobs_by_chat,
        progress,
        &empty_fingerprints,
        on_doc_complete,
    )?;

    if let Some(head) = new_head {
        render_cursor::write(&cursor_path, &head, &render_cursor::no_params())?;
    }
    Ok(summary)
}

/// What changed between two commits, as chat JIDs: the ones HEAD still
/// carries (re-render) and the ones it does not (gone upstream).
#[derive(Debug, Default)]
struct ChangedChats {
    live: HashSet<String>,
    gone: Vec<String>,
}

struct DiffScan {
    /// `None` is "no filter — render everything" (cold start).
    changed: Option<ChangedChats>,
    /// `None` means HEAD could not be read (non-doltlite sqlite); the
    /// cursor stays unwritten and next run is another cold start.
    head: Option<String>,
    elapsed: Option<std::time::Duration>,
}

/// The tables whose rows carry a `chat_row_id`, and the ones that reach
/// a chat through a `message` or `message_add_on` rowid. Every mirrored
/// table is a rowid graph, so "which chat did this row belong to" is a
/// walk up that graph — at HEAD for a row that still exists, at the
/// previous commit for one that was removed.
const CHAT_ROWID_TABLES: &[&str] = &["message", "message_add_on"];
const MESSAGE_ROWID_TABLES: &[&str] = &["message_text", "message_media"];

/// Ask doltlite what changed since `last_hash`, resolved to chat JIDs.
async fn scan_diff(db_path: &Path, last_hash: Option<&str>) -> Result<DiffScan> {
    let pool = datalib_etl::doltlite_raw::open_reader(db_path).await?;
    let head = datalib_etl::pin::head(&pool).await?;

    // Both refs, or neither: with no commit to scan *to* there is nothing
    // committed to diff against, and cold-starting is the only honest answer.
    // Scanning to the sampled hash rather than to the symbolic `HEAD` keeps
    // this diff and the reads that follow it naming one commit even while a
    // producer is still committing — see `datalib_etl::pin`.
    let scan = match (last_hash, head.as_ref()) {
        (Some(from), Some(to)) => {
            let from = datalib_etl::pin::Pin::at(from).context("render cursor")?;
            let started = std::time::Instant::now();
            let changed = changed_chats(&pool, &from, to).await?;
            DiffScan {
                changed: Some(changed),
                head: Some(to.commit().to_string()),
                elapsed: Some(started.elapsed()),
            }
        }
        _ => DiffScan {
            changed: None,
            head: head.as_ref().map(|p| p.commit().to_string()),
            elapsed: None,
        },
    };
    pool.close().await;
    Ok(scan)
}

type Pin = datalib_etl::pin::Pin;

async fn changed_chats(pool: &sqlx::SqlitePool, from: &Pin, to: &Pin) -> Result<ChangedChats> {
    let mut chat_rowids: HashSet<i64> = HashSet::new();

    chat_rowids.extend(diff_column::<i64>(pool, "chat", "_id", from, to).await?);
    for table in CHAT_ROWID_TABLES {
        chat_rowids.extend(diff_column::<i64>(pool, table, "chat_row_id", from, to).await?);
    }

    let mut message_rowids: HashSet<i64> = HashSet::new();
    for table in MESSAGE_ROWID_TABLES {
        message_rowids.extend(diff_column::<i64>(pool, table, "message_row_id", from, to).await?);
    }
    // A media file arriving after its message (a `Media/` tree copied on a
    // later run) changes only the registry; find its messages by path.
    let paths = diff_column::<String>(pool, "wa_media_files", "relative_path", from, to).await?;
    for pin in [to, from] {
        message_rowids
            .extend(in_list::<String, i64>(pool, MESSAGE_BY_PATH_SQL, &paths, pin).await?);
    }
    let addon_rowids = diff_column::<i64>(
        pool,
        "message_add_on_reaction",
        "message_add_on_row_id",
        from,
        to,
    )
    .await?;
    // Rows that still exist resolve at `to`; rows the diff removed only
    // resolve at `from`. A rowid is the same row at either.
    for pin in [to, from] {
        chat_rowids.extend(in_list::<i64, i64>(pool, CHAT_BY_ADDON_SQL, &addon_rowids, pin).await?);
    }
    let message_rowids: Vec<i64> = message_rowids.into_iter().collect();
    for pin in [to, from] {
        chat_rowids
            .extend(in_list::<i64, i64>(pool, CHAT_BY_MESSAGE_SQL, &message_rowids, pin).await?);
    }

    // A chat HEAD still names is live; one only the previous commit can
    // name is gone.
    let rowids: Vec<i64> = chat_rowids.into_iter().collect();
    let live: HashSet<String> = in_list::<i64, String>(pool, JID_BY_CHAT_SQL, &rowids, to)
        .await?
        .into_iter()
        .collect();
    let mut gone: Vec<String> = in_list::<i64, String>(pool, JID_BY_CHAT_SQL, &rowids, from)
        .await?
        .into_iter()
        .filter(|jid| !live.contains(jid))
        .collect();
    gone.sort();
    gone.dedup();
    Ok(ChangedChats { live, gone })
}

const MESSAGE_BY_PATH_SQL: &str =
    "SELECT message_row_id FROM dolt_at_message_media('{pin}') WHERE file_path IN ({placeholders})";
const CHAT_BY_ADDON_SQL: &str =
    "SELECT chat_row_id FROM dolt_at_message_add_on('{pin}') WHERE _id IN ({placeholders})";
const CHAT_BY_MESSAGE_SQL: &str =
    "SELECT chat_row_id FROM dolt_at_message('{pin}') WHERE _id IN ({placeholders})";
const JID_BY_CHAT_SQL: &str = "SELECT coalesce(j.raw_string, j.user || '@' || j.server) \
     FROM dolt_at_chat('{pin}') c JOIN dolt_at_jid('{pin}') j ON j._id = c.jid_row_id \
     WHERE c._id IN ({placeholders})";

/// The `column` of every row `table` gained, lost or changed between the
/// two commits — from whichever side of the diff carries it.
async fn diff_column<T>(
    pool: &sqlx::SqlitePool,
    table: &str,
    column: &str,
    from: &Pin,
    to: &Pin,
) -> Result<Vec<T>>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send + Unpin,
{
    // Audited: `table` and `column` are `&'static str` at every callsite;
    // the commit hashes are bound.
    let sql = format!(
        "SELECT coalesce(to_{column}, from_{column}) FROM dolt_diff_{table} \
         WHERE from_ref = ? AND to_ref = ? AND diff_type != 'unchanged' \
           AND coalesce(to_{column}, from_{column}) IS NOT NULL"
    );
    sqlx::query_scalar::<_, T>(sqlx::AssertSqlSafe(sql))
        .bind(from.commit().to_string())
        .bind(to.commit().to_string())
        .fetch_all(pool)
        .await
        .with_context(|| format!("dolt_diff_{table}"))
}

/// Run `template` (holes: `{pin}`, `{placeholders}`) against the tables
/// as they were at `pin`, binding `ids` into the IN-list.
async fn in_list<T, R>(
    pool: &sqlx::SqlitePool,
    template: &'static str,
    ids: &[T],
    pin: &Pin,
) -> Result<Vec<R>>
where
    T: for<'q> sqlx::Encode<'q, sqlx::Sqlite>
        + sqlx::Type<sqlx::Sqlite>
        + Clone
        + Send
        + Sync
        + 'static,
    R: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send + Unpin,
{
    let mut out = Vec::new();
    for chunk in ids.chunks(datalib_etl::bulk::SQL_CHUNK) {
        let mut placeholders = String::new();
        datalib_etl::bulk::push_placeholder_list(&mut placeholders, chunk.len());
        // Audited: `template` is one of the `&'static str` consts above,
        // `pin` is a validated 40-hex commit hash (`Pin::at`), and the
        // IN-list is a placeholder run sized from the chunk with every id
        // bound.
        let sql = template
            .replace("{pin}", pin.commit())
            .replace("{placeholders}", &placeholders);
        let mut q = sqlx::query_scalar::<_, R>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            q = q.bind(id.clone());
        }
        out.extend(
            q.fetch_all(pool)
                .await
                .with_context(|| format!("resolve rowids at {}", pin.commit()))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::parse::parse;
    use datalib_etl::doltlite_raw::{commit_run, has_dolt_extensions, open as open_doltlite};
    use datalib_etl::periodize::Period;
    use sqlx::sqlite::SqlitePool;

    /// The slice of msgstore's schema render reads, as the mirror engine
    /// would lay it down: rowid keys, rowid foreign keys.
    const MSGSTORE_DDL: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS jid (_id INTEGER PRIMARY KEY, user TEXT, server TEXT, raw_string TEXT)",
        "CREATE TABLE IF NOT EXISTS chat (_id INTEGER PRIMARY KEY, jid_row_id INTEGER, subject TEXT, group_type INTEGER)",
        "CREATE TABLE IF NOT EXISTS message (_id INTEGER PRIMARY KEY, chat_row_id INTEGER, key_id TEXT, \
            from_me INTEGER, sender_jid_row_id INTEGER, timestamp INTEGER, message_type INTEGER, \
            text_data TEXT, sort_id INTEGER)",
        "CREATE TABLE IF NOT EXISTS message_media (message_row_id INTEGER PRIMARY KEY, file_path TEXT, \
            mime_type TEXT, file_size INTEGER, media_caption TEXT, media_name TEXT)",
        "CREATE TABLE IF NOT EXISTS message_add_on (_id INTEGER PRIMARY KEY, chat_row_id INTEGER, key_id TEXT, \
            from_me INTEGER, sender_jid_row_id INTEGER, parent_message_row_id INTEGER, timestamp INTEGER)",
        "CREATE TABLE IF NOT EXISTS message_add_on_reaction (message_add_on_row_id INTEGER PRIMARY KEY, reaction TEXT)",
        "CREATE TABLE IF NOT EXISTS message_text (message_row_id INTEGER PRIMARY KEY, description TEXT)",
        datalib_etl_whatsapp::schema_raw::WA_MEDIA_FILES_DDL,
    ];

    /// Full incremental-render loop end-to-end:
    ///   1. populate a fresh raw doltlite db with two chats, commit
    ///   2. render → expect both chats rendered, cursor written
    ///   3. render again with no DB changes → expect zero rendered (the
    ///      dolt_diff filter sees an empty changed set)
    ///   4. modify a message in chat A, commit
    ///   5. render → expect only chat A's bucket(s) re-rendered
    ///   6. delete chat B outright, commit
    ///   7. render → nothing re-rendered, chat B reported gone
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dolt_diff_drives_incremental_render() {
        let td = tempfile::tempdir().expect("tempdir");
        let raw_dir = td.path().join("raw");
        std::fs::create_dir_all(&raw_dir).expect("mkdir raw");
        let out_dir = td.path().join("out");
        let db_path = datalib_etl::doltlite_raw::db_path_for(&raw_dir);

        let pool = open_doltlite(&db_path, MSGSTORE_DDL)
            .await
            .expect("open doltlite");
        if !has_dolt_extensions(&pool).await {
            // Diagnostic — without it a stock-sqlite test run looks
            // indistinguishable from a passing doltlite run. Same
            // pattern as `commit_run_returns_hash_and_dolt_log_entry_or_skips`
            // in etl/doltlite_raw.
            #[allow(clippy::disallowed_macros)]
            {
                eprintln!(
                    "[whatsapp incremental test] stock libsqlite3 — \
                     dolt_diff_<table> unavailable, skipping"
                );
            }
            return;
        }

        // dolt_commit wants a committer identity.
        for q in [
            "SELECT dolt_config('user.name', 'datalib-test')",
            "SELECT dolt_config('user.email', 'test@datalib.local')",
        ] {
            sqlx::query(q).execute(&pool).await.expect("dolt_config");
        }

        // Two chats, one message each. Period::All puts every message
        // into a single bucket per chat so the rendered-doc count is
        // exactly the chat count — easier to assert against.
        seed_chat(&pool, 1, "alice@s.whatsapp.net", "k_a1", "hi from alice").await;
        seed_chat(&pool, 2, "bob@s.whatsapp.net", "k_b1", "hi from bob").await;
        let _hash1 = commit_run(&pool, "seed two chats")
            .await
            .expect("commit_run")
            .expect("doltlite returned no hash");
        pool.close().await;

        // First render — cold start (no cursor). Both chats should
        // render, cursor file should appear.
        let (docs1, _) = render_capture(&raw_dir, &out_dir).await;
        assert_eq!(
            docs1.len(),
            2,
            "first render should emit one doc per chat, got {docs1:?}"
        );
        let cursor_path = render_cursor::cursor_path(&out_dir, "test");
        assert!(
            cursor_path.exists(),
            "cursor file missing after first render"
        );
        let first_cursor = render_cursor::read(&cursor_path)
            .expect("read cursor")
            .expect("cursor populated");

        // Second render — no DB changes since first cursor. dolt_diff
        // should report zero changed chats → zero docs rendered.
        let (docs2, _) = render_capture(&raw_dir, &out_dir).await;
        assert!(
            docs2.is_empty(),
            "no-op rerun should render zero docs, got {docs2:?}"
        );

        // Modify alice's message and commit.
        let pool = open_doltlite(&db_path, MSGSTORE_DDL)
            .await
            .expect("reopen doltlite");
        sqlx::query("UPDATE message SET text_data = ? WHERE chat_row_id = 1")
            .bind("hi from alice (edited)")
            .execute(&pool)
            .await
            .expect("update alice message");
        let _hash2 = commit_run(&pool, "modify alice")
            .await
            .expect("commit_run")
            .expect("doltlite returned no hash on modify");
        pool.close().await;

        // Third render — only alice's chat should be in the changed set.
        let (docs3, gone3) = render_capture(&raw_dir, &out_dir).await;
        assert_eq!(
            docs3.len(),
            1,
            "after modifying one chat, render should emit exactly one doc, got {docs3:?}"
        );
        assert!(gone3.is_empty(), "nothing was deleted, got {gone3:?}");
        let alice_chat_uuid = crate::render::whatsapp_chat_uuid("test", "alice@s.whatsapp.net");
        let expected = crate::render::whatsapp_markdown_uuid(&alice_chat_uuid, "all");
        assert_eq!(
            docs3[0], expected,
            "rendered doc should belong to alice's chat"
        );

        // Cursor advanced past the previous HEAD.
        let third_cursor = render_cursor::read(&cursor_path)
            .expect("read cursor 3")
            .expect("cursor populated 3");
        assert_ne!(
            third_cursor.last_rendered_hash, first_cursor.last_rendered_hash,
            "cursor should advance after a committed change"
        );

        // Delete bob's chat the way a backup does: the rows are simply not
        // there any more. The only trace is on the `from` side of the diff,
        // where the rowid still resolves to a JID.
        let pool = open_doltlite(&db_path, MSGSTORE_DDL)
            .await
            .expect("reopen doltlite");
        for q in [
            "DELETE FROM message WHERE chat_row_id = 2",
            "DELETE FROM chat WHERE _id = 2",
            "DELETE FROM jid WHERE _id = 2",
        ] {
            sqlx::query(q).execute(&pool).await.expect("delete bob");
        }
        commit_run(&pool, "delete bob")
            .await
            .expect("commit_run")
            .expect("doltlite returned no hash on delete");
        pool.close().await;

        let (docs4, gone4) = render_capture(&raw_dir, &out_dir).await;
        assert!(
            docs4.is_empty(),
            "a deletion re-renders nothing, got {docs4:?}"
        );
        assert_eq!(gone4, vec!["bob@s.whatsapp.net".to_string()]);
    }

    /// One jid, chat and message, all sharing `rowid` — the shape the
    /// mirror engine lays down from a real msgstore.
    async fn seed_chat(pool: &SqlitePool, rowid: i64, chat_jid: &str, key_id: &str, text: &str) {
        sqlx::query(
            "INSERT INTO jid (_id, user, server, raw_string) VALUES (?, ?, 's.whatsapp.net', ?)",
        )
        .bind(rowid)
        .bind(chat_jid.split('@').next().unwrap())
        .bind(chat_jid)
        .execute(pool)
        .await
        .expect("insert jid");
        sqlx::query("INSERT INTO chat (_id, jid_row_id, subject) VALUES (?, ?, NULL)")
            .bind(rowid)
            .bind(rowid)
            .execute(pool)
            .await
            .expect("insert chat");
        sqlx::query(
            "INSERT INTO message (_id, chat_row_id, key_id, from_me, timestamp, \
                message_type, text_data, sort_id) \
             VALUES (?, ?, ?, 0, 1700000000000, 0, ?, 1)",
        )
        .bind(rowid)
        .bind(rowid)
        .bind(key_id)
        .bind(text)
        .execute(pool)
        .await
        .expect("insert message");
    }

    /// Rendered markdown uuids, and the chat JIDs reported gone.
    async fn render_capture(raw_dir: &Path, out_dir: &Path) -> (Vec<String>, Vec<String>) {
        let raw_dir = raw_dir.to_path_buf();
        let out_dir = out_dir.to_path_buf();
        // parse + render are sync but call into tokio::task::block_in_place,
        // so we have to push the whole thing off the test's reactor thread.
        tokio::task::spawn_blocking(move || {
            let parsed = parse(&raw_dir, Period::All, "test").expect("parse");
            let mut emitted: Vec<String> = Vec::new();
            let mut gone: Vec<String> = Vec::new();
            let progress = datalib_etl::progress::Progress::noop();
            let prior: HashMap<String, String> = HashMap::new();
            let mut on_complete =
                |md: datalib_etl_render::grid_index::RenderedMarkdown| -> Result<()> {
                    emitted.push(md.markdown_uuid);
                    Ok(())
                };
            let mut on_chat_gone = |jid: &str| -> Result<()> {
                gone.push(jid.to_string());
                Ok(())
            };
            render_all(
                &parsed.chats,
                &parsed.blobs_by_chat,
                &raw_dir,
                &out_dir,
                "test",
                &progress,
                &prior,
                &mut on_complete,
                &mut on_chat_gone,
            )
            .expect("render_all");
            (emitted, gone)
        })
        .await
        .expect("spawn_blocking joined")
    }
}
