//! End-to-end integration test for the doltlite backend.

use datalib_schema::grid_rows::{GridRow, DDL as GRID_DDL};
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::providers::Provider;
use datalib_table::BulkUpsertable;
use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::query::parse_query;
use datalib_unified_index::repo::IndexRepo;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The `grid_index` step's handle on the root's index, for seeding. The
/// repo under test opens its own, read-only, once the file exists —
/// which is exactly how the applet reads what the step wrote.
async fn writer(root: &Path) -> sqlx::SqlitePool {
    datalib_core::store::open_pool(&datalib_core::layout::grid_index_db(root))
        .await
        .expect("open a writer on the grid index")
}

/// What the step does after its DDL: a read-only handle takes its schema
/// from HEAD, so a table that was never committed is not there to read.
async fn commit_schema(writer: &sqlx::SqlitePool) {
    sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', 'schema')")
        .fetch_one(writer)
        .await
        .expect("commit the schema");
}

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("datalib-dolt-itest-")
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
    let repo = DoltRepo::open(root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));
    let writer = writer(&root).await;

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
    assert!(repo
        .md_paths_for(&["c-1".to_string()])
        .await
        .unwrap()
        .is_empty());

    // Narrowness: a real failure (here, a schema mismatch — the table
    // exists but lacks the queried columns) must still be an error, not
    // read as "no data yet".
    sqlx::query("CREATE TABLE grid_rows (only_column TEXT)")
        .execute(&writer)
        .await
        .expect("create decoy grid_rows");
    commit_schema(&writer).await;
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
    let repo = DoltRepo::open(root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));
    let writer = writer(&root).await;

    for (_t, ddl) in GRID_DDL {
        sqlx::query(*ddl)
            .execute(&writer)
            .await
            .expect("create grid_rows");
    }
    for (_t, ddl) in MARKDOWNS_DDL {
        sqlx::query(*ddl)
            .execute(&writer)
            .await
            .expect("create markdowns");
    }
    commit_schema(&writer).await;
    // For Anthropic chats the rendered file is 1:1 with the
    // conversation, so markdown_uuid == conversation_uuid here.
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, created_at, created_at_utc, created_offset, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link, qmd_path, source_url, markdown_uuid, \
         is_document) \
         VALUES ('c-1','claude','Chat','Claude','2026-04-01T10:00:00+00:00', \
                 '2026-04-01T10:00:00.000000Z','+00:00', \
                 NULL,'acct-a',NULL,NULL,'Test conv','c-1',NULL,'/chat/c-1', \
                 'summary','', 'chats/c-1.md', 'https://claude.ai/chat/c-1', 'c-1', 1)",
    )
    .execute(&writer)
    .await
    .expect("insert chat row");
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, created_at, created_at_utc, created_offset, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link, markdown_uuid, is_document) \
         VALUES ('m-1','claude','User Input','Claude','2026-04-01T10:01:00+00:00', \
                 '2026-04-01T10:01:00.000000Z','+00:00', \
                 'acct-a','acct-a',NULL,NULL,'Test conv','c-1',0,'/chat/c-1','hello there','','c-1', 0)",
    )
    .execute(&writer)
    .await
    .expect("insert message row");
    sqlx::query(
        "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind, md_path, \
         renderer_version) \
         VALUES ('c-1','test','claude','chat','chats/c-1.md','test-v1')",
    )
    .execute(&writer)
    .await
    .expect("insert markdown row");

    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows, got {rows:?}");
    // Chat tiebreaks before its message.
    assert_eq!(rows[0].kind, "Chat");
    assert_eq!(rows[1].kind, "User Input");

    let filtered = repo
        .search(&parse_query("source:Claude"), 100)
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
    // NULL columns are `None`, not `Some("")`: the sqlite driver decodes a
    // NULL into an empty string when asked for a bare `String` (#13).
    assert_eq!(meta.project, None, "project was NULL");
    assert_eq!(meta.channel, None, "channel was NULL");

    let qmd = repo.qmd_path_for_markdown("c-1").await.unwrap();
    assert!(qmd.is_some());
    let qmd = qmd.unwrap();
    assert!(qmd.is_absolute(), "expected absolute qmd path, got {qmd:?}");
    assert!(qmd.to_string_lossy().ends_with("chats/c-1.md"));

    // The batch form the grid's index-state columns use must agree
    // with the single lookup, and must simply omit uuids it has no
    // rendered file for rather than inventing an entry for them.
    let batch = repo
        .md_paths_for(&["c-1".to_string(), "nope".to_string()])
        .await
        .unwrap();
    assert_eq!(batch.len(), 1, "unknown uuid should be absent: {batch:?}");
    assert_eq!(batch.get("c-1"), Some(&qmd));

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}

