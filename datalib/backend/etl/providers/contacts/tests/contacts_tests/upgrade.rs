//! A store written while what could not be stored sat in `dav_unstored`
//! and nothing listed what the server had opens under this build
//! listing every card it holds from a server, held at its etag, and
//! owing what the old table named: rung 4 of `schema_raw::LADDER`.

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::download_problems::RecordProblem;
use datalib_etl::run_problems;
use datalib_etl::stop::StopFlag;
use datalib_etl_contacts::ingest::db::{addressbook_pk, ContactRow};
use datalib_etl_contacts::ingest::schema_raw::full_ddl;
use datalib_etl_contacts::ingest::{db_path_for, RawDb};
use datalib_etl_web::dav::state;

const OLD_UNSTORED_DDL: &str = "CREATE TABLE IF NOT EXISTS dav_unstored (
    collection TEXT NOT NULL, href TEXT NOT NULL, detail TEXT NOT NULL,
    PRIMARY KEY (collection, href))";

fn card(uid: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{uid}\r\nEND:VCARD\r\n")
}

async fn strings(pool: &sqlx::SqlitePool, sql: &'static str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_listing_opens_holding_what_it_had_and_owing_the_rest() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let book = addressbook_pk(
        "carddav.enterprise.test",
        "/dav/addressbooks/user/p/Default/",
    );
    let href = |uid: &str| format!("/dav/addressbooks/user/p/Default/{uid}.vcf");
    {
        let mut ddl: Vec<String> = full_ddl()
            .into_iter()
            .filter(|d| !d.contains("dav_resources"))
            .collect();
        ddl.push(OLD_UNSTORED_DDL.to_string());
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = dr::open(&path, &ddl).await.unwrap();
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = pool.begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[
                ContactRow::new(
                    book.clone(),
                    "tng-picard".into(),
                    href("tng-picard"),
                    Some("\"p1\"".into()),
                    Some("tng-picard".into()),
                    None,
                    &card("tng-picard"),
                ),
                // A `.vcf` file's card: an href but no etag.
                ContactRow::new(
                    "local!Bridge.vcf".into(),
                    "tng-riker".into(),
                    "Bridge.vcf".into(),
                    None,
                    Some("tng-riker".into()),
                    None,
                    &card("tng-riker"),
                ),
            ],
            &now,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO dav_unstored (collection, href, detail) VALUES (?, ?, ?)")
            .bind(&book)
            .bind(href("tng-data"))
            .bind("the vCard has no UID, so it cannot be stored")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let stop = StopFlag::new();
        run_problems::collecting(&pool, &stop, |found| async move {
            found.records_failed([RecordProblem::new(
                "contacts",
                &href("tng-data"),
                "the vCard has no UID, so it cannot be stored",
            )]);
            found.records_tried_all("contacts");
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap();
        dr::commit_run(&pool, "an earlier build").await.unwrap();
        pool.close().await;
    }

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let listed: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT collection, href, etag FROM dav_resources ORDER BY href")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        listed,
        [
            (book.clone(), href("tng-data"), None),
            (book.clone(), href("tng-picard"), Some("\"p1\"".into())),
        ],
        "a server's cards are listed; a file's is not"
    );
    let held: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version, last_error \
         FROM dav_resources_bookkeeping ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        held,
        [
            (
                state::resource_id(&book, &href("tng-data")),
                false,
                None,
                Some("the vCard has no UID, so it cannot be stored".into())
            ),
            (
                state::resource_id(&book, &href("tng-picard")),
                true,
                Some("\"p1\"".into()),
                None
            ),
        ]
    );
    let owed: Vec<String> = state::owed(pool, &book)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.href)
        .collect();
    assert_eq!(owed, [href("tng-data")], "only what was never stored");
    assert!(
        strings(pool, "SELECT scope_key FROM problems")
            .await
            .is_empty(),
        "the old record rows go; the next attempt writes the row that stands for it"
    );
    let tables = strings(
        pool,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'dav_unstored'",
    )
    .await;
    assert!(tables.is_empty(), "dav_unstored is gone");
    assert_eq!(
        strings(
            pool,
            "SELECT value FROM _datalib_meta WHERE key = 'schema_version'"
        )
        .await,
        ["4"]
    );
    db.close().await;
}
