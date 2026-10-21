//! The Google Drive download against the TNG tape (`tape.rs`).

use std::io::Read as _;
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_google_drive::ingest::{self, api, db_path_for, FetchSummary, RawDb};
use datalib_etl_google_drive_config::{
    ExportFormat, Exports, FileKind, GoogleDriveApi, PerKindExports,
};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::interrupt::dump_tables;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::json_response;

use crate::tape::{self, Edition, DECK, DOC, SHEET};

pub const TABLES: &[&str] = &[
    "files",
    "revisions",
    "comments",
    "captures",
    "capture_parts",
];

pub async fn run(playback: &Path, store: &Path) -> FetchSummary {
    run_with(playback, store, GoogleDriveApi::default()).await
}

async fn run_with(playback: &Path, store: &Path, api: GoogleDriveApi) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let download = ingest::fetch(ingest::FetchOptions {
        db: db.clone(),
        latchkey: LatchkeySettings::default(),
        api,
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    });
    let summary = playback::scope(playback, download)
        .await
        .expect("fetch under playback");
    db.commit_all("test").await.expect("commit");
    db.close().await;
    summary
}

async fn open(store: &Path) -> RawDb {
    RawDb::open(&db_path_for(store)).await.expect("open store")
}

async fn scalar(db: &RawDb, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap()
}

async fn parts_of(db: &RawDb, capture: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT part, blake3 FROM capture_parts WHERE capture_id = ? ORDER BY part")
        .bind(capture)
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn blob(db: &RawDb, blake3: &str) -> Vec<u8> {
    db.cas().get(blake3).await.unwrap().expect("blob").bytes
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_captures_every_file_in_its_native_form() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    let s = run(&t, &store).await;
    assert_eq!(
        (s.listed, s.owed, s.captured, s.failed, s.left),
        (3, 3, 3, 0, 0),
        "{s:?}"
    );
    assert_eq!(
        (s.exports, s.exports_declined, s.images),
        (2, 1, 2),
        "{s:?}"
    );

    let db = open(&store).await;
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM files").await, 3);
    // The Sheet is read-only to this account, so Drive lists it no
    // revisions and none is asked for.
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM revisions WHERE file_id = '1PicardLogSupplemental'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM revisions WHERE file_id = '1LaForgeDutyRoster'"
        )
        .await,
        0
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM comments").await, 2);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM comments WHERE payload->>'$.deleted' = 1"
        )
        .await,
        1,
        "a deleted comment is kept, marked"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM files WHERE typeof(payload) = 'blob'"
        )
        .await,
        3,
        "payloads are stored as JSONB"
    );

    // The Doc's capture: its native JSON byte for byte, its image by
    // where the JSON points at it, its export.
    let (version, head, native_rev): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT version, head_revision_id, native_revision_id FROM captures WHERE file_id = ?",
    )
    .bind(DOC)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(version, "12");
    assert_eq!(head.as_deref(), Some("29"));
    assert_eq!(native_rev.as_deref(), Some("ALm37BW-rev12"));
    let parts = parts_of(&db, &format!("{DOC}#12")).await;
    let names: Vec<&str> = parts.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "export.odt",
            "image:/tabs/0/documentTab/inlineObjects/kix.enterprise/inlineObjectProperties/\
             embeddedObject/imageProperties/contentUri",
            "native.json.gz",
        ]
    );
    let native = blob(&db, parts[2].1.as_deref().unwrap()).await;
    assert_eq!(
        gunzip(&native),
        json_response(&tape::doc_native(Edition::Before)).body,
        "the native JSON is kept exactly as Google sent it"
    );
    assert_eq!(
        blob(&db, parts[1].1.as_deref().unwrap()).await,
        tape::ENTERPRISE_PNG
    );
    assert_eq!(
        blob(&db, parts[0].1.as_deref().unwrap()).await,
        tape::DOC_ODT_BEFORE
    );

    // The deck's export was declined: its part has no bytes, the capture
    // is held, and a warning says why.
    let deck = parts_of(&db, &format!("{DECK}#30")).await;
    assert_eq!(deck[0], ("export.odp".to_string(), None));
    assert!(deck
        .iter()
        .any(|(p, b)| p == "native.json.gz" && b.is_some()));
    let (severity, detail): (String, String) =
        sqlx::query_as("SELECT severity, sample FROM problems WHERE scope_key = ?")
            .bind(format!("files:{DECK}"))
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(severity, "warning");
    assert!(detail.contains("odp"), "{detail}");

    let held: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, held_version FROM files_bookkeeping ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(
        held,
        vec![
            (SHEET.to_string(), Some("7".to_string())),
            (DOC.to_string(), Some("12".to_string())),
            (DECK.to_string(), Some("30".to_string())),
        ]
    );
    db.close().await;
}

