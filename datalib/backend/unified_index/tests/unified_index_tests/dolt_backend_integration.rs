//! End-to-end integration test for the doltlite backend.

use datalib_schema::grid_rows::{GridRow, DDL as GRID_DDL, INDEXES as GRID_INDEXES};
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::problems::{
    Outcome, Problem, ProblemRow, Reason, Scope, Stage, DDL as PROBLEMS_DDL,
};
use datalib_schema::providers::Provider;
use datalib_table::BulkUpsertable;
use datalib_unified_index::dolt_repo::{listing_sql, DoltRepo};
use datalib_unified_index::query::{parse_query, Field};
use datalib_unified_index::repo::IndexRepo;
use datalib_unified_index::sort::Sort;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The `grid_index` step's handle on the root's index, for seeding, and
/// writing the way the step does: on the writer branch, never on `main`
/// (`etl/README.md`, "A writer works on its own branch"). The repo under
/// test opens its own, read-only, once the file exists — which is exactly
/// how the applet reads what the step wrote.
async fn writer(root: &Path) -> sqlx::SqlitePool {
    let pool = datalib_core::store::open_pool(&datalib_core::layout::grid_index_db(root))
        .await
        .expect("open a writer on the grid index");
    sqlx::query("SELECT dolt_checkout('-b', 'datalib_writer')")
        .execute(&pool)
        .await
        .expect("put the writer on its branch");
    pool
}

/// What the step does after its DDL, and again after every batch: commit
/// on its branch, then publish by moving `main` (`commit_run`). The repo
/// reads `main`, so a table or a row that was never published is not
/// there to read.
async fn commit(writer: &sqlx::SqlitePool, what: &str) {
    sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', ?)")
        .bind(what)
        .fetch_one(writer)
        .await
        .expect("dolt_commit");
    sqlx::query("SELECT dolt_branch('-f', 'main', 'datalib_writer')")
        .execute(writer)
        .await
        .expect("publish main");
}

/// The INSERT the index itself uses, from the derived column list, so the
/// load-time columns (`touched_at_utc`, `source_id`, …) are computed as
/// the step computes them and the test cannot drift from the DDL.
async fn insert_rows(writer: &sqlx::SqlitePool, rows: &[GridRow]) {
    let columns = std::iter::once(GridRow::ID_COLUMN)
        .chain(GridRow::TYPED_COLUMNS.iter().copied())
        .collect::<Vec<_>>();
    let placeholders = vec!["?"; columns.len()].join(", ");
    let sql = format!(
        "INSERT INTO grid_rows ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    for row in rows {
        row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql.clone())))
            .execute(writer)
            .await
            .expect("insert grid row");
    }
}

/// A document row: a chat, filed under the path's first segment.
fn chat_row(uuid: &str, qmd_path: &str) -> GridRow {
    chat_row_at(uuid, qmd_path, "2026-04-01T10:00:00+00:00")
}

