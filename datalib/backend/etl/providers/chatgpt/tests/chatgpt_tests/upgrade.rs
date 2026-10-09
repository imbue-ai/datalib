//! A store written while a conversation was held by its row's
//! `update_time` and an attachment by its edge's `blake3` opens under
//! this build holding everything it held and owing exactly what it had
//! not fetched: rung 1 of `schema_raw::LADDER`.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, WirePayloadRow as _};
use datalib_etl_chatgpt::ingest::schema_raw::{ConversationAttachmentRow, ConversationRow, MeRow};
use datalib_etl_chatgpt::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed::{self, Listed};
use datalib_problems::Reason;

/// The sidecar as the build before `held_version` wrote it.
fn old_bookkeeping_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (id TEXT PRIMARY KEY, \
         fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, \
         last_attempt_at_utc TEXT NULL, last_error TEXT NULL, \
         volatile_payload TEXT NULL, tz_offset TEXT NULL)"
    )
}

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
    let mut ddl = vec![MeRow::ddl(), ConversationRow::ddl()];
    ddl.extend(ConversationAttachmentRow::all_ddl());
    for table in ["me", "conversations", "chatgpt_attachments"] {
        ddl.push(old_bookkeeping_ddl(table));
    }
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();

    // c-picard fetched at a float update_time, with one file landed, one
    // chatgpt.com no longer has and one that failed; c-riker never
    // fetched at all.
    sqlx::query(
        "INSERT INTO conversations (id, title, update_time, payload) \
         VALUES ('c-picard', 'Captain''s log', '1710959331.420159', jsonb('{}'))",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (file, blake3) in [
        ("f-log", Some("ab".repeat(32))),
        ("f-lost", None),
        ("f-fail", None),
    ] {
        sqlx::query(
            "INSERT INTO chatgpt_attachments (id, conversation_id, file_id, blake3) \
             VALUES (?, 'c-picard', ?, ?)",
        )
        .bind(ConversationAttachmentRow::pk_recipe("c-picard", file))
        .bind(file)
        .bind(blake3)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    dr::record_object_attempt(&mut tx, "conversations", "c-picard", None)
        .await
        .unwrap();
    dr::record_object_error(&mut tx, "conversations", "c-riker", "HTTP 500")
        .await
        .unwrap();
    dr::record_object_attempt(&mut tx, "chatgpt_attachments", "c-picard#f-log", None)
        .await
        .unwrap();
    dr::record_object_skipped(
        &mut tx,
        "chatgpt_attachments",
        "c-picard#f-lost",
        Reason::NotFound,
        "file metadata: HTTP 404",
    )
    .await
    .unwrap();
    dr::record_object_error(
        &mut tx,
        "chatgpt_attachments",
        "c-picard#f-fail",
        "HTTP 500",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let keys = |l: Vec<Listed>| l.into_iter().map(|l| l.key).collect::<Vec<_>>();
    let listing = vec![
        Listed::new("c-picard", Some("1710959331")),
        Listed::new("c-riker", Some("1710959000")),
    ];
    assert_eq!(
        keys(owed::owed(pool, "conversations", listing).await.unwrap()),
        ["c-riker"],
        "a conversation fetched is held at its update_time in whole seconds"
    );
    assert_eq!(
        keys(
            owed::owed(
                pool,
                "chatgpt_attachments",
                db.attachments_listed().await.unwrap()
            )
            .await
            .unwrap()
        ),
        ["c-picard#f-fail"],
        "a file landed or gone is held; one that failed is owed"
    );
    assert_eq!(
        strings(pool, "SELECT scope_key FROM problems ORDER BY scope_key").await,
        [
            "chatgpt_attachments:c-picard#f-fail",
            "chatgpt_attachments:c-picard#f-lost",
            "conversations:c-riker",
        ],
        "every row stands: the gone file's is its warning"
    );
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
