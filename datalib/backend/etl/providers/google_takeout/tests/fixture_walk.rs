//! End-to-end fixture walk: point the extractor at the checked-in
//! TNG-themed Takeout tree and assert each feed lands the rows
//! the provider's INGEST.md promises.

use std::path::{Path, PathBuf};

use datalib_etl::progress::Progress;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_google_takeout::ingest::{self, FetchOptions, RawDb, SyncFlags};

fn fixture_root() -> PathBuf {
    let rel =
        std::env::var("TAKEOUT_FIXTURE_DIR").expect("TAKEOUT_FIXTURE_DIR must be set by the build");
    // Under `bazel test` the runfiles root is CWD; under `cargo test`
    // the env var is repo-relative from the workspace root.
    let p = PathBuf::from(&rel);
    if p.is_dir() {
        return p;
    }
    let up = PathBuf::from("../../../../..").join(&rel);
    assert!(up.is_dir(), "fixture dir not found: {rel}");
    up
}

/// A temp cache per run: tests must never touch this host's real one.
async fn opts(work: &Path, db: &RawDb, sync: SyncFlags) -> FetchOptions {
    FetchOptions {
        db: db.clone(),
        input_path: fixture_root(),
        cache: FingerprintCache::open(&work.join("fingerprints.sqlite"))
            .await
            .unwrap(),
        sync,
        progress: Progress::noop(),
        control: Default::default(),
    }
}

async fn run_all() -> (tempfile::TempDir, ingest::FetchSummary, PathBuf) {
    let work = tempfile::tempdir().unwrap();
    let db_path = work.path().join("gt.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    let summary = ingest::fetch(opts(work.path(), &db, SyncFlags::all()).await)
        .await
        .unwrap();
    // Closed, not dropped: every caller reopens this store, and a
    // dropped pool is still a live connection for a moment.
    db.commit_all("test").await.unwrap();
    db.close().await;
    (work, summary, db_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_reviews_lands_two_rows() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.maps_reviews, 2);
    let db = RawDb::open(&db_path).await.unwrap();
    let rows = db.load_payloads("maps_reviews").await.unwrap();
    assert_eq!(rows.len(), 2);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|v| {
            v.get("properties")
                .and_then(|p| p.get("location"))
                .and_then(|l| l.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(names.iter().any(|n| n == "Ten Forward"));
    assert!(names.iter().any(|n| n == "Resort Lounge"));
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_saved_places_handles_ftid_cid_and_an_address_query() {
    let (_work, summary, _db_path) = run_all().await;
    assert_eq!(summary.maps_saved_places, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_photo_lands_row_and_blob() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.maps_photos, 1);
    let db = RawDb::open(&db_path).await.unwrap();
    let rows = db.load_payloads("maps_photos").await.unwrap();
    assert_eq!(rows.len(), 1);
    // blake3 column populated from JPEG bytes.
    let blake3: Option<String> = sqlx::query_scalar("SELECT blake3 FROM maps_photos WHERE id = ?")
        .bind("2026-06-04-tenfwd.jpg")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let blake3 = blake3.expect("blake3 populated");
    assert_eq!(blake3.len(), 64);
    // CAS has the bytes.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cas_objects WHERE blake3 = ?)")
            .bind(&blake3)
            .fetch_one(db.cas().pool())
            .await
            .unwrap();
    assert!(exists, "photo bytes in CAS");
}

#[tokio::test(flavor = "multi_thread")]
async fn youtube_subscriptions_handles_quoted_titles() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.youtube_subscriptions, 3);
    let db = RawDb::open(&db_path).await.unwrap();
    let title: String = sqlx::query_scalar(
        "SELECT channel_title FROM youtube_subscriptions WHERE id = 'UCriker002'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(title, "Riker, William T.");
}

#[tokio::test(flavor = "multi_thread")]
async fn youtube_watch_history_parses_cells_and_timestamps() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.youtube_watch_history, 3);
    let db = RawDb::open(&db_path).await.unwrap();
    // video_id promoted column populated for each row.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM youtube_watch_history WHERE video_id IS NOT NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 3);
    let when: Option<String> = sqlx::query_scalar(
        "SELECT when_ts FROM youtube_watch_history WHERE video_id = 'trekS01E01'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    // Parsed PDT-relative; the rfc3339 wallclock is the local 11:48
    // turned into a -07:00 offset.
    assert!(when.unwrap().starts_with("2026-06-04T11:48:37"));
}

/// The fixture's third entry has a multi-byte character just where the
/// timestamp look-back starts, which once panicked the whole ingest.
#[tokio::test(flavor = "multi_thread")]
async fn a_multibyte_char_before_the_timestamp_lands_with_it() {
    let html = std::fs::read_to_string(fixture_root().join(WATCH_HISTORY)).unwrap();
    let cell = ingest::mdl_html::iter_cells(&html)
        .find(|c| c.contains("trekS04E02"))
        .unwrap();
    let text = ingest::mdl_html::strip_tags(cell);
    assert!(
        !text.is_char_boundary(text.find(" AM ").unwrap() - 30),
        "the fixture must put the look-back inside the 'ü'"
    );

    let (_work, _summary, db_path) = run_all().await;
    let db = RawDb::open(&db_path).await.unwrap();
    let when: Option<String> = sqlx::query_scalar(
        "SELECT when_ts FROM youtube_watch_history WHERE video_id = 'trekS04E02'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(when.unwrap().starts_with("2026-06-06T09:00:00"));
}

#[tokio::test(flavor = "multi_thread")]
async fn google_chat_lands_groups_users_messages_and_attachments() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.chat_groups, 1);
    assert_eq!(summary.chat_users, 1);
    assert_eq!(summary.chat_messages, 2);
    // The second is named by the message and absent from the export.
    assert_eq!(summary.chat_attachments, 2);
    let db = RawDb::open(&db_path).await.unwrap();
    // The DM group key is the takeout directory name verbatim.
    let group_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM chat_groups ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(group_ids, vec!["DM TNG-BRIDGE"]);
    // The fetched attachment's edge row has the CAS blake3 set.
    let blake3: Option<String> = sqlx::query_scalar(
        "SELECT blake3 FROM chat_attachments WHERE export_name = 'course-laid-in.txt'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let blake3 = blake3.expect("blake3 set");
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM cas_objects WHERE blake3 = ?")
        .bind(&blake3)
        .fetch_one(db.cas().pool())
        .await
        .unwrap();
    let s = String::from_utf8(bytes).unwrap();
    assert!(s.contains("Course 314"));
}

/// The fixture's cells are laid out as Google writes them: "Prompted …",
/// optional "N generated image." and "Attached N file." lines, the date,
/// then the response. Read with a guess at the layout, the prompt took in
/// the response and the footer, and the response lost its first paragraph.
#[tokio::test(flavor = "multi_thread")]
async fn gemini_apps_splits_each_cell_into_prompt_and_response() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.gemini_activity, 3);
    let db = RawDb::open(&db_path).await.unwrap();
    let rows = db.load_payloads("gemini_activity").await.unwrap();
    let by_prompt = |prompt: &str| {
        rows.iter()
            .find(|v| v["promptText"] == prompt)
            .unwrap_or_else(|| panic!("no cell prompted {prompt:?}: {rows:#?}"))
    };

    let prime = by_prompt("Tell me about the Prime Directive. Is it ever waived?");
    let response = prime["responseHtml"].as_str().unwrap();
    assert!(
        response.starts_with("<p>The Prime Directive (General Order 1)"),
        "{response}"
    );
    assert!(response.contains("court of inquiry"), "{response}");
    assert!(!response.contains("Products:"), "{response}");
    assert_eq!(prime["whenStr"], "Feb 14, 2026, 11:48:37 AM PDT");
    assert_eq!(
        prime["attachedFiles"],
        serde_json::json!([{
            "file": "Prime Directive summary-1701236400000001.txt",
            "name": "Prime Directive summary.txt",
        }])
    );

    let drawn = by_prompt("Make this saucer sketch look like the Enterprise-D at warp.");
    assert_eq!(
        drawn["generatedImages"],
        serde_json::json!(["2364030112000000001-ncc1701d0e5f6a7b.jpeg"])
    );
    by_prompt("How does a warp coil work?");

    let when_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gemini_activity WHERE when_ts IS NOT NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(when_count, 3);
}