fn chat_row_at(uuid: &str, qmd_path: &str, created_at: &str) -> GridRow {
    GridRow::builder()
        .uuid(uuid)
        .provider(Provider::Claude)
        .kind("Chat")
        .source_label("Claude")
        .is_document(true)
        .created_at(Some(created_at.to_string()))
        .account(Some("acct-a".to_string()))
        .conversation_name(Some("Test conv".to_string()))
        .conversation_uuid(uuid)
        .entire_chat(format!("/chat/{uuid}"))
        .body("summary")
        .qmd_path(Some(qmd_path.to_string()))
        .source_url(Some(format!("https://claude.ai/chat/{uuid}")))
        .markdown_uuid(Some(uuid.to_string()))
        .build()
        .expect("a valid chat row")
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
    let listing = repo
        .filter_uuids(&parse_query(""), &["c-1".into()], None)
        .await
        .unwrap();
    assert!(
        listing.uuids.is_empty(),
        "expected no rows, got {listing:?}"
    );
    assert!(repo
        .rows_by_uuids(&["c-1".into()])
        .await
        .unwrap()
        .is_empty());
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
    commit(&writer, "schema").await;
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
    commit(&writer, "schema").await;
    // For Anthropic chats the rendered file is 1:1 with the
    // conversation, so markdown_uuid == conversation_uuid here.
    let message = GridRow::builder()
        .uuid("m-1")
        .provider(Provider::Claude)
        .kind("User Input")
        .source_label("Claude")
        .created_at(Some("2026-04-01T10:01:00+00:00".to_string()))
        .author(Some("acct-a".to_string()))
        .account(Some("acct-a".to_string()))
        .conversation_name(Some("Test conv".to_string()))
        .conversation_uuid("c-1")
        .message_index(Some(0))
        .entire_chat("/chat/c-1")
        .body("hello there")
        .markdown_uuid(Some("c-1".to_string()))
        .build()
        .unwrap();
    insert_rows(&writer, &[chat_row("c-1", "chats/c-1.md"), message]).await;
    sqlx::query(
        "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind, md_path, \
         renderer_version) \
         VALUES ('c-1','test','claude','chat','chats/c-1.md','test-v1')",
    )
    .execute(&writer)
    .await
    .expect("insert markdown row");
    commit(&writer, "rows").await;

    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows, got {rows:?}");
    // Newest first: the message came a minute after the chat began.
    assert_eq!(rows[0].kind, "User Input");
    assert_eq!(rows[1].kind, "Chat");

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
    // Filed as the grid files the row: by the path's first segment.
    assert_eq!(meta.source_id.as_deref(), Some("chats"));
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

/// A search's rows are listed once, as uuids, and every page is read by
/// uuid: the listing is in the order asked for, a qmd ranking keeps its
/// own order unless a sort replaces it, and the rows come back in the
/// order they were asked for, whatever order the table holds them in.
#[tokio::test]
async fn a_listing_orders_filters_and_reads_back_by_uuid() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let writer = writer(&root).await;
    create_grid_tables(&writer).await;
    insert_rows(
        &writer,
        &[
            chat_row_at("c-old", "enterprise/a.md", "2026-01-01T09:00:00+00:00"),
            chat_row_at("c-new", "enterprise/b.md", "2026-03-01T09:00:00+00:00"),
            chat_row_at("c-mid", "enterprise/c.md", "2026-02-01T09:00:00+00:00"),
            chat_row_at("v-1", "voyager/d.md", "2026-02-15T09:00:00+00:00"),
        ],
    )
    .await;
    commit(&writer, "rows").await;
    let repo = DoltRepo::open(root.clone()).await.unwrap();
    let head = repo
        .head()
        .await
        .unwrap()
        .expect("a committed index has a head");

    let newest_first = repo.ordered_uuids(&parse_query(""), None).await.unwrap();
    assert_eq!(newest_first.uuids, ["c-new", "v-1", "c-mid", "c-old"]);
    assert_eq!(newest_first.at.as_deref(), Some(head.as_str()));

    let enterprise = parse_query("source_id:enterprise");
    let oldest_first = repo
        .ordered_uuids(&enterprise, Sort::parse("created_at:asc"))
        .await
        .unwrap();
    assert_eq!(oldest_first.uuids, ["c-old", "c-mid", "c-new"]);

    // A qmd ranking, with a row from another source and one the index
    // does not have.
    let ranked: Vec<String> = ["c-mid", "v-1", "gone", "c-old", "c-new"]
        .map(String::from)
        .into();
    let in_rank_order = repo.filter_uuids(&enterprise, &ranked, None).await.unwrap();
    assert_eq!(in_rank_order.uuids, ["c-mid", "c-old", "c-new"]);
    let by_score = |s| repo.filter_uuids(&enterprise, &ranked, Sort::parse(s));
    assert_eq!(by_score("score:desc").await.unwrap(), in_rank_order);
    assert_eq!(
        by_score("score:asc").await.unwrap().uuids,
        ["c-new", "c-old", "c-mid"]
    );
    let resorted = repo
        .filter_uuids(&enterprise, &ranked, Sort::parse("created_at:desc"))
        .await
        .unwrap();
    assert_eq!(resorted.uuids, ["c-new", "c-mid", "c-old"]);

    let asked: Vec<String> = ["c-old", "gone", "v-1"].map(String::from).into();
    let rows = repo.rows_by_uuids(&asked).await.unwrap();
    let got: Vec<&str> = rows.iter().map(|r| r.uuid.as_str()).collect();
    assert_eq!(got, ["c-old", "v-1"]);

    insert_rows(&writer, &[chat_row("c-later", "enterprise/e.md")]).await;
    commit(&writer, "more rows").await;
    assert_ne!(
        repo.head().await.unwrap().as_deref(),
        Some(head.as_str()),
        "a seal moves the head, so a cached listing is not reused past it"
    );

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}

