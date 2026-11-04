//! A page trashed in Notion after it was mirrored says so, and says so
//! no longer once it is restored: the trash pass, from the tape to the
//! document. The search by edit time shows none of this, so each of these
//! read as an ordinary page before the pass was built.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use datalib_etl::progress::Progress;
use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::{FetchOptions, RawDb};
use datalib_etl_notion_render::render::parse_api_dir;
use datalib_etl_notion_render::render::render::render_notion;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_web::playback::{Asked, Playback};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::*;

const NOTE: &str = "*This page is in Notion's trash.*";
const LATER: &str = "2026-09-02T00:00:00.000Z";

async fn flagged(store: &Path, id: &str) -> bool {
    let db = RawDb::open(store).await.unwrap();
    let in_trash: i64 = sqlx::query_scalar("SELECT in_trash FROM pages WHERE id = ?")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await;
    in_trash == 1
}

async fn payload(store: &Path, id: &str) -> Value {
    let db = RawDb::open(store).await.unwrap();
    let pages = db.load_pages().await.unwrap();
    db.close().await;
    pages.into_iter().find(|p| p["id"] == id).unwrap()
}

async fn body(store: &Path, id: &str) -> String {
    let db = RawDb::open(store).await.unwrap();
    let bodies = db.load_page_markdown().await.unwrap();
    db.close().await;
    bodies
        .into_iter()
        .find(|(page, _)| page == id)
        .map(|(_, md)| md)
        .unwrap_or_default()
}

/// The document a cold render of `store` writes for `id`.
fn document(store: &Path, id: &str) -> String {
    let parsed = parse_api_dir(store, RawRange::cold()).unwrap();
    let root = tempdir().unwrap();
    let mut files = Vec::new();
    let mut on_doc = |md: datalib_etl_render::grid_index::RenderedMarkdown| {
        files.push((md.bucket_key.clone(), md.md_path));
        Ok(())
    };
    render_notion(
        &parsed,
        root.path(),
        "notion",
        &Progress::noop(),
        &mut on_doc,
        &HashMap::new(),
        1,
    )
    .unwrap();
    let (_, file) = files
        .iter()
        .find(|(bucket, _)| bucket.as_deref() == Some(id))
        .unwrap();
    std::fs::read_to_string(file).unwrap()
}

/// The commit a render resuming from now would read from.
fn head(store: &Path) -> String {
    parse_api_dir(store, RawRange::cold())
        .unwrap()
        .head
        .unwrap()
}

/// The buckets a warm render from `from` is handed.
fn rendered_since(store: &Path, from: &str) -> HashSet<String> {
    let stale: HashSet<String> = HashSet::new();
    parse_api_dir(
        store,
        RawRange {
            cursor: Some(from),
            pin: None,
            stale: Some(&stale),
        },
    )
    .unwrap()
    .render
    .unwrap()
}

/// A workspace of two pages at [`EDITED`], mirrored.
async fn mirrored(tape: &Path, store: &Path) {
    serve_page(tape, BRIDGE, EDITED, "Captain's log.\n");
    serve_page(tape, SICKBAY, EDITED, "Sickbay log.\n");
    serve_search(
        tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, EDITED)]),
        None,
    );
    run(tape, store, &[]).await.unwrap();
}

/// The bridge goes into the trash at `stamp`. Search no longer shows it,
/// as it shows no page in the trash.
fn goes_in(tape: &Path, stamp: &str) {
    serve_search(tape, None, json!([page(SICKBAY, EDITED)]), None);
    serve_trash(tape, None, json!([trashed(BRIDGE, stamp)]), None);
}

/// The whole story, at a stamp that never moves: the search has nothing
/// to say at any step of it. Going in costs the listing and nothing else;
/// the page's body stays, and only its document renders again. Coming out
/// costs one request, and the page can go back in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_trashed_upstream_shows_the_note_and_a_restored_one_loses_it() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    assert!(!flagged(&store, BRIDGE).await);
    assert!(!document(&store, BRIDGE).contains(NOTE));
    let before = head(&store);

    goes_in(&tape, EDITED);
    let went_in = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(went_in.trashed_pages, 1);
    assert_eq!(
        went_in.official_requests, 2,
        "the search and the trash: its stamp did not move, so nothing is owed"
    );
    assert!(flagged(&store, BRIDGE).await);
    assert!(!flagged(&store, SICKBAY).await);
    let doc = document(&store, BRIDGE);
    assert!(doc.contains(NOTE), "{doc}");
    assert!(doc.contains("Captain's log."), "the body stays: {doc}");
    assert!(!document(&store, SICKBAY).contains(NOTE));
    assert!(
        rendered_since(&store, &before).contains(BRIDGE),
        "a page going into the trash is named for render"
    );
    assert!(!rendered_since(&store, &before).contains(SICKBAY));
    let in_trash = head(&store);

    serve_trash(&tape, None, json!([]), None);
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, EDITED)]),
        None,
    );
    let came_out = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(came_out.restored_pages, 1);
    assert_eq!(
        came_out.official_requests, 3,
        "the search, the trash, and the page itself"
    );
    assert!(!flagged(&store, BRIDGE).await);
    let doc = document(&store, BRIDGE);
    assert!(!doc.contains(NOTE), "{doc}");
    assert!(rendered_since(&store, &in_trash).contains(BRIDGE));

    let quiet = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(quiet.official_requests, 2, "nothing is asked about again");

    goes_in(&tape, EDITED);
    run(&tape, &store, &[]).await.unwrap();
    assert!(document(&store, BRIDGE).contains(NOTE), "and back in");
    assert!(problems(&store).await.is_empty());
}