/// Every file a cell names lands with its bytes: a link Google
/// percent-encoded, the image a response drew, and that image written to
/// disk under another extension than the one the page names it by.
#[tokio::test(flavor = "multi_thread")]
async fn gemini_apps_stores_every_file_a_cell_names() {
    let e = Export::new();
    let s = e.sync().await;
    assert_eq!(s.gemini_attachments, 3, "{s:?}");
    assert_eq!(e.keys().await, Vec::<String>::new());

    let db = RawDb::open(&e.db_path).await.unwrap();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT filename, blake3 FROM gemini_attachments ORDER BY filename")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "2364030112000000001-ncc1701d0e5f6a7b.jpeg",
            "Prime Directive summary-1701236400000001.txt",
            "saucer-sketch-0c1d2e3f40516273.jpg",
        ]
    );
    assert!(rows.iter().all(|r| r.1.is_some()), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_run_skips_via_file_checkpoint() {
    let (work, summary1, db_path) = run_all().await;
    assert!(summary1.maps_reviews > 0);
    assert!(summary1.youtube_watch_history > 0);

    // Same fixture root, freshly opened db pool — cursor rows from
    // the first run mean every file's fingerprint matches and the
    // walkers short-circuit.
    let db = RawDb::open(&db_path).await.unwrap();
    let summary2 = ingest::fetch(opts(work.path(), &db, SyncFlags::all()).await)
        .await
        .unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;
    let _ = work; // keep temp dir alive

    assert_eq!(summary2.maps_reviews, 0);
    assert_eq!(summary2.maps_saved_places, 0);
    assert_eq!(summary2.youtube_watch_history, 0);
    assert_eq!(summary2.youtube_subscriptions, 0);
    assert_eq!(summary2.chat_messages, 0);
    assert_eq!(summary2.gemini_activity, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_flags_default_disables_everything() {
    let work = tempfile::tempdir().unwrap();
    let db_path = work.path().join("gt.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    // Default SyncFlags has every feed off.
    let summary = ingest::fetch(opts(work.path(), &db, SyncFlags::default()).await)
        .await
        .unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;
    assert_eq!(summary.maps_reviews, 0);
    assert_eq!(summary.youtube_subscriptions, 0);
    assert_eq!(summary.chat_messages, 0);
    assert_eq!(summary.gemini_activity, 0);
}

/// Voice's `Bills.html` sits at `Voice/Bills.html` under the export root;
/// matching it as a bare `Bills.html` never found it.
#[tokio::test(flavor = "multi_thread")]
async fn google_voice_lands_the_bills() {
    let (_work, summary, _db_path) = run_all().await;
    assert!(summary.voice_bills > 0, "{summary:?}");
}

// ── #898: a file that is gone takes its records with it ─────────────

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

/// A private copy of the fixture export, a store, and a cache, so a test
/// can delete files between syncs.
struct Export {
    work: tempfile::TempDir,
    root: PathBuf,
    db_path: PathBuf,
}

impl Export {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("Takeout");
        std::fs::create_dir_all(&root).unwrap();
        copy_tree(&fixture_root(), &root);
        let db_path = work.path().join("gt.doltlite_db");
        Self {
            work,
            root,
            db_path,
        }
    }

    async fn sync(&self) -> ingest::FetchSummary {
        let db = RawDb::open(&self.db_path).await.unwrap();
        let summary = ingest::fetch(FetchOptions {
            input_path: self.root.clone(),
            ..opts(self.work.path(), &db, SyncFlags::all()).await
        })
        .await
        .unwrap();
        db.commit_all("test").await.unwrap();
        db.close().await;
        summary
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel)).unwrap();
    }

    async fn count(&self, table: &str) -> i64 {
        let db = RawDb::open(&self.db_path).await.unwrap();
        let n = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(db.pool())
            .await
            .unwrap();
        db.close().await;
        n
    }
}

const MESSAGES: &str = "Google Chat/Groups/DM TNG-BRIDGE/messages.json";

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_chat_file_takes_its_records() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("chat_messages").await, 2);
    assert_eq!(e.count("chat_attachments").await, 2);

    e.remove(MESSAGES);
    let s = e.sync().await;
    assert_eq!(s.removed, 2);
    assert_eq!(e.count("chat_messages").await, 0);
    assert_eq!(e.count("chat_attachments").await, 0);
    assert_eq!(
        e.count("chat_groups").await,
        1,
        "group_info.json is still there"
    );

    e.remove("Google Chat/Groups/DM TNG-BRIDGE/group_info.json");
    e.remove("Google Chat/Users/User 1234567890/user_info.json");
    e.sync().await;
    assert_eq!(e.count("chat_groups").await, 0);
    assert_eq!(e.count("chat_users").await, 0);
}