async fn create_grid_tables(writer: &sqlx::SqlitePool) {
    for (_t, ddl) in GRID_DDL.iter().chain(MARKDOWNS_DDL.iter()) {
        sqlx::query(*ddl)
            .execute(writer)
            .await
            .expect("create table");
    }
}

async fn insert_chat_row(writer: &sqlx::SqlitePool, uuid: &str) {
    insert_rows(writer, &[chat_row(uuid, "chats/x.md")]).await;
}

/// The repo reads what the step committed, never what it is writing: a
/// row that has reached the working set but not a commit is not there,
/// and appears at the commit. Under streaming the working set holds a
/// whole batch between the step's SQL `COMMIT` and its `dolt_commit`
/// (`doltlite_two_process_test` measures that window), and this is what
/// keeps the grid from serving it.
#[tokio::test]
async fn a_row_the_step_has_not_committed_is_not_served() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let writer = writer(&root).await;
    create_grid_tables(&writer).await;
    commit(&writer, "schema").await;
    let repo = DoltRepo::open(root.clone()).await.unwrap();

    insert_chat_row(&writer, "c-1").await;
    let before = repo.search(&parse_query(""), 100).await.unwrap();
    assert!(before.is_empty(), "served an uncommitted row: {before:?}");
    assert!(repo.chat_meta("c-1").await.unwrap().is_none());

    commit(&writer, "rows").await;
    let after = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(repo.chat_meta("c-1").await.unwrap().is_some());

    drop(repo);
    let _ = std::fs::remove_file(&db_path);
}

