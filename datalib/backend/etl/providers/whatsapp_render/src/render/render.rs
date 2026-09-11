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
use sqlx::Row;

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing WhatsApp doc rebuilt. v5: an `@lid` chat
/// or sender reads as its display name or phone number when msgstore's
/// `lid_display_name` / `jid_map` know one.
pub const RENDER_VERSION: u32 = 5;

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
    // Chat JIDs the diff named that no `wa_chat` row still carries. The
    // scan happens in here rather than in `parse`, so the caller learns
    // about them the same way it learns about documents.
    on_chat_gone: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<RenderSummary> {
    // Incremental gate: if a render cursor exists at the root of this
    // source's render directory, ask doltlite which chats changed
    // between that hash and HEAD via `dolt_diff_wa_<table>`. Skip the
    // rest. Cold start (no cursor) or no doltlite db on disk renders
    // every chat.
    let cursor_path = render_cursor::cursor_path(out_dir, source_id);
    let prior = render_cursor::read_for_params(&cursor_path, &render_cursor::no_params())?;
    let db_path = doltlite_raw::db_path_for(raw_dir);

    let (filtered_owned, new_head): (Option<Vec<NormalizedChat>>, Option<String>) =
        if db_path.exists() {
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
                source = source_id,
                scan_elapsed_ms = elapsed.map(|d| d.as_millis() as u64),
                changed_chats = changed.as_ref().map(|s| s.len() as i64).unwrap_or(-1),
                cold_start = changed.is_none(),
                "[render] whatsapp dolt_diff scan"
            );
            // Every backup is a full snapshot (the ingest truncates first), so
            // a chat the diff named that `wa_chat` no longer carries is one the
            // phone deleted.
            if let Some(set) = changed.as_ref() {
                let pool = tokio::task::block_in_place(|| {
                    let h = tokio::runtime::Handle::current();
                    h.block_on(datalib_etl::doltlite_raw::open_reader(&db_path))
                })?;
                // Its own pool, so its own pin: this asks which chats are gone at
                // a commit, not in whatever the writer is part-way through.
                let gone = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let Some(pin) = datalib_etl::pin::head(&pool).await? else {
                            return Ok::<_, anyhow::Error>(Vec::new());
                        };
                        datalib_etl::pin::install_views(&pool, &pin).await?;
                        doltlite_raw::buckets_without_rows(
                            &pool,
                            datalib_etl::pin::Reads::At(&pin),
                            set,
                            &[("wa_chat", "chat_jid")],
                        )
                        .await
                    })
                })?;
                for jid in &gone {
                    on_chat_gone(jid)?;
                }
            }
            (filtered, head)
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
    let pool = datalib_etl::doltlite_raw::open_reader(db_path).await?;

    let new_head: Option<String> = datalib_etl::pin::head(&pool)
        .await?
        .map(|p| p.commit().to_string());

    // Both refs, or neither: with no commit to scan *to* there is nothing
    // committed to diff against, and cold-starting is the only honest answer.
    // Scanning to the sampled hash rather than to the symbolic `HEAD` keeps
    // this diff and the reads that follow it naming one commit even while a
    // producer is still committing — see `datalib_etl::pin`.
    let (changed, elapsed) = match (last_hash, new_head.as_deref()) {
        (None, _) | (_, None) => (None, None),
        (Some(from_ref), Some(to_ref)) => {
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
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_text
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_media
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_add_on
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_jid, from_chat_jid)
                      FROM dolt_diff_wa_message_add_on_reaction
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                )
                WHERE chat_jid IS NOT NULL
            ";
            let started = std::time::Instant::now();
            let rows = sqlx::query(sql)
                .bind(from_ref)
                .bind(to_ref)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::parse::parse;
    use datalib_etl::doltlite_raw::{commit_run, has_dolt_extensions, open as open_doltlite};
    use datalib_etl::periodize::Period;
    use datalib_etl_whatsapp::schema_raw::ALL_DDL;
    use sqlx::sqlite::SqlitePool;

    /// Full incremental-render loop end-to-end:
    ///   1. populate a fresh raw doltlite db with two chats, commit
    ///   2. render → expect both chats rendered, cursor written
    ///   3. render again with no DB changes → expect zero rendered (the
    ///      dolt_diff filter sees an empty changed set)
    ///   4. modify a message in chat A, commit
    ///   5. render → expect only chat A's bucket(s) re-rendered
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dolt_diff_drives_incremental_render() {
        let td = tempfile::tempdir().expect("tempdir");
        let raw_dir = td.path().join("raw");
        std::fs::create_dir_all(&raw_dir).expect("mkdir raw");
        let out_dir = td.path().join("out");
        let db_path = datalib_etl::doltlite_raw::db_path_for(&raw_dir);

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
            "SELECT dolt_config('user.name', 'datalib-test')",
            "SELECT dolt_config('user.email', 'test@datalib.local')",
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

    async fn render_capture(raw_dir: &Path, out_dir: &Path) -> Vec<String> {
        let raw_dir = raw_dir.to_path_buf();
        let out_dir = out_dir.to_path_buf();
        // parse + render are sync but call into tokio::task::block_in_place,
        // so we have to push the whole thing off the test's reactor thread.
        tokio::task::spawn_blocking(move || {
            let parsed = parse(&raw_dir, Period::All, "test").expect("parse");
            let mut emitted: Vec<String> = Vec::new();
            let progress = datalib_etl::progress::Progress::noop();
            let prior: HashMap<String, String> = HashMap::new();
            let mut on_complete =
                |md: datalib_etl_render::grid_index::RenderedMarkdown| -> Result<()> {
                    emitted.push(md.markdown_uuid);
                    Ok(())
                };
            let mut on_chat_gone = |_: &str| -> Result<()> { Ok(()) };
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
            emitted
        })
        .await
        .expect("spawn_blocking joined")
    }
}
