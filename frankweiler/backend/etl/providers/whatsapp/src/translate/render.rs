//! WhatsApp render — thin adapter over
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! Opens the blob_cas pair (entity pool + sibling CAS pool) so
//! chat-common can stream attachment bytes through the universal
//! `SqliteBlobReader` interface, then forwards every other arg
//! straight through.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobReader, InMemoryBlobReader};

use super::blob_reader::WhatsAppBlobReader;
use frankweiler_etl::doltlite_raw;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl_chat_common::{
    render::{RenderProfile, RenderSummary},
    NormalizedChat,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing WhatsApp doc rebuilt.
///
/// v3 = attachment bytes now stream through `frankweiler_etl::blob_cas`
/// (the same store every other chat-style provider uses). The on-disk
/// `blobs/<short>.<ext>` filename is whatever
/// [`BlobView::rendered_filename`] picks, which is blake3-prefixed
/// (was sha256-prefixed in v2); existing docs all rebuild.
///
/// v2 = attachments now materialize bytes into the rendered page's
/// `blobs/` subdir, so images render inline instead of "(not yet
/// fetched)".
///
/// v1 = chat-common's unified block style + reactions inline +
/// per-message `id="m-{uuid}"` anchors.
///
/// [`BlobView::rendered_filename`]: frankweiler_etl::blob_cas::BlobView::rendered_filename
pub const RENDER_VERSION: u32 = 3;

const SOURCE_LABEL: &str = "WhatsApp";

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "whatsapp",
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "WhatsApp Chat".to_string(),
        message_kind: "WhatsApp Message".to_string(),
        reaction_kind: "WhatsApp Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render every chat. `raw_dir` is the source's `input_path` — same
/// value translate's `parse` walks — used here to derive the sibling
/// CAS path so the renderer can stream attachment bytes through
/// [`SqliteBlobReader`].
pub fn render_all(
    chats: &[NormalizedChat],
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    _prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let blobs = open_blob_reader(raw_dir)?;

    // Incremental gate: if a render cursor exists at the root of this
    // source's render directory, ask doltlite which chats changed
    // between that hash and HEAD via `dolt_diff_wa_<table>`. Skip the
    // rest. Cold start (no cursor) or no doltlite db on disk renders
    // every chat.
    //
    // The orchestrator's per-doc `prior_fingerprints` map is ignored
    // here — dolt is the single source of truth for "did anything
    // change?". Cost: a row change in a long-running chat re-renders
    // every period bucket of that chat with identical bytes (mtimes
    // bump). Same-bytes rewrites are fine; if a downstream consumer
    // grows sensitive to mtime, reintroduce a per-bucket compare.
    let cursor_path = render_cursor::cursor_path(out_dir, "whatsapp", source_name);
    let prior = render_cursor::read(&cursor_path)?;
    let db_path = doltlite_raw::db_path_for(raw_dir);

    let (filtered_owned, new_head, scan_elapsed): (
        Option<Vec<NormalizedChat>>,
        Option<String>,
        Option<std::time::Duration>,
    ) = if db_path.exists() {
        let (changed, head, elapsed) = tokio::task::block_in_place(|| {
            let h = tokio::runtime::Handle::try_current();
            match h {
                Ok(h) => h.block_on(scan_diff(
                    &db_path,
                    prior.as_ref().map(|c| c.last_rendered_hash.as_str()),
                )),
                Err(_) => tokio::runtime::Runtime::new()?.block_on(scan_diff(
                    &db_path,
                    prior.as_ref().map(|c| c.last_rendered_hash.as_str()),
                )),
            }
        })?;
        let filtered = changed.as_ref().map(|set| {
            chats
                .iter()
                .filter(|c| set.contains(&c.id))
                .cloned()
                .collect::<Vec<_>>()
        });
        tracing::info!(
            source = source_name,
            scan_elapsed_ms = elapsed.map(|d| d.as_millis() as u64),
            changed_chats = changed.as_ref().map(|s| s.len() as i64).unwrap_or(-1),
            cold_start = changed.is_none(),
            "[translate] whatsapp dolt_diff scan"
        );
        (filtered, head, elapsed)
    } else {
        (None, None, None)
    };
    let to_render: &[NormalizedChat] = filtered_owned.as_deref().unwrap_or(chats);

    let empty_fingerprints: HashMap<String, String> = HashMap::new();
    let summary = frankweiler_etl_chat_common::render::render_all(
        &profile(),
        to_render,
        out_dir,
        source_name,
        blobs,
        progress,
        &empty_fingerprints,
        on_doc_complete,
    )?;

    if let Some(head) = new_head {
        render_cursor::write(&cursor_path, &head, scan_elapsed)?;
    }
    Ok(summary)
}

/// Ask doltlite: what chats changed since `last_hash`, and what's the
/// current HEAD? Returns `(changed_chat_jids, new_head)`. `None` for
/// changed means "no filter — render everything" (cold start). `None`
/// for new_head means we couldn't read HEAD (non-doltlite sqlite); the
/// cursor stays unwritten and next run is another cold start.
async fn scan_diff(
    db_path: &Path,
    last_hash: Option<&str>,
) -> Result<(
    Option<HashSet<String>>,
    Option<String>,
    Option<std::time::Duration>,
)> {
    let pool = open_ro_pool(db_path).await?;

    let new_head: Option<String> =
        sqlx::query_scalar("SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1")
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();

    let (changed, elapsed) = match last_hash {
        None => (None, None),
        Some(from_ref) => {
            // One union across the per-table dolt_diff vtabs. The
            // `chat_jid` column lives on every wa_message_* table, so a
            // single COALESCE(to, from) projects the natural bucket key
            // across added/modified/removed rows. wa_jid and
            // wa_media_files don't carry chat_jid and so are omitted;
            // attachment changes propagate via wa_message_media.
            let sql = "
                SELECT DISTINCT chat_jid FROM (
                    SELECT coalesce(to_chat_jid, from_chat_jid) AS chat_jid
                      FROM dolt_diff_wa_chat
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_text
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_media
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_add_on
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_add_on_reaction
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE chat_jid IS NOT NULL
            ";
            let started = std::time::Instant::now();
            let rows = sqlx::query(sql)
                .bind(from_ref)
                .fetch_all(&pool)
                .await
                .context("query dolt_diff_wa_* changed chats")?;
            let elapsed = started.elapsed();
            let set: HashSet<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
            (Some(set), Some(elapsed))
        }
    };

    pool.close().await;
    Ok((changed, new_head, elapsed))
}

/// Build the `SqliteBlobReader` chat-common reads attachment bytes
/// through. Falls back to an empty in-memory reader when the raw store
/// or CAS files aren't present (e.g. a translate-only re-run against a
/// data root whose extract was skipped — the renderer emits placeholder
/// markdown in that case, same as for any other blob_cas miss).
fn open_blob_reader(raw_dir: &Path) -> Result<Arc<dyn BlobReader>> {
    let db_path = doltlite_raw::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(InMemoryBlobReader::empty_handle());
    }
    let cas_path = blob_cas::cas_path_for(&db_path);

    // Bridge sync render → async sqlx the same way `translate::parse`
    // does: borrow the current tokio Handle if there is one, else spin
    // a fresh runtime for the open. Sync orchestrator calls translate
    // from within `#[tokio::main]`, so the Handle path is the hot one.
    tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(h) => h.block_on(open_pools(&db_path, &cas_path)),
            Err(_) => tokio::runtime::Runtime::new()?.block_on(open_pools(&db_path, &cas_path)),
        }
    })
}