/// A source's storage report is written into that source's own
/// `render_markdown/` tree, so the first segment of its `qmd_path` is
/// the measured source. It must not be filed there: `source_id:` has
/// to answer `datalib` for it, and the measured source's own name has to
/// leave it out. Both halves — the column's value and the SQL filter —
/// are checked here against a real store, because the two are derived
/// separately and can disagree.
#[tokio::test]
async fn storage_rows_are_filed_under_datalib_not_the_measured_source() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(root.clone()).await.expect("open doltlite");
    let writer = writer(&root).await;

    for (_t, ddl) in GRID_DDL {
        sqlx::query(*ddl)
            .execute(&writer)
            .await
            .expect("create grid_rows");
    }
    commit_schema(&writer).await;
    // Both rows sit under `claude-work/render_markdown/`: the chat is
    // that source's data, the measurement is datalib describing it.
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, created_at, created_at_utc, \
         created_offset, conversation_uuid, entire_chat, text, qmd_path, markdown_uuid, \
         is_document) \
         VALUES ('c-1','claude','Chat','Claude','2026-04-01T10:00:00+00:00', \
                 '2026-04-01T10:00:00.000000Z','+00:00','c-1','/chat/c-1','summary', \
                 'claude-work/render_markdown/chats/c-1.md','c-1', 1)",
    )
    .execute(&writer)
    .await
    .expect("insert chat row");
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, created_at, created_at_utc, \
         created_offset, account, conversation_uuid, entire_chat, text, qmd_path, markdown_uuid, \
         is_document) \
         VALUES ('s-1','datalib','Store','Storage','2026-04-01T10:00:00+00:00', \
                 '2026-04-01T10:00:00.000000Z','+00:00','claude-work','s-1','/chat/s-1', \
                 'claude-work/ingest/entities.doltlite_db', \
                 'claude-work/render_markdown/_datalib/storage.md','s-1', 0)",
    )
    .execute(&writer)
    .await
    .expect("insert storage row");

    let all = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
    let storage = all.iter().find(|r| r.uuid == "s-1").expect("storage row");
    assert_eq!(storage.source_id, "datalib");

    let measured = repo
        .search(&parse_query("source_id:claude-work"), 100)
        .await
        .unwrap();
    assert_eq!(
        measured.iter().map(|r| &r.uuid).collect::<Vec<_>>(),
        vec!["c-1"],
        "the measured source's name must not pull in what measures it"
    );

    let datalibs = repo
        .search(&parse_query("source_id:datalib"), 100)
        .await
        .unwrap();
    assert_eq!(
        datalibs.iter().map(|r| &r.uuid).collect::<Vec<_>>(),
        vec!["s-1"]
    );

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}

/// The SELECT list in `dolt_repo` is hand-written and the row mapper
/// reads each column with `try_get(..).unwrap_or_default()`, so a
/// column left out of the list comes back as a blank rather than an
/// error — the one failure nothing downstream can see. Write a row
/// with every column filled, through the same `BulkUpsertable` contract
/// the index writes with, and require every field on the wire to be
/// filled on the way back.
#[tokio::test]
async fn every_wire_field_survives_the_round_trip() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(root.clone()).await.unwrap();
    let writer = writer(&root).await;
    for (_t, ddl) in GRID_DDL {
        sqlx::query(*ddl).execute(&writer).await.unwrap();
    }
    commit_schema(&writer).await;

    let row = GridRow::builder()
        .uuid("row-1")
        .provider(Provider::Claude)
        .kind("Chat")
        .source_label("Claude")
        .is_document(true)
        .created_at(Some("2026-06-02T13:00:00-07:00".to_string()))
        .modified_at(Some("2026-06-03T09:30:00-07:00".to_string()))
        .author(Some("Jean-Luc Picard".to_string()))
        .account(Some("acct-1701".to_string()))
        .project(Some("proj-1701".to_string()))
        .org_uuid(Some("org-1701".to_string()))
        .org_name(Some("Starfleet".to_string()))
        .channel(Some("bridge".to_string()))
        .conversation_name(Some("Captain's Log".to_string()))
        .conversation_uuid("row-1")
        .message_index(Some(0))
        .entire_chat("/chat/row-1")
        .text("Stardate 47988.1")
        .slack_link(Some("slack://x".to_string()))
        .qmd_path(Some("claude-api/render_markdown/row-1.md".to_string()))
        .source_url(Some("https://claude.ai/chat/row-1".to_string()))
        .git_sha(Some("abc123".to_string()))
        .upstream_id(Some("row-1".to_string()))
        .upstream_entity_kind(Some("conversation".to_string()))
        .upstream_scope(Some("org-1701".to_string()))
        .notion_page_uuid(Some("page-1".to_string()))
        .notion_block_uuid(Some("block-1".to_string()))
        .markdown_uuid(Some("row-1".to_string()))
        .byte_size(Some(4096))
        .item_count(Some(7))
        .build()
        .unwrap();
    // The builder has no setters for these: a diff marks a finished row.
    let row = GridRow {
        diff_status: Some("modified".to_string()),
        diff_changed_columns: Some("text".to_string()),
        ..row
    };
    // The INSERT the index itself uses, from the derived column list —
    // so this test cannot drift from the DDL either.
    let columns = std::iter::once(GridRow::ID_COLUMN)
        .chain(GridRow::TYPED_COLUMNS.iter().copied())
        .collect::<Vec<_>>();
    let placeholders = vec!["?"; columns.len()].join(", ");
    let sql = format!(
        "INSERT INTO grid_rows ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&writer)
        .await
        .unwrap();

    let rows = repo.search(&parse_query(""), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    let wire = serde_json::to_value(&rows[0]).unwrap();
    // Filled by the applet from the config, or only by a free-text
    // search: absent from a repo's own answer by design.
    let not_the_repos: [&str; 3] = ["provider_ref", "source_ref", "score"];
    for key in not_the_repos {
        assert!(wire.get(key).is_none(), "{key}: {wire}");
    }
    for (key, value) in wire.as_object().unwrap() {
        let blank = value.is_null() || value.as_str().is_some_and(str::is_empty);
        assert!(
            !blank,
            "SearchRow.{key} came back blank from a fully populated row: \
             is its column in SEARCH_ROW_COLUMNS?"
        );
    }
    assert_eq!(wire["is_document"], true);
    assert_eq!(wire["modified_at"], "2026-06-03T09:30:00-07:00");
    drop(repo);
}