/// A group's `messages.json` is all of its messages, so one a re-read
/// file no longer carries is gone, attachment edge and all.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_dropped_from_a_reread_file_is_gone() {
    let e = Export::new();
    e.sync().await;

    let path = e.root.join(MESSAGES);
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["messages"].as_array_mut().unwrap().pop();
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 1);
    assert_eq!(e.count("chat_messages").await, 1);
    assert_eq!(
        e.count("chat_attachments").await,
        0,
        "T2 carried the attachment"
    );
}

/// A `messages.json` with no `messages` array lists nothing, which is not
/// the same as listing no messages.
#[tokio::test(flavor = "multi_thread")]
async fn a_messages_file_without_a_list_deletes_nothing() {
    let e = Export::new();
    e.sync().await;

    std::fs::write(e.root.join(MESSAGES), b"{}").unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 0);
    assert_eq!(e.count("chat_messages").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_maps_photo_sidecar_takes_its_row() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("maps_photos").await, 1);

    e.remove("Maps/Photos and videos/2026-06-04-tenfwd.jpg.json");
    let s = e.sync().await;
    assert_eq!(s.removed, 1);
    assert_eq!(e.count("maps_photos").await, 0);
}

/// Voice records are keyed by content, so a gone file costs a read of the
/// rest, and only what no remaining file holds goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_voice_file_takes_only_its_records() {
    let e = Export::new();
    e.sync().await;
    let before = e.count("voice_messages").await;
    let bills = e.count("voice_bills").await;
    assert!(bills > 0);

    e.remove("Voice/Calls/Wesley Crusher - Missed - 2364-03-03T11_00_00Z.html");
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("voice_messages").await, before - 1);
    assert_eq!(e.count("voice_bills").await, bills);

    e.remove("Voice/Bills.html");
    e.sync().await;
    assert_eq!(e.count("voice_bills").await, 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_error_deletes_nothing() {
    let e = Export::new();
    e.sync().await;

    e.remove(MESSAGES);
    e.remove("Voice/Calls/Wesley Crusher - Missed - 2364-03-03T11_00_00Z.html");
    std::os::unix::fs::symlink(e.root.join("nowhere"), e.root.join("dangling.json")).unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 0);
    assert_eq!(e.count("chat_messages").await, 2);
}

// ── A single-file feed's file is the whole of its table ─────────────

const REVIEWS: &str = "Maps (your places)/Reviews.json";
const SAVED: &str = "Maps (your places)/Saved Places.json";
const SUBSCRIPTIONS: &str = "YouTube and YouTube Music/subscriptions/subscriptions.csv";
const WATCH_HISTORY: &str = "YouTube and YouTube Music/history/watch-history.html";
const GEMINI: &str = "My Activity/Gemini Apps/MyActivity.html";

impl Export {
    fn rewrite(&self, rel: &str, edit: impl FnOnce(String) -> String) {
        let path = self.root.join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, edit(text)).unwrap();
    }
}

fn drop_first_feature(json: String) -> String {
    let mut doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    doc["features"].as_array_mut().unwrap().remove(0);
    doc.to_string()
}

fn drop_last_line(csv: String) -> String {
    let mut lines: Vec<&str> = csv.lines().collect();
    lines.pop();
    lines.join("\n") + "\n"
}

/// Cut the first of the MDL activity cells Takeout's HTML feeds are made of.
fn drop_first_cell(html: String) -> String {
    let marker = "<div class=\"outer-cell";
    let first = html.find(marker).unwrap();
    let second = first + marker.len() + html[first + marker.len()..].find(marker).unwrap();
    format!("{}{}", &html[..first], &html[second..])
}

fn drop_last_cell(html: String) -> String {
    let last = html.rfind("<div class=\"outer-cell").unwrap();
    let end = html.rfind("</body>").unwrap();
    format!("{}{}", &html[..last], &html[end..])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_dropped_from_a_newer_export_is_gone() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("maps_reviews").await, 2);

    e.rewrite(REVIEWS, drop_first_feature);
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("maps_reviews").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_place_dropped_from_a_newer_export_is_gone() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("maps_saved_places").await, 3);

    e.rewrite(SAVED, drop_first_feature);
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("maps_saved_places").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_dropped_from_a_newer_export_is_gone() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("youtube_subscriptions").await, 3);

    e.rewrite(SUBSCRIPTIONS, drop_last_line);
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("youtube_subscriptions").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_dropped_from_a_newer_export_is_gone() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("youtube_watch_history").await, 3);

    e.rewrite(WATCH_HISTORY, drop_first_cell);
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("youtube_watch_history").await, 2);
}

/// The dropped cell is the one with the attachment, so its CAS edge must
/// go with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_gemini_activity_dropped_from_a_newer_export_is_gone_with_its_attachment() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("gemini_activity").await, 3);
    assert_eq!(e.count("gemini_attachments").await, 3);

    e.rewrite(GEMINI, drop_first_cell);
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "{s:?}");
    assert_eq!(e.count("gemini_activity").await, 2);
    assert_eq!(e.count("gemini_attachments").await, 2);
}

/// A reviews file with no `features` list says nothing about which reviews
/// exist, which is not the same as listing none.
#[tokio::test(flavor = "multi_thread")]
async fn a_reviews_file_without_a_list_deletes_nothing() {
    let e = Export::new();
    e.sync().await;

    e.rewrite(REVIEWS, |_| "{}".to_string());
    let s = e.sync().await;
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("maps_reviews").await, 2);
}

/// A single-file feed whose file is missing deletes nothing: an export
/// requested without that product looks exactly like this.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_single_file_feed_deletes_nothing() {
    let e = Export::new();
    e.sync().await;

    for rel in [REVIEWS, SAVED, SUBSCRIPTIONS, WATCH_HISTORY, GEMINI] {
        e.remove(rel);
    }
    let s = e.sync().await;
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("maps_reviews").await, 2);
    assert_eq!(e.count("maps_saved_places").await, 3);
    assert_eq!(e.count("youtube_subscriptions").await, 3);
    assert_eq!(e.count("youtube_watch_history").await, 3);
    assert_eq!(e.count("gemini_activity").await, 3);
}

// ── A product missing from the export deletes nothing ───────────────

const PRODUCTS: [&str; 3] = ["Google Chat", "Voice", "Maps/Photos and videos"];

impl Export {
    /// Move a product's folder out of the export, as a Takeout requested
    /// without it would be; returns where it went.
    fn set_aside(&self, rel: &str) -> PathBuf {
        let aside = self.work.path().join("aside").join(rel);
        std::fs::create_dir_all(aside.parent().unwrap()).unwrap();
        std::fs::rename(self.root.join(rel), &aside).unwrap();
        aside
    }

    fn put_back(&self, rel: &str, aside: &Path) {
        std::fs::rename(aside, self.root.join(rel)).unwrap();
    }
}