async fn open_pools(db_path: &Path, cas_path: &Path) -> Result<Arc<dyn BlobReader>> {
    let refs_pool = open_ro_pool(db_path).await?;
    if !cas_path.exists() {
        // Refs without bytes: a placeholder will fire for every
        // attachment whose `read_by_ref_id` returns None, which the
        // chat-common renderer already handles.
        return Ok(InMemoryBlobReader::empty_handle());
    }
    let cas_pool = open_ro_pool(cas_path).await?;
    Ok(WhatsAppBlobReader::new(refs_pool, cas_pool).into_handle())
}

async fn open_ro_pool(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("sqlite uri for {}", path.display()))?
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_raw::ALL_DDL;
    use crate::translate::parse::parse;
    use frankweiler_etl::doltlite_raw::{commit_run, has_dolt_extensions, open as open_doltlite};
    use frankweiler_etl::periodize::Period;

    /// Full incremental-render loop end-to-end:
    ///   1. populate a fresh raw doltlite db with two chats, commit
    ///   2. render → expect both chats rendered, cursor written
    ///   3. render again with no DB changes → expect zero rendered (the
    ///      dolt_diff filter sees an empty changed set)
    ///   4. modify a message in chat A, commit
    ///   5. render → expect only chat A's bucket(s) re-rendered
    ///
    /// Skipped silently on stock libsqlite3 (no dolt_* SQL surface). Under
    /// bazel (where doltlite is linked) this is the full incremental
    /// story.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dolt_diff_drives_incremental_render() {
        let td = tempfile::tempdir().expect("tempdir");
        let raw_dir = td.path().join("raw");
        std::fs::create_dir_all(&raw_dir).expect("mkdir raw");
        let out_dir = td.path().join("out");
        let db_path = frankweiler_etl::doltlite_raw::db_path_for(&raw_dir);

        let pool = open_doltlite(&db_path, ALL_DDL)
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
            "SELECT dolt_config('user.name', 'frankweiler-test')",
            "SELECT dolt_config('user.email', 'test@frankweiler.local')",
        ] {
            sqlx::query(q).execute(&pool).await.expect("dolt_config");
        }

        // Two chats, one message each. Period::All puts every message
        // into a single bucket per chat so the rendered-doc count is
        // exactly the chat count — easier to assert against.
        seed_chat(&pool, "alice@s.whatsapp.net", "k_a1", "hi from alice").await;
        seed_chat(&pool, "bob@s.whatsapp.net", "k_b1", "hi from bob").await;
        let _hash1 = commit_run(&pool, "seed two chats")
            .await
            .expect("commit_run")
            .expect("doltlite returned no hash");
        pool.close().await;

        // First render — cold start (no cursor). Both chats should
        // render, cursor file should appear.
        let docs1 = render_capture(&raw_dir, &out_dir).await;
        assert_eq!(
            docs1.len(),
            2,
            "first render should emit one doc per chat, got {docs1:?}"
        );
        let cursor_path = render_cursor::cursor_path(&out_dir, "whatsapp", "test");
        assert!(
            cursor_path.exists(),
            "cursor file missing after first render"
        );
        let first_cursor = render_cursor::read(&cursor_path)
            .expect("read cursor")
            .expect("cursor populated");

        // Second render — no DB changes since first cursor. dolt_diff
        // should report zero changed chats → zero docs rendered.
        let docs2 = render_capture(&raw_dir, &out_dir).await;
        assert!(
            docs2.is_empty(),
            "no-op rerun should render zero docs, got {docs2:?}"
        );

        // Modify alice's message and commit.
        let pool = open_doltlite(&db_path, ALL_DDL)
            .await
            .expect("reopen doltlite");
        sqlx::query("UPDATE wa_message SET text_data = ? WHERE chat_jid = ?")
            .bind("hi from alice (edited)")
            .bind("alice@s.whatsapp.net")
            .execute(&pool)
            .await
            .expect("update alice message");
        let _hash2 = commit_run(&pool, "modify alice")
            .await
            .expect("commit_run")
            .expect("doltlite returned no hash on modify");
        pool.close().await;

        // Third render — only alice's chat should be in the changed set.
        let docs3 = render_capture(&raw_dir, &out_dir).await;
        assert_eq!(
            docs3.len(),
            1,
            "after modifying one chat, render should emit exactly one doc, got {docs3:?}"
        );
        let alice_chat_uuid = crate::translate::whatsapp_chat_uuid("test", "alice@s.whatsapp.net");
        let expected = crate::translate::whatsapp_markdown_uuid(&alice_chat_uuid, "all");
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
    }

    async fn seed_chat(pool: &SqlitePool, chat_jid: &str, key_id: &str, text: &str) {
        sqlx::query(
            "INSERT INTO wa_jid (raw_string, user, server) \
             VALUES (?, ?, 's.whatsapp.net')",
        )
        .bind(chat_jid)
        .bind(chat_jid.split('@').next().unwrap())
        .execute(pool)
        .await
        .expect("insert wa_jid");
        sqlx::query("INSERT INTO wa_chat (chat_jid, subject) VALUES (?, NULL)")
            .bind(chat_jid)
            .execute(pool)
            .await
            .expect("insert wa_chat");
        sqlx::query(
            "INSERT INTO wa_message (chat_jid, key_id, from_me, timestamp, \
                message_type, text_data, sort_id) \
             VALUES (?, ?, 0, 1700000000000, 0, ?, 1)",
        )
        .bind(chat_jid)
        .bind(key_id)
        .bind(text)
        .execute(pool)
        .await
        .expect("insert wa_message");
    }

    /// Runs `parse` + `render_all` and returns the markdown_uuids the
    /// chat-common renderer emitted (via on_doc_complete). Uses
    /// `Period::All` so every chat collapses to a single bucket.
    async fn render_capture(raw_dir: &Path, out_dir: &Path) -> Vec<String> {
        let raw_dir = raw_dir.to_path_buf();
        let out_dir = out_dir.to_path_buf();
        // parse + render are sync but call into tokio::task::block_in_place,
        // so we have to push the whole thing off the test's reactor thread.
        tokio::task::spawn_blocking(move || {
            let chats = parse(&raw_dir, Period::All, "test").expect("parse");
            let mut emitted: Vec<String> = Vec::new();
            let progress = frankweiler_etl::progress::Progress::noop();
            let prior: HashMap<String, String> = HashMap::new();
            let mut on_complete = |md: frankweiler_etl::load::RenderedMarkdown| -> Result<()> {
                emitted.push(md.markdown_uuid);
                Ok(())
            };
            render_all(
                &chats,
                &raw_dir,
                &out_dir,
                "test",
                &progress,
                &prior,
                &mut on_complete,
            )
            .expect("render_all");
            emitted
        })
        .await
        .expect("spawn_blocking joined")
    }
}
