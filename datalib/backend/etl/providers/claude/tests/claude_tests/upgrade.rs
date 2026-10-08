//! A store written while a conversation and a project were held by
//! their row's `updated_at`, a project's docs by a sweep marker alone
//! and an attachment by its edge's `blake3` opens under this build
//! holding everything it held and owing exactly what it had not
//! fetched: rung 1 of `schema_raw::LADDER`.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, WirePayloadRow as _};
use datalib_etl_claude::ingest::schema_raw::{
    ConversationAttachmentRow, ConversationRow, OrgRow, ProjectDocRow, ProjectRow, UserRow,
};
use datalib_etl_claude::ingest::{db_path_for, RawDb};
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

const T1: &str = "2369-01-01T00:00:00Z";
const T2: &str = "2369-01-02T00:00:00Z";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_sidecar_held_anything_opens_holding_what_it_had() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut ddl = vec![
        UserRow::ddl(),
        OrgRow::ddl(),
        ProjectRow::ddl(),
        ProjectDocRow::ddl(),
        ConversationRow::ddl(),
    ];
    ddl.extend(ConversationAttachmentRow::all_ddl());
    for table in [
        "users",
        "orgs",
        "projects",
        "project_docs",
        "conversations",
        "claude_attachments",
    ] {
        ddl.push(old_bookkeeping_ddl(table));
    }
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();

    // c-picard fetched, with one file landed, one claude.ai no longer
    // has and one that failed; c-riker never fetched. p-bridge's docs
    // were listed; p-holo's listing failed, so it has no marker.
    sqlx::query(
        "INSERT INTO conversations (id, org_uuid, updated_at, payload) \
         VALUES ('c-picard', 'org-a', ?, jsonb('{}'))",
    )
    .bind(T1)
    .execute(&pool)
    .await
    .unwrap();
    for project in ["p-bridge", "p-holo"] {
        sqlx::query(
            "INSERT INTO projects (id, org_uuid, updated_at, payload) \
             VALUES (?, 'org-a', ?, jsonb('{}'))",
        )
        .bind(project)
        .bind(T2)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (file, blake3) in [
        ("f-log", Some("ab".repeat(32))),
        ("f-lost", None),
        ("f-fail", None),
    ] {
        sqlx::query(
            "INSERT INTO claude_attachments (id, conversation_uuid, file_uuid, blake3) \
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
    for (table, id) in [
        ("conversations", "c-picard"),
        ("projects", "p-bridge"),
        ("projects", "p-holo"),
        ("claude_attachments", "c-picard#f-log"),
    ] {
        dr::record_object_attempt(&mut tx, table, id, None)
            .await
            .unwrap();
    }
    dr::record_object_error(&mut tx, "conversations", "c-riker", "HTTP 500")
        .await
        .unwrap();
    dr::record_object_skipped(
        &mut tx,
        "claude_attachments",
        "c-picard#f-lost",
        Reason::NotFound,
        "HTTP 404, claude.ai no longer has it",
    )
    .await
    .unwrap();
    dr::record_object_error(&mut tx, "claude_attachments", "c-picard#f-fail", "HTTP 500")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    dr::upsert_scope_state(
        &pool,
        "claude:sweep:project_docs:p-bridge",
        "2369-02-01T00:00:00Z",
    )
    .await
    .unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let keys = |l: Vec<Listed>| l.into_iter().map(|l| l.key).collect::<Vec<_>>();
    assert_eq!(
        keys(
            owed::owed(
                pool,
                "conversations",
                vec![
                    Listed::new("c-picard", Some(T1)),
                    Listed::new("c-riker", Some(T1))
                ]
            )
            .await
            .unwrap()
        ),
        ["c-riker"],
        "a conversation fetched is held at its updated_at"
    );
    let projects = vec![
        Listed::new("p-bridge", Some(T2)),
        Listed::new("p-holo", Some(T2)),
    ];
    assert!(
        owed::owed(pool, "projects", projects.clone())
            .await
            .unwrap()
            .is_empty(),
        "a project is held at its updated_at"
    );
    assert_eq!(
        keys(
            owed::owed(pool, "project_docs_listings", projects)
                .await
                .unwrap()
        ),
        ["p-holo"],
        "a docs listing with a marker is held at its project's stamp"
    );
    assert_eq!(
        keys(
            owed::owed(
                pool,
                "claude_attachments",
                db.attachments_listed().await.unwrap()
            )
            .await
            .unwrap()
        ),
        ["c-picard#f-fail"],
        "a file landed or gone is held; one that failed is owed"
    );
    assert_eq!(
        strings(pool, "SELECT scope FROM sync_scope_state ORDER BY scope").await,
        ["claude:sweep:project_docs:p-bridge"],
        "the markers stay: they still say when a listing is due"
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
