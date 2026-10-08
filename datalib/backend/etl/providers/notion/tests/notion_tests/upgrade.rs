//! A store written while a body's stamp was a column of its row, a
//! comments listing had no row at all and the search kept a mark opens
//! under this build holding everything it held, owing exactly what it
//! had not fetched, and with the mark gone: rung 1 of
//! `schema_raw::LADDER`.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl_notion::ingest::schema_raw::{
    NotionAttachmentRow, COMMENTS_DDL, COMMENT_ANCHORS_DDL, PAGES_DDL, USERS_DDL,
};
use datalib_etl_notion::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed;
use serde_json::json;

use crate::support::*;

const OLD_PAGE_MARKDOWN_DDL: &str = "CREATE TABLE IF NOT EXISTS page_markdown (
    id TEXT PRIMARY KEY, markdown TEXT NULL, truncated INTEGER NOT NULL DEFAULT 0,
    unresolved_block_ids TEXT NULL, source_last_edited_time TEXT NULL)";

/// The sidecar as the build before `held_version` wrote it.
fn old_bookkeeping_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (id TEXT PRIMARY KEY, \
         fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, \
         last_attempt_at_utc TEXT NULL, last_error TEXT NULL, \
         volatile_payload TEXT NULL, tz_offset TEXT NULL)"
    )
}

const PICARD: &str = "1701d000-0000-4000-8000-00000000aa01";
const RIKER: &str = "1701d000-0000-4000-8000-00000000aa02";
const SLOT_A: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/a.png";
const SLOT_B: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/b.png";