/// Whether trashing a page moves its stamp has not been measured, since
/// that takes a write to a real workspace. If it does, a body and comments
/// are owed at the new stamp as for any edit, and Notion answers both for
/// a page in the trash. If restoring moves it, the search lists the page,
/// which clears the flag with no request of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stamp_that_moves_going_in_and_coming_out_is_an_edit() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;

    goes_in(&tape, LATER);
    serve_body(&tape, BRIDGE, "Captain's log, in the trash.\n", false);
    let went_in = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(went_in.trashed_pages, 1);
    assert_eq!(went_in.bodies, 1);
    assert!(flagged(&store, BRIDGE).await);
    assert_eq!(payload(&store, BRIDGE).await["last_edited_time"], LATER);
    assert_eq!(
        body(&store, BRIDGE).await,
        "Captain's log, in the trash.\n",
        "the body is owed at the stamp the listing carried"
    );

    let restored = "2026-09-03T00:00:00.000Z";
    serve_search(&tape, None, json!([page(BRIDGE, restored)]), None);
    serve_trash(&tape, None, json!([]), None);
    let asked = Asked::default();
    let came_out = run_with(Playback::at(&tape).asked(asked.clone()), &store, |o| o)
        .await
        .unwrap();
    assert!(!flagged(&store, BRIDGE).await);
    assert!(!document(&store, BRIDGE).contains(NOTE));
    assert_eq!(came_out.restored_pages, 0);
    assert_eq!(came_out.upd_pages, 1, "the search brought it back");
    assert_eq!(
        asked.count(&format!("/pages/{BRIDGE}/markdown")),
        1,
        "its body, at the new stamp"
    );
    assert_eq!(
        came_out.official_requests, 4,
        "the search, the trash, its body and its comments, and no request for the page"
    );
}

/// A page Notion no longer shows after it left the trash keeps what it
/// held and the note it had: nothing better is known of it. It is asked
/// about once, not on every run for ever, and that is no problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_deleted_for_good_is_asked_about_once() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    goes_in(&tape, EDITED);
    run(&tape, &store, &[]).await.unwrap();

    serve_trash(&tape, None, json!([]), None);
    serve_status(&tape, &format!("{BASE}/pages/{BRIDGE}"), 404);
    let purged = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(purged.official_requests, 3);
    assert_eq!(purged.restored_pages, 0);
    assert!(!flagged(&store, BRIDGE).await, "the trash no longer has it");
    assert_eq!(
        payload(&store, BRIDGE).await["in_trash"],
        true,
        "the object as Notion last returned it"
    );
    let doc = document(&store, BRIDGE);
    assert!(doc.contains(NOTE), "{doc}");
    assert!(doc.contains("Captain's log."), "{doc}");
    assert!(problems(&store).await.is_empty());

    let later = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(later.official_requests, 2, "the search and the trash");
}

/// Search is not a promise that a listing is up to date: a page it leaves
/// out may still be in the trash, and asking for it by id is what stops a
/// lagging listing clearing a flag that is true. Nothing is written, and a
/// page the listing does name at the stamp held is not written again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_that_lags_clears_nothing_and_a_page_held_is_not_rewritten() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    goes_in(&tape, EDITED);
    run(&tape, &store, &[]).await.unwrap();
    let stored = payload(&store, BRIDGE).await;

    let mut signed = trashed(BRIDGE, EDITED);
    signed["cover"] = json!({"file": {"url": "https://files.example.test/c?X-Amz-Signature=a"}});
    serve_trash(&tape, None, json!([signed.clone()]), None);
    let again = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(again.trashed_pages, 0);
    assert_eq!(payload(&store, BRIDGE).await, stored);

    serve_trash(&tape, None, json!([]), None);
    serve(&tape, &format!("{BASE}/pages/{BRIDGE}"), signed);
    let lagging = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(lagging.restored_pages, 0);
    assert!(flagged(&store, BRIDGE).await, "still in the trash");
    assert_eq!(payload(&store, BRIDGE).await, stored);
    assert!(problems(&store).await.is_empty());
}