/// A file whose `version` has not moved is not fetched again, whatever
/// else its listing entry says: an export is not byte-stable, so a
/// second one would store a copy of the same version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_account_costs_the_listing_and_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(&tape::tape(d.path(), Edition::Before), &store).await;
    let db = open(&store).await;
    let before = dump_tables(db.pool(), TABLES).await.unwrap();
    db.close().await;

    // A tape with the listing and nothing else: any capture would miss.
    let only_listing = d.path().join("only-listing");
    tape::listing(
        &only_listing,
        vec![
            tape::entry(
                DOC,
                FileKind::Document,
                12,
                "Captain's log, supplemental",
                2,
            ),
            tape::entry(
                SHEET,
                FileKind::Spreadsheet,
                7,
                "Engineering duty roster",
                2,
            ),
        ],
        vec![tape::entry(
            DECK,
            FileKind::Presentation,
            30,
            "Away team briefing",
            2,
        )],
    );
    let s = run(&only_listing, &store).await;
    assert_eq!(
        (s.listed, s.owed, s.captured, s.failed),
        (3, 0, 0, 0),
        "{s:?}"
    );
    let db = open(&store).await;
    assert_eq!(dump_tables(db.pool(), TABLES).await.unwrap(), before);
    db.close().await;
}

/// A new version is a new capture beside the old one, which stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_version_is_captured_beside_the_old_one() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(&tape::tape(d.path(), Edition::Before), &store).await;
    let s = run(&tape::tape(d.path(), Edition::After), &store).await;
    assert_eq!((s.owed, s.captured, s.failed), (1, 1, 0), "{s:?}");

    let db = open(&store).await;
    let captures: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT version, native_revision_id FROM captures WHERE file_id = ? ORDER BY id",
    )
    .bind(DOC)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        captures,
        vec![
            ("12".to_string(), Some("ALm37BW-rev12".to_string())),
            ("13".to_string(), Some("ALm37BW-rev13".to_string())),
        ]
    );
    let old = parts_of(&db, &format!("{DOC}#12")).await;
    let new = parts_of(&db, &format!("{DOC}#13")).await;
    assert_ne!(old[2].1, new[2].1, "the two versions' native JSON differ");
    assert_eq!(
        old[1].1, new[1].1,
        "the unchanged image is one blob in the CAS, named by both"
    );
    assert_eq!(
        blob(&db, old[0].1.as_deref().unwrap()).await,
        tape::DOC_ODT_BEFORE,
        "the old export is still there"
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM comments").await, 3);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM revisions WHERE file_id = '1PicardLogSupplemental'"
        )
        .await,
        3
    );
    db.close().await;
}

/// latchkey signs each kind's native API with its own service, so a
/// refusal on one (no sign-in, a scope not granted) ends that kind and
/// leaves the others to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kind_whose_service_is_refused_stops_alone() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    tape::put(
        &t,
        api::req_bytes(&api::native_url(FileKind::Spreadsheet, SHEET)),
        tape::bytes(
            403,
            br#"{"error": {"code": 403, "message": "Request had insufficient authentication scopes.",
                "status": "PERMISSION_DENIED",
                "details": [{"reason": "ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
        ),
    );
    let store = d.path().join("store");
    let s = run(&t, &store).await;
    assert_eq!((s.captured, s.left), (2, 1), "{s:?}");

    let db = open(&store).await;
    let held: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM files_bookkeeping WHERE held_version IS NOT NULL ORDER BY id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(held, vec![DOC.to_string(), DECK.to_string()]);
    let said: String = sqlx::query_scalar(
        "SELECT sample FROM problems WHERE scope_key LIKE 'phase%google-sheets'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        said.contains("latchkey auth browser google-sheets"),
        "{said}"
    );
    db.close().await;
}

/// Each kind is exported only in the formats its own list names; an
/// empty list asks Drive for no export at all, so a capture holds the
/// native JSON and its images alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_follow_each_kinds_list() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    let api = GoogleDriveApi {
        exports: Exports::PerKind(PerKindExports {
            document: vec![ExportFormat::Odf],
            spreadsheet: vec![],
            presentation: vec![],
        }),
        ..Default::default()
    };
    let s = run_with(&t, &store, api).await;
    assert_eq!(
        (s.captured, s.exports, s.exports_declined),
        (3, 1, 0),
        "{s:?}"
    );

    let db = open(&store).await;
    let exports: Vec<String> =
        sqlx::query_scalar("SELECT id FROM capture_parts WHERE part LIKE 'export.%' ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(exports, vec![format!("{DOC}#12#export.odt")]);
    let names: Vec<String> = parts_of(&db, &format!("{SHEET}#7"))
        .await
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(names, vec!["native.json.gz".to_string()]);
    db.close().await;
}