/// The order a fresh root has: the applet is up and answering before the
/// step's first pass creates the tables and commits them. The repo's
/// read-only connection opened before those tables existed, and has to
/// see them at the next read rather than answer "no rows" for the rest of
/// its life.
#[tokio::test]
async fn a_repo_opened_before_the_first_commit_reads_after_it() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(root.clone()).await.unwrap();
    let writer = writer(&root).await;
    create_grid_tables(&writer).await;
    // Opens the handle: the file exists, the tables are not committed.
    assert!(repo.search(&parse_query(""), 100).await.unwrap().is_empty());

    insert_chat_row(&writer, "c-1").await;
    commit(&writer, "first pass").await;
    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");

    // And a table that arrives later — a newer build adding one — the
    // same way.
    sqlx::query(
        "CREATE TABLE edges (edge_uuid TEXT PRIMARY KEY, src_markdown_uuid TEXT, \
                 src_anchor_uuid TEXT, dst_markdown_uuid TEXT, dst_anchor_uuid TEXT, label TEXT)",
    )
    .execute(&writer)
    .await
    .unwrap();
    sqlx::query("INSERT INTO edges VALUES ('e-1','c-1',NULL,'c-2',NULL,'cites')")
        .execute(&writer)
        .await
        .unwrap();
    commit(&writer, "edges").await;
    let edges = repo.outgoing_edges("c-1").await.unwrap();
    assert_eq!(edges.len(), 1, "{edges:?}");

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
    commit(&writer, "schema").await;
    // Both rows sit under `claude-work/render_markdown/`: the chat is
    // that source's data, the measurement is datalib describing it.
    let storage = GridRow::builder()
        .uuid("s-1")
        .provider(Provider::Datalib)
        .kind("Store")
        .source_label("Storage")
        .created_at(Some("2026-04-01T10:00:00+00:00".to_string()))
        .account(Some("claude-work".to_string()))
        .conversation_uuid("s-1")
        .entire_chat("/chat/s-1")
        .body("claude-work/ingest/entities.doltlite_db")
        .qmd_path(Some(
            "claude-work/render_markdown/_datalib/storage.md".to_string(),
        ))
        .markdown_uuid(Some("s-1".to_string()))
        .build()
        .unwrap();
    insert_rows(
        &writer,
        &[
            chat_row("c-1", "claude-work/render_markdown/chats/c-1.md"),
            storage,
        ],
    )
    .await;
    commit(&writer, "rows").await;

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
    commit(&writer, "schema").await;

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
        .body("Stardate 47988.1")
        .qmd_path(Some("claude-api/render_markdown/row-1.md".to_string()))
        .source_url(Some("https://claude.ai/chat/row-1".to_string()))
        .git_sha(Some("abc123".to_string()))
        .upstream_id(Some("row-1".to_string()))
        .upstream_entity_kind(Some("conversation".to_string()))
        .upstream_account(Some("org-1701".to_string()))
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
    insert_rows(&writer, &[row]).await;
    commit(&writer, "rows").await;

    let rows = repo.search(&parse_query(""), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    let wire = serde_json::to_value(&rows[0]).unwrap();
    // Filled by the applet from the config, or only by a free-text
    // search: absent from a repo's own answer by design.
    let not_the_repos: [&str; 2] = ["source_ref", "score"];
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

/// The query that exercises a filter key, or `None` for a key no index
/// can serve in newest-first order. Exhaustive, so a new key does not
/// compile until it says which it is.
fn query_for(field: &Field) -> Option<&'static str> {
    match field {
        Field::Source => Some("source:Claude"),
        Field::SourceId => Some("source_id:slack"),
        Field::Kind => Some("kind:Chat"),
        Field::Channel => Some("channel:bridge"),
        Field::Is => Some("is:document"),
        Field::Convo => Some("convo:00000000-0000-8000-8000-000000000001"),
        Field::Author => Some("author:picard"),
        Field::Account => Some("account:acct-1701"),
        Field::Project => Some("project:proj-1701"),
        Field::NotionPage => Some("notion_page:00000000-0000-8000-8000-000000000002"),
        Field::Change => Some("change:added"),
        // Ranges on `created_at_utc`, which cannot share an index with a
        // newest-first order: a `before:` far back walks the sort index.
        Field::Before | Field::After => None,
        // Not filters: free text goes to qmd, an unknown key matches nothing.
        Field::Subj | Field::Other(_) => None,
    }
}

