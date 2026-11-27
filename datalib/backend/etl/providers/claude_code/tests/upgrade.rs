//! A store written while records were keyed by their uuid alone opens
//! under this build holding every record, keyed `(transcript_id, id)`
//! and still upserted by id: rung 1 of `schema_raw::LADDER`.

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow as _};
use datalib_etl::store_handle::RawStoreHandle as _;
use datalib_etl_agent_sessions::{raw_ddl, RECORDS_BY_TRANSCRIPT};
use datalib_etl_claude_code::ingest::schema_raw::{RecordRow, TranscriptRow};
use datalib_etl_claude_code::ingest::{db_path_for, RawDb};
use datalib_sql::{test_sql, Sql};

/// `records` as the builds before the rung declared it.
const RECORDS_BY_UUID: &str = "CREATE TABLE IF NOT EXISTS records (
    id             TEXT PRIMARY KEY,
    payload        TEXT NULL,
    transcript_id  TEXT NOT NULL
)";

async fn strings(pool: &sqlx::SqlitePool, sql: Sql) -> Vec<String> {
    sql.query_scalar::<String>().fetch_all(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_keyed_by_uuid_opens_keyed_by_transcript_holding_every_record() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = raw_ddl(
        TranscriptRow::ddl(),
        RECORDS_BY_UUID.to_string(),
        vec![RECORDS_BY_TRANSCRIPT.to_string()],
    );
    let old: Vec<&str> = old.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &old).await.unwrap();
    test_sql!(
        "INSERT INTO records (id, payload, transcript_id) VALUES \
         ('r-2', jsonb('{\"uuid\": \"r-2\", \"text\": \"Engage.\"}'), 'picard'), \
         ('r-1', jsonb('{\"uuid\": \"r-1\", \"text\": \"Make it so.\"}'), 'riker'), \
         ('r-3', jsonb('{\"uuid\": \"r-3\", \"text\": \"Aye, sir.\"}'), 'picard')"
    )
    .query()
    .execute(&pool)
    .await
    .unwrap();
    dr::commit_run(&pool, "records keyed by uuid")
        .await
        .unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.unwrap();
    let pool = db.pool();
    assert_eq!(
        strings(
            pool,
            test_sql!(
                "SELECT transcript_id || '/' || id || ' ' || json(payload) FROM records \
             ORDER BY transcript_id, id"
            ),
        )
        .await,
        [
            "picard/r-2 {\"uuid\":\"r-2\",\"text\":\"Engage.\"}",
            "picard/r-3 {\"uuid\":\"r-3\",\"text\":\"Aye, sir.\"}",
            "riker/r-1 {\"uuid\":\"r-1\",\"text\":\"Make it so.\"}",
        ]
    );
    assert_eq!(
        dr::primary_key_columns(pool, "records").await.unwrap(),
        ["transcript_id", "id"]
    );
    assert_eq!(
        strings(
            pool,
            test_sql!(
                "SELECT name || ':' || \"unique\" FROM pragma_index_list('records') \
             WHERE origin = 'c' ORDER BY name"
            ),
        )
        .await,
        ["records_by_id:1"],
        "the uuid index is unique and the old transcript index is gone"
    );

    // A write still lands on the record by its id.
    let mut tx = pool.begin().await.unwrap();
    bulk_upsert_entity_in_tx(
        &mut tx,
        &[RecordRow {
            id_and_payload: WirePayload {
                id: "r-1".into(),
                payload: "{\"uuid\": \"r-1\", \"text\": \"Make it so, Number One.\"}".into(),
            },
            transcript_id: "riker".into(),
        }],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        strings(
            pool,
            test_sql!("SELECT json(payload) ->> '$.text' FROM records WHERE id = 'r-1'")
        )
        .await,
        ["Make it so, Number One."]
    );
    assert_eq!(
        strings(pool, test_sql!("SELECT id FROM records"))
            .await
            .len(),
        3
    );
    db.close_all().await;
}