/// An export requested without Chat, Voice or Maps photos looks exactly
/// like one whose product was emptied, so a missing product folder is
/// read as "not exported", never as "deleted".
#[tokio::test(flavor = "multi_thread")]
async fn a_product_missing_from_the_export_deletes_nothing() {
    let e = Export::new();
    e.sync().await;
    let tables = [
        "chat_users",
        "chat_groups",
        "chat_messages",
        "chat_attachments",
        "voice_messages",
        "voice_bills",
        "maps_photos",
    ];
    let mut before = Vec::new();
    for t in tables {
        before.push(e.count(t).await);
    }

    for p in PRODUCTS {
        e.set_aside(p);
    }
    let s = e.sync().await;
    assert_eq!(s.removed, 0, "{s:?}");
    for (t, n) in tables.iter().zip(before) {
        assert_eq!(e.count(t).await, n, "{t}");
    }
}

/// Holding the deletions back keeps the cursor, so a product that comes
/// back smaller still loses what it dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_product_that_returns_smaller_loses_what_it_dropped() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("chat_messages").await, 2);

    let aside = e.set_aside("Google Chat");
    e.sync().await;
    std::fs::remove_file(aside.join("Groups/DM TNG-BRIDGE/messages.json")).unwrap();
    e.put_back("Google Chat", &aside);
    let s = e.sync().await;
    assert_eq!(s.removed, 2, "{s:?}");
    assert_eq!(e.count("chat_messages").await, 0);
    assert_eq!(e.count("chat_groups").await, 1);
}

// ── What a run could not do is a problem until it succeeds ──────────

const MAPS_PHOTO_SIDECAR: &str = "Maps/Photos and videos/2026-06-04-tenfwd.jpg.json";
const MAPS_PHOTO_MEDIA: &str = "Maps/Photos and videos/2026-06-04-tenfwd.jpg";
const CHAT_ATTACHMENT: &str = "Google Chat/Groups/DM TNG-BRIDGE/course-laid-in.txt";
const GEMINI_ATTACHMENT: &str =
    "My Activity/Gemini Apps/Prime Directive summary-1701236400000001.txt";
const VOICE_MMS: &str = "Voice/Calls/Jean-Luc Picard - Text - 2364-03-01T09_00_00Z-1-1.jpg";
const VOICE_MISSED: &str = "Voice/Calls/Wesley Crusher - Missed - 2364-03-03T11_00_00Z.html";

/// The rows the fixture leaves on a clean run, built in on purpose: a
/// saved place with no key, a watch-history entry that is not a video,
/// and a Chat attachment the export lacks.
fn from_the_fixture(key: &str) -> bool {
    key.starts_with("skipped:maps_saved_places:")
        || key.starts_with("skipped:youtube_watch_history:")
        || key == "chat_attachments:TNG-BRIDGE/T2/T2#risa-shore-leave.png"
}

impl Export {
    /// `(scope_key, severity, reason)` of every problem the test made,
    /// sorted.
    async fn problems(&self) -> Vec<(String, String, String)> {
        let db = RawDb::open(&self.db_path).await.unwrap();
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT scope_key, severity, reason FROM problems ORDER BY scope_key")
                .fetch_all(db.pool())
                .await
                .unwrap();
        db.close().await;
        rows.into_iter()
            .filter(|r| !from_the_fixture(&r.0))
            .collect()
    }

    async fn keys(&self) -> Vec<String> {
        self.problems().await.into_iter().map(|r| r.0).collect()
    }

    /// The keys of a part's `skipped:` rows end in a hash, so a
    /// test names the part.
    async fn skipped_parts(&self) -> Vec<String> {
        self.keys()
            .await
            .into_iter()
            .map(|k| match k.strip_prefix("skipped:") {
                Some(rest) => format!("skipped:{}", rest.rsplit_once(':').unwrap().0),
                None => k,
            })
            .collect()
    }

    /// Move a file out of the export; returns where it went.
    fn take(&self, rel: &str) -> PathBuf {
        let aside = self.work.path().join("taken").join(rel);
        std::fs::create_dir_all(aside.parent().unwrap()).unwrap();
        std::fs::rename(self.root.join(rel), &aside).unwrap();
        aside
    }
}

/// A feed that fails as a whole is a `phase:` row, and the rest of the
/// export still lands; the run that reads it clears the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_feed_that_fails_is_a_phase_problem_until_it_reads() {
    let e = Export::new();
    let good = std::fs::read(e.root.join(REVIEWS)).unwrap();
    std::fs::write(e.root.join(REVIEWS), b"{ not json").unwrap();
    let s = e.sync().await;
    assert_eq!(s.feeds_failed, 1, "{s:?}");
    assert!(s.maps_saved_places > 0, "the other feeds ran: {s:?}");
    assert_eq!(e.keys().await, ["phase:maps_reviews"]);

    std::fs::write(e.root.join(REVIEWS), good).unwrap();
    e.sync().await;
    assert_eq!(e.keys().await, Vec::<String>::new());
    assert_eq!(e.count("maps_reviews").await, 2);
}

/// A sidecar that will not parse is left unstamped and named; a photo
/// whose media is not in the export lands without bytes, says so on its
/// record, and is looked for again until it is there.
#[tokio::test(flavor = "multi_thread")]
async fn a_maps_photo_that_did_not_read_is_tried_again_until_it_does() {
    let e = Export::new();
    let media = e.take(MAPS_PHOTO_MEDIA);
    e.sync().await;
    assert_eq!(
        e.problems().await,
        [(
            "maps_photos:2026-06-04-tenfwd.jpg".to_string(),
            "warning".to_string(),
            "not_found".to_string()
        )]
    );
    assert_eq!(
        e.count("maps_photos").await,
        1,
        "the row lands without bytes"
    );

    std::fs::rename(&media, e.root.join(MAPS_PHOTO_MEDIA)).unwrap();
    let s = e.sync().await;
    assert_eq!(s.blobs_stored, 1, "{s:?}");
    assert_eq!(e.keys().await, Vec::<String>::new());

    let sidecar = std::fs::read(e.root.join(MAPS_PHOTO_SIDECAR)).unwrap();
    std::fs::write(e.root.join(MAPS_PHOTO_SIDECAR), b"{").unwrap();
    e.sync().await;
    assert_eq!(e.skipped_parts().await, ["skipped:maps_photos"]);
    std::fs::write(e.root.join(MAPS_PHOTO_SIDECAR), sidecar).unwrap();
    e.sync().await;
    assert_eq!(e.keys().await, Vec::<String>::new());
}