/// A filter the search bar offers must be served by an index in the
/// order the grid sorts by. Without one, a filter that matches few rows
/// walks the whole newest-first index row by row: 38 s for one row among
/// 74k (`docs/dev/plans/paged_grids.md`). Fails naming the query whose
/// plan scans the table or sorts it in a temporary B-tree, and naming an
/// index no query uses.
#[tokio::test]
async fn every_filter_key_is_served_by_an_index() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let writer = writer(&root).await;
    for (_t, ddl) in GRID_DDL.iter().chain(GRID_INDEXES.iter()) {
        sqlx::query(*ddl).execute(&writer).await.expect("create");
    }
    let every_field = [
        Field::Before,
        Field::After,
        Field::Subj,
        Field::Source,
        Field::SourceId,
        Field::Kind,
        Field::Channel,
        Field::Is,
        Field::Convo,
        Field::Author,
        Field::Account,
        Field::Project,
        Field::NotionPage,
        Field::Change,
        Field::Other(String::new()),
    ];
    let queries = every_field
        .iter()
        .filter_map(query_for)
        // The unfiltered grid, its inverse, and what a Browse card asks.
        .chain(["", "-is:document", "source_id:slack is:document"]);
    let mut unserved: Vec<String> = Vec::new();
    let mut used: std::collections::BTreeSet<String> = Default::default();
    for q in queries {
        let (sql, params) = listing_sql(&parse_query(q), None);
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(explain));
        for p in &params {
            query = query.bind(p.clone());
        }
        let plan: Vec<String> = query
            .fetch_all(&writer)
            .await
            .expect("explain")
            .iter()
            .map(|r| sqlx::Row::get::<String, _>(r, "detail"))
            .collect();
        // A filter must SEARCH an index on its own column. SCANning the
        // newest-first index in order and testing each row also avoids a
        // sort, and is exactly the walk this test exists to catch; only
        // the unfiltered grid may do it.
        let served = if q.is_empty() {
            plan.iter().any(|d| {
                d.starts_with("SCAN grid_rows USING") && d.contains("grid_rows_by_touched")
            })
        } else {
            plan.iter().any(|d| {
                d.starts_with("SEARCH grid_rows USING") && d.contains("INDEX grid_rows_by_")
            })
        };
        let sorts = plan.iter().any(|d| d.contains("TEMP B-TREE"));
        if !served || sorts {
            unserved.push(format!("{q:?}: {plan:?}"));
        }
        for detail in &plan {
            if let Some(rest) = detail.split("INDEX ").nth(1) {
                used.insert(rest.split_whitespace().next().unwrap_or("").to_string());
            }
        }
    }
    // The other direction: an index no query plans with is a cost every
    // write pays for nothing.
    let unused: Vec<&str> = GRID_INDEXES
        .iter()
        .filter_map(|(_t, ddl)| ddl.split_whitespace().nth(5))
        .filter(|name| !used.contains(*name))
        .collect();
    assert!(
        unused.is_empty(),
        "no filter key plans with these: {unused:?}"
    );
    assert!(
        unserved.is_empty(),
        "no index serves these in newest-first order:\n{}",
        unserved.join("\n")
    );
}

/// The problems banner and table read the index's `problems` through the
/// same read transaction as the grid: a problem the step committed is
/// there, by query and by document.
#[tokio::test]
async fn a_committed_problem_is_read_back_by_query_and_by_document() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let repo = DoltRepo::open(root.clone()).await.unwrap();
    let writer = writer(&root).await;
    for (_t, ddl) in GRID_DDL.iter().chain(PROBLEMS_DDL.iter()) {
        sqlx::query(*ddl).execute(&writer).await.unwrap();
    }
    let row = ProblemRow::new(
        "slack",
        Stage::GridRow,
        Scope::Markdown("md-1"),
        Some("row-1"),
        Outcome::Nulled,
        Problem::field("created_at", Reason::CoercionFailed, "yesterday"),
        Some(1),
    );
    let columns = std::iter::once(ProblemRow::ID_COLUMN)
        .chain(ProblemRow::TYPED_COLUMNS.iter().copied())
        .collect::<Vec<_>>();
    let sql = format!(
        "INSERT INTO problems ({}) VALUES ({})",
        columns.join(", "),
        vec!["?"; columns.len()].join(", ")
    );
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&writer)
        .await
        .unwrap();
    commit(&writer, "problems").await;

    let all = repo
        .problems(&datalib_unified_index::problems::parse(""), 10)
        .await
        .unwrap();
    assert_eq!(all, vec![row.clone()]);
    assert_eq!(repo.document_problems("md-1").await.unwrap(), vec![row]);
    assert!(repo.document_problems("md-2").await.unwrap().is_empty());
}