/// The trash holds what the mirror never held, most of it years old, and
/// being in the trash is no reason to mirror it: 473 rows in a measured
/// trash named none of a store's 52,726 pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_the_store_never_held_is_not_mirrored_from_the_trash() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    serve_trash(&tape, None, json!([trashed(HOLODECK, EDITED)]), None);
    let s = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(s.trashed_pages, 0);
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), SICKBAY.to_string()]
    );
}

/// A listing cut short says nothing of the pages it did not reach, so no
/// flag clears on its word and no page is asked about. What it did read
/// stands. A row says so until a listing comes whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_cut_short_clears_nothing_and_says_so() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    goes_in(&tape, EDITED);
    run(&tape, &store, &[]).await.unwrap();

    serve_trash(&tape, None, json!([trashed(SICKBAY, EDITED)]), Some("t2"));
    let cut = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        cut.official_requests, 2,
        "the search and the first page of the trash; the tape has no second"
    );
    assert!(flagged(&store, SICKBAY).await, "read before the cut");
    assert!(flagged(&store, BRIDGE).await, "not reached, so not cleared");
    assert_eq!(cut.restored_pages, 0);
    assert_eq!(problems(&store).await, vec![row("listing:trash", "error")]);

    serve_trash(&tape, None, json!([trashed(SICKBAY, EDITED)]), None);
    let whole = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(whole.restored_pages, 1);
    assert!(!flagged(&store, BRIDGE).await);
    assert!(flagged(&store, SICKBAY).await);
    assert!(problems(&store).await.is_empty());
}

/// A trash listing refused for a reason of its own costs the listing: a
/// credential at an older `Notion-Version` answers the filter with a 400,
/// and the rest of the run goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trash_listing_refused_costs_only_the_listing() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "Captain's log.\n");
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), None);
    serve_trash_status(&tape, 400);
    let s = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(s.bodies, 1, "the loops ran");
    assert_eq!(problems(&store).await, vec![row("listing:trash", "error")]);
}

/// A credential refused at the trash listing ends the run as it would at
/// any request after the first listing: what was fetched is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_refused_at_the_trash_listing_keeps_what_was_listed() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "Captain's log.\n");
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), None);
    serve_trash_status(&tape, 401);
    let s = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(s.listed, 1);
    assert_eq!(s.bodies, 0, "no request after it would have fared better");
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);
    assert_eq!(
        problems(&store).await,
        vec![row("phase:credential", "error")]
    );
}

/// A page that will not answer for itself stays flagged, with a row, and
/// the next run asks again: a listing leaving it out proves nothing alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_that_did_not_answer_stays_flagged_until_it_does() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    mirrored(&tape, &store).await;
    goes_in(&tape, EDITED);
    run(&tape, &store, &[]).await.unwrap();

    serve_trash(&tape, None, json!([]), None);
    serve_status(&tape, &format!("{BASE}/pages/{BRIDGE}"), 500);
    run(&tape, &store, &[]).await.unwrap();
    assert!(flagged(&store, BRIDGE).await);
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("pages:{BRIDGE}"), "warning")],
        "a warning: what the store holds of the page is whole"
    );

    serve_object(&tape, BRIDGE, EDITED);
    run(&tape, &store, &[]).await.unwrap();
    assert!(!flagged(&store, BRIDGE).await);
    assert!(problems(&store).await.is_empty());
}

/// Under roots a page is owed against the stamp `listed_pages` holds. A
/// trash listing that moved the page's stamp moves that one too, or the
/// run after asks for the page's object for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_roots_a_trashed_page_is_not_owed_against_its_old_stamp() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_workspace(&tape, &[BRIDGE]);
    serve_page(&tape, BRIDGE, EDITED, "Captain's log.\n");
    run(&tape, &store, &[BRIDGE]).await.unwrap();

    serve_workspace(&tape, &[]);
    serve_trash(&tape, None, json!([trashed(BRIDGE, LATER)]), None);
    serve_body(&tape, BRIDGE, "Captain's log, in the trash.\n", false);
    let went_in = run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(went_in.trashed_pages, 1);
    assert!(flagged(&store, BRIDGE).await);
    assert!(document(&store, BRIDGE).contains(NOTE));

    let after = run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(after.official_requests, 2, "the search and the trash");

    serve_trash(&tape, None, json!([]), None);
    serve_object(&tape, BRIDGE, LATER);
    let came_out = run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(came_out.restored_pages, 1);
    assert!(!flagged(&store, BRIDGE).await);
    assert!(!document(&store, BRIDGE).contains(NOTE));
}

/// One page and nothing under it reaches none of a full run's listings:
/// a trash listing is no business of a run that names one page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_page_does_not_read_the_trash() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "Captain's log.\n");
    let asked = Asked::default();
    run_with(Playback::at(&tape).asked(asked.clone()), &store, |o| {
        FetchOptions {
            page: Some(BRIDGE.to_string()),
            ..o
        }
    })
    .await
    .unwrap();
    assert_eq!(asked.count("/search"), 0);
}