/// One Chat file that will not parse costs that file, not the feed; an
/// attachment not in the export is a warning on its edge, tried again
/// though the `messages.json` naming it is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_file_or_attachment_that_did_not_read_is_tried_again() {
    let e = Export::new();
    let attachment = e.take(CHAT_ATTACHMENT);
    let good = std::fs::read(e.root.join(MESSAGES)).unwrap();
    std::fs::write(e.root.join(MESSAGES), b"{ not json").unwrap();
    let s = e.sync().await;
    assert_eq!(s.chat_users, 1, "the rest of the feed landed: {s:?}");
    assert_eq!(e.skipped_parts().await, ["skipped:google_chat"]);

    std::fs::write(e.root.join(MESSAGES), good).unwrap();
    e.sync().await;
    assert_eq!(e.count("chat_messages").await, 2);
    assert_eq!(
        e.problems().await,
        [(
            "chat_attachments:TNG-BRIDGE/T2/T2#course-laid-in.txt".to_string(),
            "warning".to_string(),
            "not_found".to_string()
        )]
    );

    std::fs::rename(&attachment, e.root.join(CHAT_ATTACHMENT)).unwrap();
    let s = e.sync().await;
    assert_eq!(
        s.chat_messages, 0,
        "messages.json was not read again: {s:?}"
    );
    assert_eq!(s.blobs_stored, 1, "{s:?}");
    assert_eq!(e.keys().await, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gemini_attachment_not_in_the_export_is_tried_again() {
    let e = Export::new();
    let attachment = e.take(GEMINI_ATTACHMENT);
    e.sync().await;
    let problems = e.problems().await;
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].0.starts_with("gemini_attachments:")
            && problems[0]
                .0
                .ends_with("#Prime Directive summary-1701236400000001.txt"),
        "{problems:?}"
    );
    assert_eq!(
        (problems[0].1.as_str(), problems[0].2.as_str()),
        ("warning", "not_found")
    );

    std::fs::rename(&attachment, e.root.join(GEMINI_ATTACHMENT)).unwrap();
    let s = e.sync().await;
    assert_eq!(s.gemini_activity, 0, "the activity file was not read again");
    assert_eq!(e.keys().await, Vec::<String>::new());
}

/// A Voice file whose attachment is not in the export is named and read
/// again next run, and the read that finds it clears the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_attachment_not_in_the_export_is_tried_again() {
    let e = Export::new();
    let mms = e.take(VOICE_MMS);
    e.sync().await;
    assert_eq!(e.skipped_parts().await, ["skipped:google_voice"]);
    e.sync().await;
    assert_eq!(
        e.skipped_parts().await,
        ["skipped:google_voice"],
        "still not there"
    );

    std::fs::rename(&mms, e.root.join(VOICE_MMS)).unwrap();
    let s = e.sync().await;
    assert_eq!(s.blobs_stored, 1, "{s:?}");
    assert_eq!(e.keys().await, Vec::<String>::new());
    let s = e.sync().await;
    assert_eq!(s.voice_messages, 0, "stamped once it read whole: {s:?}");
}

/// A run that holds deletions back must leave the rewritten file looking
/// rewritten, or the next run reads only what changed, deletes nothing,
/// and the record the rewrite dropped stays for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_seen_on_a_held_back_run_is_acted_on_by_the_next() {
    let e = Export::new();
    e.sync().await;
    let before = e.count("voice_messages").await;

    e.rewrite(VOICE_MISSED, |html| {
        html.replace(
            "2364-03-03T11:00:00.000-08:00",
            "2364-03-03T12:00:00.000-08:00",
        )
    });
    let bills = std::fs::read(e.root.join("Voice/Bills.html")).unwrap();
    std::fs::write(e.root.join("Voice/Bills.html"), b"\xff\xfe").unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("voice_messages").await, before + 1);
    let keys = e.keys().await;
    assert!(
        keys.contains(&"listing:removed_records".to_string()),
        "{keys:?}"
    );

    std::fs::write(e.root.join("Voice/Bills.html"), bills).unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 1, "the call the rewrite replaced goes: {s:?}");
    assert_eq!(e.count("voice_messages").await, before);
    assert_eq!(e.keys().await, Vec::<String>::new());
}

/// A stopped run reports nothing, so it clears nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_run_leaves_the_last_runs_problems() {
    let e = Export::new();
    let good = std::fs::read(e.root.join(REVIEWS)).unwrap();
    std::fs::write(e.root.join(REVIEWS), b"{ not json").unwrap();
    e.sync().await;
    assert_eq!(e.keys().await, ["phase:maps_reviews"]);

    std::fs::write(e.root.join(REVIEWS), good).unwrap();
    let db = RawDb::open(&e.db_path).await.unwrap();
    let o = FetchOptions {
        input_path: e.root.clone(),
        ..opts(e.work.path(), &db, SyncFlags::all()).await
    };
    o.control.stop.request();
    ingest::fetch(o).await.unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;
    assert_eq!(e.keys().await, ["phase:maps_reviews"]);
}

// ── one entry the parser trips on costs only itself ─────────────────

/// A feed that fails costs that feed and nothing else, and says so where
/// the Manage row reads it rather than only in the log.
#[tokio::test(flavor = "multi_thread")]
async fn a_feed_that_fails_is_a_problem_row_and_the_rest_land() {
    let e = Export::new();
    e.rewrite(SAVED, |_| "{ not json".to_string());
    let s = e.sync().await;
    assert_eq!(s.feeds_failed, 1, "{s:?}");
    assert_eq!(s.maps_saved_places, 0, "{s:?}");
    assert_eq!(s.maps_reviews, 2, "{s:?}");
    assert_eq!(s.youtube_watch_history, 3, "{s:?}");

    let db = RawDb::open(&e.db_path).await.unwrap();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT scope_key, severity, sample FROM problems WHERE scope_key LIKE 'phase:%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "phase:maps_saved_places");
    assert_eq!(rows[0].1, "error");
    assert!(rows[0].2.starts_with("parse Saved Places.json"), "{rows:?}");
}

/// An entry read and not stored once reached only the log, one identical
/// `warn!` per entry: 321 saved places with no key, 36 watch-history
/// entries that are not videos. Each is now a `problems` row.
#[tokio::test(flavor = "multi_thread")]
async fn entries_read_and_not_stored_are_problem_rows() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.maps_saved_places, 3);
    assert_eq!(summary.youtube_watch_history, 3);
    let db = RawDb::open(&db_path).await.unwrap();
    type Row = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT scope_key, severity, reason, rule, field, sample FROM problems \
         WHERE scope_key LIKE 'skipped:%' ORDER BY scope_key",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    let [place, watch] = rows.as_slice() else {
        panic!("one row per skipped entry: {rows:?}");
    };
    assert!(
        place.0.starts_with("skipped:maps_saved_places:"),
        "{place:?}"
    );
    assert_eq!(
        (place.1.as_str(), place.2.as_str()),
        ("error", "no_identity")
    );
    assert_eq!(place.4.as_deref(), Some("date"), "{place:?}");
    assert!(
        watch.0.starts_with("skipped:youtube_watch_history:"),
        "{watch:?}"
    );
    assert_eq!(watch.1, "warning");
    assert_eq!(watch.3.as_deref(), Some("youtube_watch_not_a_video"));
    assert!(watch.5.contains("/post/"), "{watch:?}");
}

