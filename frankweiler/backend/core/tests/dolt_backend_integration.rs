//! End-to-end integration test for the doltlite backend.
//!
//! Opens a doltlite file in a temp directory, connects [`DoltRepo`],
//! creates the `grid_rows` table, inserts a handful of fixture rows, and
//! verifies that [`MirrorRepo::search`] returns them in the expected order.
//!
//! No subprocess, no port — just a file on disk. This test runs anywhere
//! `sqlite`-compatible doltlite is linked (the sqlx-sqlite driver opens
//! the file natively).

use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::query::parse_query;
use frankweiler_core::repo::MirrorRepo;
use frankweiler_schema::grid_rows::DDL as GRID_DDL;
use frankweiler_schema::markdowns::DDL as MARKDOWNS_DDL;
use std::path::PathBuf;
use std::sync::Arc;

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("fw-dolt-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db")
}

/// A fresh data root has no `grid_rows`/`markdowns` yet (they appear on
/// the first sync). The read paths must report that as "no data yet" —
/// not as an error toast — while anything other than the exact
/// missing-table case still surfaces as an error.
#[tokio::test]
async fn dolt_repo_databaseless_root_reads_as_empty() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(&db_path, root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));

    // No GRID_DDL / MARKDOWNS_DDL: this is the pre-first-sync state.
    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
    let rows = repo
        .search_by_uuids(&parse_query(""), &["c-1".into()], 100)
        .await
        .unwrap();
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
    assert!(repo.grid_row_refs().await.unwrap().is_empty());
    assert!(repo.chat_meta("c-1").await.unwrap().is_none());
    assert!(repo.qmd_path_for_markdown("c-1").await.unwrap().is_none());

    // Narrowness: a real failure (here, a schema mismatch — the table
    // exists but lacks the queried columns) must still be an error, not
    // read as "no data yet".
    sqlx::query("CREATE TABLE grid_rows (only_column TEXT)")
        .execute(repo.pool())
        .await
        .expect("create decoy grid_rows");
    let err = repo.search(&parse_query(""), 100).await.unwrap_err();
    assert!(
        err.to_string().contains("no such column"),
        "expected a surfaced schema error, got {err}"
    );

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test]
async fn dolt_repo_round_trip_search_and_chat_meta() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(&db_path, root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));

    for (_t, ddl) in GRID_DDL {
        sqlx::query(ddl)
            .execute(repo.pool())
            .await
            .expect("create grid_rows");
    }
    for (_t, ddl) in MARKDOWNS_DDL {
        sqlx::query(ddl)
            .execute(repo.pool())
            .await
            .expect("create markdowns");
    }
    // For Anthropic chats the rendered file is 1:1 with the
    // conversation, so markdown_uuid == conversation_uuid here.
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, when_ts_utc, when_offset, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link, qmd_path, source_url, markdown_uuid) \
         VALUES ('c-1','anthropic','Chat','Claude','2026-04-01T10:00:00+00:00', \
                 '2026-04-01T10:00:00.000000Z','+00:00', \
                 NULL,'acct-a',NULL,NULL,'Test conv','c-1',NULL,'/chat/c-1', \
                 'summary','', 'chats/c-1.md', 'https://claude.ai/chat/c-1', 'c-1')",
    )
    .execute(repo.pool())
    .await
    .expect("insert chat row");
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, when_ts_utc, when_offset, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link, markdown_uuid) \
         VALUES ('m-1','anthropic','User Input','Claude','2026-04-01T10:01:00+00:00', \
                 '2026-04-01T10:01:00.000000Z','+00:00', \
                 'acct-a','acct-a',NULL,NULL,'Test conv','c-1',0,'/chat/c-1','hello there','','c-1')",
    )
    .execute(repo.pool())
    .await
    .expect("insert message row");
    sqlx::query(
        "INSERT INTO markdowns (markdown_uuid, source_name, provider, kind, md_path, \
         row_set_hash, renderer_version) \
         VALUES ('c-1','test','anthropic','chat','chats/c-1.md','deadbeef','test-v1')",
    )
    .execute(repo.pool())
    .await
    .expect("insert markdown row");

    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows, got {rows:?}");
    // Chat tiebreaks before its message.
    assert_eq!(rows[0].kind, "Chat");
    assert_eq!(rows[1].kind, "User Input");

    let filtered = repo
        .search(&parse_query("source:Claude type:all"), 100)
        .await
        .unwrap();
    assert!(!filtered.is_empty());
    assert!(filtered.iter().all(|r| r.source == "Claude"));

    let meta = repo
        .chat_meta("c-1")
        .await
        .unwrap()
        .expect("chat meta present");
    assert_eq!(meta.name.as_deref(), Some("Test conv"));
    assert_eq!(meta.source_label.as_deref(), Some("Claude"));
    assert_eq!(
        meta.source_url.as_deref(),
        Some("https://claude.ai/chat/c-1")
    );

    let qmd = repo.qmd_path_for_markdown("c-1").await.unwrap();
    assert!(qmd.is_some());
    let qmd = qmd.unwrap();
    assert!(qmd.is_absolute(), "expected absolute qmd path, got {qmd:?}");
    assert!(qmd.to_string_lossy().ends_with("chats/c-1.md"));

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}