async fn strings(pool: &sqlx::SqlitePool, sql: &'static str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_sidecar_held_anything_opens_holding_what_it_had() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut ddl = vec![
        PAGES_DDL.to_string(),
        OLD_PAGE_MARKDOWN_DDL.to_string(),
        COMMENTS_DDL.to_string(),
        COMMENT_ANCHORS_DDL.to_string(),
        USERS_DDL.to_string(),
        old_bookkeeping_ddl("pages"),
        old_bookkeeping_ddl("page_markdown"),
    ];
    ddl.extend(NotionAttachmentRow::all_ddl());
    for table in ["comments", "comment_anchors", "users", "notion_attachments"] {
        ddl.push(dr::bookkeeping_ddl_for(table));
    }
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();
    let edge_a = NotionAttachmentRow::pk_recipe(BRIDGE, SLOT_A);
    let edge_b = NotionAttachmentRow::pk_recipe(BRIDGE, SLOT_B);
    let bridge = json!({"id": BRIDGE, "object": "page",
        "created_by": {"object": "user", "id": PICARD},
        "last_edited_by": {"object": "user", "id": RIKER}});
    sqlx::query("INSERT INTO pages (id, last_edited_time, payload) VALUES (?, ?, jsonb(?))")
        .bind(BRIDGE)
        .bind(EDITED)
        .bind(bridge.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO page_markdown (id, markdown, source_last_edited_time) VALUES (?, ?, ?)",
    )
    .bind(BRIDGE)
    .bind("Captain's log.\n")
    .bind(EDITED)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO pages (id, last_edited_time, payload) VALUES (?, ?, jsonb(?))")
        .bind(SICKBAY)
        .bind(EDITED)
        .bind(json!({"id": SICKBAY, "object": "page"}).to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id, name, payload) VALUES (?, ?, jsonb(?))")
        .bind(PICARD)
        .bind("Jean-Luc Picard")
        .bind(json!({"id": PICARD}).to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO notion_attachments (id, page_id, ref_id, blake3) VALUES (?, ?, ?, ?)")
        .bind(&edge_a)
        .bind(BRIDGE)
        .bind(SLOT_A)
        .bind("ab".repeat(32))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO notion_attachments (id, page_id, ref_id) VALUES (?, ?, ?)")
        .bind(&edge_b)
        .bind(BRIDGE)
        .bind(SLOT_B)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    // The bridge fetched whole; sickbay's object fetched, its comments
    // listing failed and its body never came; the holodeck's object
    // never came at all; one user fetched and one not; one attachment
    // fetched and one not.
    for (table, id) in [
        ("pages", BRIDGE),
        ("page_markdown", BRIDGE),
        ("pages", SICKBAY),
        ("users", PICARD),
        ("notion_attachments", edge_a.as_str()),
    ] {
        dr::record_object_attempt(&mut tx, table, id, None)
            .await
            .unwrap();
    }
    for (table, id, err) in [
        ("pages", SICKBAY, "comments: HTTP 500"),
        ("page_markdown", SICKBAY, "HTTP 500"),
        ("pages", HOLODECK, "HTTP 500"),
        ("users", RIKER, "HTTP 500"),
        ("notion_attachments", edge_b.as_str(), "HTTP 500"),
    ] {
        dr::record_object_error(&mut tx, table, id, err)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    dr::upsert_scope_state(&pool, "workspace", EDITED)
        .await
        .unwrap();
    // The table an older build declared, and its record.
    sqlx::query("CREATE TABLE IF NOT EXISTS sync_scope_config (scope TEXT PRIMARY KEY, config TEXT NOT NULL, updated_at_utc TEXT NOT NULL, tz_offset TEXT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sync_scope_config VALUES ('notion:download', '{\"refresh_window_days\":0}', '2369-04-15T00:00:00Z', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let held = |table: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, bool, Option<String>)>(sqlx::AssertSqlSafe(format!(
                "SELECT id, fetched_at_utc IS NOT NULL, held_version \
                 FROM {table}_bookkeeping ORDER BY id"
            )))
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let at = |id: &str, version: &str| (id.to_string(), true, Some(version.to_string()));
    assert_eq!(
        held("pages").await,
        [at(BRIDGE, EDITED), at(SICKBAY, EDITED)],
        "a stub whose object never came goes; the rest are held at their stamp"
    );
    assert_eq!(
        strings(pool, "SELECT id FROM pages ORDER BY id").await,
        [BRIDGE, SICKBAY]
    );
    assert_eq!(
        held("page_markdown").await,
        [at(BRIDGE, EDITED), (SICKBAY.to_string(), false, None)],
        "a body is held at the stamp its row carried"
    );
    assert_eq!(
        held("page_comments").await,
        [at(BRIDGE, EDITED)],
        "a listing that failed is not held"
    );
    let has_column: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('page_markdown') \
         WHERE name = 'source_last_edited_time')",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(!has_column, "the stamp column is gone");

    let listed = db.pages_listed().await.unwrap();
    let keys = |l: Vec<owed::Listed>| l.into_iter().map(|l| l.key).collect::<Vec<_>>();
    assert_eq!(
        keys(
            owed::owed(pool, "page_markdown", listed.clone())
                .await
                .unwrap()
        ),
        [SICKBAY]
    );
    assert_eq!(
        keys(owed::owed(pool, "page_comments", listed).await.unwrap()),
        [SICKBAY]
    );
    assert_eq!(
        keys(
            owed::owed(pool, "users", db.users_listed().await.unwrap())
                .await
                .unwrap()
        ),
        [RIKER]
    );
    assert_eq!(
        keys(
            owed::owed(
                pool,
                "notion_attachments",
                db.attachments_listed().await.unwrap()
            )
            .await
            .unwrap()
        ),
        [edge_b.as_str()]
    );
    assert_eq!(
        strings(pool, "SELECT scope_key FROM problems ORDER BY scope_key").await,
        [
            format!("notion_attachments:{edge_b}"),
            format!("page_markdown:{SICKBAY}"),
            format!("users:{RIKER}"),
        ],
        "the marks on page rows go; what is still owed keeps its row"
    );
    assert!(strings(pool, "SELECT scope FROM coverage").await.is_empty());
    assert!(strings(pool, "SELECT scope FROM sync_scope_state")
        .await
        .is_empty());
    let retired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sync_scope_config'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(retired, 0, "the retired table is dropped on open");
    assert_eq!(
        strings(
            pool,
            "SELECT value FROM _datalib_meta WHERE key = 'schema_version'"
        )
        .await,
        ["1"]
    );
    db.close().await;
}