/// A feed whose file is unchanged reads nothing and reports nothing, so
/// what it skipped last time must still be a row.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_file_keeps_its_skipped_rows() {
    let e = Export::new();
    e.sync().await;
    let skipped = || async {
        let db = RawDb::open(&e.db_path).await.unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM problems WHERE scope_key LIKE 'skipped:%'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        db.close().await;
        n
    };
    assert_eq!(skipped().await, 2);
    e.sync().await;
    assert_eq!(skipped().await, 2);

    e.rewrite(WATCH_HISTORY, drop_last_cell);
    e.sync().await;
    assert_eq!(
        skipped().await,
        1,
        "the post left the export, so its row goes"
    );
}

// ── A file this reader cannot read deletes nothing ───────────────────

impl Export {
    async fn phase_problems(&self) -> Vec<String> {
        let db = RawDb::open(&self.db_path).await.unwrap();
        let keys = sqlx::query_scalar(
            "SELECT scope_key FROM problems WHERE scope_key LIKE 'phase:%' ORDER BY scope_key",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        db.close().await;
        keys
    }
}

/// Every feature keeps its place in the list and loses the field the
/// reader keys it by, as if Google had renamed it.
fn rename_properties(json: String) -> String {
    json.replace("\"properties\"", "\"attributes\"")
}

fn rename_cells(html: String) -> String {
    html.replace("<div class=\"outer-cell", "<div class=\"activity-entry")
}

fn two_column_csv(_: String) -> String {
    "Channel Id,Channel Title\nUCpicard001,Captain's Log Official\n".to_string()
}

fn no_feature_list(_: String) -> String {
    r#"{"type":"FeatureCollection","items":[]}"#.to_string()
}

type Edit = fn(String) -> String;

/// One single-file feed's file, rewritten by `edit` between two syncs.
struct Rewrite {
    rel: &'static str,
    feed: &'static str,
    table: &'static str,
    rows: i64,
    edit: Edit,
}

/// A newer export in a layout this reader does not know used to read as
/// a file that lists nothing — every watch, subscription, review or
/// Gemini activity deleted — or, for a Maps file with no `features`,
/// as a quiet `warn!` with the file marked read (audit 2026-10-02 §4).
/// Now it deletes nothing and fails its feed where a person sees it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_a_layout_this_reader_does_not_know_deletes_nothing_and_says_so() {
    let reviews = |edit| Rewrite {
        rel: REVIEWS,
        feed: "maps_reviews",
        table: "maps_reviews",
        rows: 2,
        edit,
    };
    let gemini = |edit| Rewrite {
        rel: GEMINI,
        feed: "gemini_apps",
        table: "gemini_activity",
        rows: 3,
        edit,
    };
    let cases = [
        reviews(no_feature_list),
        reviews(rename_properties),
        Rewrite {
            rel: SAVED,
            feed: "maps_saved_places",
            table: "maps_saved_places",
            rows: 3,
            edit: rename_properties,
        },
        Rewrite {
            rel: SUBSCRIPTIONS,
            feed: "youtube_subscriptions",
            table: "youtube_subscriptions",
            rows: 3,
            edit: two_column_csv,
        },
        Rewrite {
            rel: WATCH_HISTORY,
            feed: "youtube_watch_history",
            table: "youtube_watch_history",
            rows: 3,
            edit: rename_cells,
        },
        gemini(rename_cells),
        gemini(|_| String::new()),
    ];
    for c in cases {
        let e = Export::new();
        e.sync().await;
        assert_eq!(e.count(c.table).await, c.rows, "{}", c.rel);

        e.rewrite(c.rel, c.edit);
        let s = e.sync().await;
        assert_eq!(s.removed, 0, "{}: {s:?}", c.rel);
        assert_eq!(e.count(c.table).await, c.rows, "{}", c.rel);
        assert_eq!(
            e.phase_problems().await,
            vec![format!("phase:{}", c.feed)],
            "{}",
            c.rel
        );
    }
}

/// The other side of that rule: a file in the known layout that lists
/// nothing is a product emptied upstream, and empties its table.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_lists_nothing_empties_its_table() {
    fn no_features(_: String) -> String {
        r#"{"type":"FeatureCollection","features":[]}"#.to_string()
    }
    fn header_only(csv: String) -> String {
        csv.lines().next().unwrap().to_string() + "\n"
    }
    let cases: [(&str, &str, Edit); 3] = [
        (REVIEWS, "maps_reviews", no_features),
        (SAVED, "maps_saved_places", no_features),
        (SUBSCRIPTIONS, "youtube_subscriptions", header_only),
    ];
    for (rel, table, edit) in cases {
        let e = Export::new();
        e.sync().await;
        e.rewrite(rel, edit);
        e.sync().await;
        assert_eq!(e.count(table).await, 0, "{rel}");
        assert_eq!(e.phase_problems().await, Vec::<String>::new(), "{rel}");
    }
}

// ── A unit read whole is replaced in one transaction ────────────────

impl Export {
    async fn exec(&self, sql: &'static str) {
        let db = RawDb::open(&self.db_path).await.unwrap();
        sqlx::query(sql).execute(db.pool()).await.unwrap();
        db.commit_all("test").await.unwrap();
        db.close().await;
    }
}

// ── A reader that changes reaches a store already synced ────────────

impl Export {
    async fn schema_version(&self) -> String {
        let db = RawDb::open(&self.db_path).await.unwrap();
        let v = sqlx::query_scalar("SELECT value FROM _datalib_meta WHERE key = 'schema_version'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        db.close().await;
        v
    }
}

/// An unchanged file is not read again, so a store an earlier build
/// synced would never meet the fixed Maps, YouTube and Gemini readers.
/// Rung 1 of the ladder forgets those feeds' files, and only theirs.
#[tokio::test(flavor = "multi_thread")]
async fn rung_1_reads_the_feeds_whose_reader_changed_again() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(
        e.schema_version().await,
        "1",
        "a new store starts at the top"
    );
    assert_eq!(e.sync().await.gemini_activity, 0, "nothing changed");

    e.exec("UPDATE _datalib_meta SET value = '0' WHERE key = 'schema_version'")
        .await;
    let s = e.sync().await;
    assert_eq!(e.schema_version().await, "1");
    assert_eq!(
        (
            s.maps_photos,
            s.maps_saved_places,
            s.youtube_watch_history,
            s.gemini_activity
        ),
        (1, 3, 3, 3),
        "{s:?}"
    );
    assert_eq!(
        (s.maps_reviews, s.youtube_subscriptions, s.chat_messages),
        (0, 0, 0),
        "a feed whose reader did not change is not read again: {s:?}"
    );
}

const VOICE_TEXT: &str = "Voice/Calls/Jean-Luc Picard - Text - 2364-03-01T09_00_00Z.html";

/// Voice stamped the files it read in one transaction and pruned what
/// they no longer held in a later one. A run that failed between the two
/// left the rewritten file stamped, so no later run read every file again
/// and the call the rewrite replaced stayed for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_run_that_fails_before_its_prune_prunes_on_the_next() {
    let e = Export::new();
    e.sync().await;
    let before = e.count("voice_messages").await;

    e.rewrite(VOICE_MISSED, |html| {
        html.replace(
            "2364-03-03T11:00:00.000-08:00",
            "2364-03-03T12:00:00.000-08:00",
        )
    });
    e.exec(
        "CREATE TRIGGER refuse_prune BEFORE DELETE ON voice_messages \
         BEGIN SELECT RAISE(ABORT, 'crash at the prune'); END",
    )
    .await;
    let failed = e.sync().await;
    assert_eq!(failed.feeds_failed, 1, "{failed:?}");
    e.exec("DROP TRIGGER refuse_prune").await;

    e.sync().await;
    assert_eq!(
        e.count("voice_messages").await,
        before,
        "the call the rewrite replaced goes"
    );
}

/// The same window in Chat: a re-read `messages.json` was stamped before
/// the messages it dropped were deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_run_that_fails_before_its_prune_prunes_on_the_next() {
    let e = Export::new();
    e.sync().await;

    let path = e.root.join(MESSAGES);
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["messages"].as_array_mut().unwrap().pop();
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    e.exec(
        "CREATE TRIGGER refuse_prune BEFORE DELETE ON chat_messages \
         BEGIN SELECT RAISE(ABORT, 'crash at the prune'); END",
    )
    .await;
    let failed = e.sync().await;
    assert_eq!(failed.feeds_failed, 1, "{failed:?}");
    e.exec("DROP TRIGGER refuse_prune").await;

    e.sync().await;
    assert_eq!(
        e.count("chat_messages").await,
        1,
        "the message the file dropped goes"
    );
}

/// A `messages.json` whose entries none carry a `message_id` read as a
/// group with no messages, and every message of the group was deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_messages_file_none_of_whose_entries_has_an_id_deletes_nothing() {
    let e = Export::new();
    e.sync().await;

    std::fs::write(
        e.root.join(MESSAGES),
        br#"{"messages": [{"text": "Engage."}, {"text": "Make it so."}]}"#,
    )
    .unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("chat_messages").await, 2);
    assert_eq!(e.skipped_parts().await, ["skipped:google_chat"]);

    std::fs::write(e.root.join(MESSAGES), br#"{"messages": []}"#).unwrap();
    let s = e.sync().await;
    assert_eq!(s.removed, 2, "a list of no messages empties the group");
    assert_eq!(e.count("chat_messages").await, 0);
}

/// A Voice file rewritten to nothing (a text thread with no message, a
/// call record with no time, a bills page with no table) read as a file
/// holding nothing, and the prune deleted what it had held.
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_file_that_is_recognizably_nothing_deletes_nothing() {
    let e = Export::new();
    e.sync().await;
    let messages = e.count("voice_messages").await;
    let bills = e.count("voice_bills").await;

    for rel in [VOICE_TEXT, VOICE_MISSED, "Voice/Bills.html"] {
        let good = std::fs::read(e.root.join(rel)).unwrap();
        std::fs::write(e.root.join(rel), b"").unwrap();
        let s = e.sync().await;
        assert_eq!(s.removed, 0, "{rel} emptied: {s:?}");
        assert_eq!(e.count("voice_messages").await, messages, "{rel}");
        assert_eq!(e.count("voice_bills").await, bills, "{rel}");
        assert!(
            e.skipped_parts()
                .await
                .contains(&"skipped:google_voice".to_string()),
            "{rel} is a problem"
        );
        std::fs::write(e.root.join(rel), good).unwrap();
    }
}

/// Reading an unchanged export again must leave the store as it was: a
/// re-stamped sidecar is a commit, and a bigger store, on every sync. The
/// Chat attachment the fixture leaves out stays out: a problem recorded
/// again unchanged changes nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn reading_an_unchanged_export_again_commits_nothing() {
    let e = Export::new();
    let mut commits = Vec::new();
    for _ in 0..2 {
        let db = RawDb::open(&e.db_path).await.unwrap();
        ingest::fetch(FetchOptions {
            input_path: e.root.clone(),
            ..opts(e.work.path(), &db, SyncFlags::all()).await
        })
        .await
        .unwrap();
        commits.push(
            datalib_etl::doltlite_raw::commit_run(db.pool(), "test")
                .await
                .unwrap(),
        );
        db.close().await;
    }
    assert!(commits[0].is_some());
    assert_eq!(
        commits[1], None,
        "reading an unchanged export again changes nothing in the store"
    );
}

/// Make `path` unreadable, or `false` where the test cannot: root reads
/// through any mode, and CI's container runs as root.
#[cfg(unix)]
fn lock(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(path).is_ok() {
        unlock(path);
        return false;
    }
    true
}

#[cfg(unix)]
fn unlock(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// A Voice file that will not open may hold any record, so a run that
/// reads the rest deletes nothing while it is unread. It was a walk
/// error before; once it was not, the prune took what only it held.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_file_that_will_not_open_holds_deletions_back() {
    let e = Export::new();
    e.sync().await;
    let before = e.count("voice_messages").await;
    let bills = e.count("voice_bills").await;

    e.rewrite(VOICE_MISSED, |html| {
        html.replace(
            "2364-03-03T11:00:00.000-08:00",
            "2364-03-03T12:00:00.000-08:00",
        )
    });
    let locked = e.root.join("Voice/Bills.html");
    if !lock(&locked) {
        return;
    }
    let s = e.sync().await;
    unlock(&locked);
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("voice_bills").await, bills);
    let keys = e.keys().await;
    assert_eq!(
        keys,
        ["listing:removed_records", "record:files:Voice/Bills.html"],
        "{keys:?}"
    );

    let s = e.sync().await;
    assert_eq!(s.removed, 1, "the call the rewrite replaced goes: {s:?}");
    assert_eq!(e.count("voice_messages").await, before);
    assert_eq!(e.keys().await, Vec::<String>::new());
}

/// A photo whose sidecar will not open is there: it keeps its row, with
/// a row of its own saying why, until the sidecar opens.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_maps_photo_sidecar_that_will_not_open_keeps_its_row() {
    let e = Export::new();
    e.sync().await;
    assert_eq!(e.count("maps_photos").await, 1);

    let rel = MAPS_PHOTO_SIDECAR;
    let locked = e.root.join(rel);
    if !lock(&locked) {
        return;
    }
    let s = e.sync().await;
    unlock(&locked);
    assert_eq!(s.removed, 0, "{s:?}");
    assert_eq!(e.count("maps_photos").await, 1);
    assert_eq!(e.keys().await, [format!("record:files:{rel}")]);

    e.sync().await;
    assert_eq!(e.keys().await, Vec::<String>::new());
}

// ── An export left zipped ───────────────────────────────────────────

/// Every fixture file as `(Takeout/<rel>, path)`, for packing into parts.
fn fixture_entries() -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let rel = format!("{rel}/{name}");
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &rel, out);
            } else {
                out.push((rel, entry.path()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&fixture_root(), "Takeout", &mut out);
    out.sort();
    out
}

/// The fixture as Google would send it in two parts: Chat and Maps in a
/// `.zip`, the rest in a `.tgz`.
fn pack_fixture(dir: &Path) {
    use std::io::Write;
    let (first, rest): (Vec<_>, Vec<_>) = fixture_entries().into_iter().partition(|(rel, _)| {
        rel.starts_with("Takeout/Google Chat/") || rel.starts_with("Takeout/Maps")
    });
    let mut zip = zip::ZipWriter::new(
        std::fs::File::create(dir.join("takeout-23640301T090000Z-1-001.zip")).unwrap(),
    );
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (rel, path) in &first {
        zip.start_file(rel.as_str(), opts).unwrap();
        zip.write_all(&std::fs::read(path).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    let gz = flate2::write::GzEncoder::new(
        std::fs::File::create(dir.join("takeout-23640301T090000Z-2-001.tgz")).unwrap(),
        flate2::Compression::fast(),
    );
    let mut tar = tar::Builder::new(gz);
    for (rel, path) in &rest {
        tar.append_path_with_name(path, rel).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

/// What a fetch landed, less how the export was packed.
fn landed(summary: &ingest::FetchSummary) -> serde_json::Value {
    let mut v = serde_json::to_value(summary).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.remove("archives");
    obj.remove("unpacked");
    v
}

async fn fetch_from(work: &Path, input: &Path, sync: SyncFlags) -> ingest::FetchSummary {
    let db = RawDb::open(&work.join("gt.doltlite_db")).await.unwrap();
    let mut o = opts(work, &db, sync).await;
    o.input_path = input.to_path_buf();
    let summary = ingest::fetch(o).await.unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;
    summary
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zipped_export_lands_what_the_unpacked_one_does() {
    let (_tree_work, from_tree, _) = run_all().await;
    let work = tempfile::tempdir().unwrap();
    let parts = work.path().join("export");
    std::fs::create_dir(&parts).unwrap();
    pack_fixture(&parts);

    let first = fetch_from(work.path(), &parts, SyncFlags::all()).await;
    assert_eq!(first.archives, 2);
    assert!(first.unpacked > 0, "{first:?}");
    assert_eq!(landed(&first), landed(&from_tree));
}

/// Unchanged parts are not unpacked again, and no fingerprint of the
/// temporary directory is left in the host cache.
#[tokio::test(flavor = "multi_thread")]
async fn unchanged_parts_are_not_unpacked_again() {
    let work = tempfile::tempdir().unwrap();
    let parts = work.path().join("export");
    std::fs::create_dir(&parts).unwrap();
    pack_fixture(&parts);
    fetch_from(work.path(), &parts, SyncFlags::all()).await;

    let again = fetch_from(work.path(), &parts, SyncFlags::all()).await;
    assert_eq!(again.archives, 2);
    assert_eq!(again.unpacked, 0, "{again:?}");
    assert_eq!(landed(&again), landed(&ingest::FetchSummary::default()));

    let cache = FingerprintCache::open(&work.path().join("fingerprints.sqlite"))
        .await
        .unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT abs_path FROM fingerprints")
        .fetch_all(cache.pool())
        .await
        .unwrap();
    assert!(
        rows.iter().all(|p| p.contains("/export/takeout-")),
        "only the parts are fingerprinted: {rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_feed_turned_on_later_unpacks_the_parts_again() {
    let work = tempfile::tempdir().unwrap();
    let parts = work.path().join("export");
    std::fs::create_dir(&parts).unwrap();
    pack_fixture(&parts);
    let chat = SyncFlags {
        google_chat: true,
        ..SyncFlags::default()
    };
    let first = fetch_from(work.path(), &parts, chat.clone()).await;
    assert_eq!(first.chat_messages, 2);
    assert_eq!(first.voice_bills, 0);

    let with_voice = SyncFlags {
        google_voice: true,
        ..chat
    };
    let second = fetch_from(work.path(), &parts, with_voice).await;
    assert!(second.unpacked > 0, "{second:?}");
    assert!(second.voice_bills > 0, "{second:?}");
    assert_eq!(second.chat_messages, 0, "Chat's files are unchanged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewritten_part_is_unpacked_and_read_again() {
    let work = tempfile::tempdir().unwrap();
    let parts = work.path().join("export");
    std::fs::create_dir(&parts).unwrap();
    pack_fixture(&parts);
    fetch_from(work.path(), &parts, SyncFlags::all()).await;

    // The same export, with Chat and Maps moved into the .tgz.
    std::fs::remove_dir_all(&parts).unwrap();
    std::fs::create_dir(&parts).unwrap();
    let gz = flate2::write::GzEncoder::new(
        std::fs::File::create(parts.join("takeout-23640301T090000Z-1-001.tgz")).unwrap(),
        flate2::Compression::fast(),
    );
    let mut tar = tar::Builder::new(gz);
    for (rel, path) in fixture_entries() {
        tar.append_path_with_name(path, rel).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();

    let again = fetch_from(work.path(), &parts, SyncFlags::all()).await;
    assert!(again.unpacked > 0, "{again:?}");
    assert_eq!(again.removed, 0, "the same files hold the same records");
    assert_eq!(
        again.chat_messages, 0,
        "an unchanged file is not read again"
    );
}
