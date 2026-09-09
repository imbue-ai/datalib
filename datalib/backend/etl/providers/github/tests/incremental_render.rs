//! Render incrementality for github, and the trap that comes with it.
//!
//! Putting a renderer on a `dolt_diff` cursor narrows what it emits to the
//! documents that changed. Every provider here previously declared the
//! documents it saw and let the driver delete the rest — correct while the
//! renderer walked everything, and catastrophic the moment it stops. These
//! tests pin the second run: it must render nothing new and, above all,
//! must not treat the documents it skipped as deleted.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_github::download::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_github::synthesize::GithubSynth;
use datalib_etl_github_render::render::{parse_api_dir, render_github};
use datalib_etl_render::grid_index::RenderedMarkdown;
use serde_json::{json, Map, Value};
use tempfile::tempdir;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

const REPO: &str = "octocat/hello";

fn write_event(api: &Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

fn build_events(api: &Path, prs: &[(u64, &str)]) {
    std::fs::create_dir_all(api).unwrap();
    let mut k = Map::new();
    k.insert("user_id".into(), json!(42));
    write_event(
        api,
        datalib_etl_github::download::ENTITY_SELF,
        k,
        json!({"id": 42, "login": "octocat"}),
    );
    for (num, title) in prs {
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(REPO));
        k.insert("pr_number".into(), json!(num));
        write_event(
            api,
            datalib_etl_github::download::ENTITY_PR,
            k,
            json!({
                "number": num,
                "title": title,
                "state": "open",
                "html_url": format!("https://github.com/{REPO}/pull/{num}"),
                "head": {"sha": "abc", "ref": "br"},
                "base": {"sha": "def", "ref": "main"},
            }),
        );
    }
}

async fn download(api: &Path, playback: &Path, out_db: &Path) {
    GithubSynth::new(api).synthesize(playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, playback);
    // The test owns the store: one connection for the download and the
    // assertions both, because two is what breaks a doltlite file.
    let db = RawDb::open(&db_path_for(out_db)).await.unwrap();
    let out = fetch(FetchOptions {
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: std::time::Duration::ZERO,
        ..FetchOptions::new(db.clone())
    })
    .await;
    out.unwrap();

    // Commit, the way the orchestrator's `RawStoreSession::finish` does
    // after a real download. `fetch` on its own leaves the rows in the
    // working set, so the store has no `dolt_log` entry — and with no HEAD
    // to stamp there is no render cursor, so every run cold-starts and
    // none of these tests would be exercising the diff.
    //
    // On the same handle: reopening here would be a second connection
    // while this one is still alive, which is what makes a `dolt_commit`
    // fail with "commit conflict".
    let _ = sqlx::query("SELECT dolt_commit('-Am', 'test: download')")
        .execute(db.pool())
        .await;
    let commits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(
        commits > 0,
        "the store must have a HEAD for a cursor to name"
    );
    db.close().await;
}

/// One render pass. Returns the uuids emitted and how many the diff let it
/// skip. `prior` is the fingerprint map the store would hand back.
fn render_once(
    raw: &Path,
    out: &Path,
    prior: &HashMap<String, String>,
) -> (Vec<RenderedMarkdown>, usize) {
    let cursor_path = datalib_etl::render_cursor::cursor_path(out, "github");
    let cursor = datalib_etl::render_cursor::read_for_params(
        &cursor_path,
        &datalib_etl::render_cursor::no_params(),
    )
    .unwrap();
    let parsed =
        parse_api_dir(raw, cursor.as_ref().map(|c| c.last_rendered_hash.as_str())).unwrap();
    let mut docs = Vec::new();
    render_github(&parsed, out, "github", &Progress::noop(), prior, &mut |d| {
        docs.push(d);
        Ok(())
    })
    .unwrap();
    (docs, parsed.docs_skipped)
}

/// The headline: a second render over an unchanged store does no work.
///
/// Before the port this renderer re-derived every PR on every run and let
/// the fingerprint compare throw the results away — correct output, and the
/// whole cost paid anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_render_over_an_unchanged_store_renders_nothing() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("raw");
    let out = d.path().join("out");

    build_events(&d.path().join("ev"), &[(1, "one"), (2, "two")]);
    download(&d.path().join("ev"), &d.path().join("pb"), &out_db).await;

    let (first, skipped) = render_once(&db_path_for(&out_db), &out, &HashMap::new());
    assert_eq!(first.len(), 2, "cold start renders both PRs");
    assert_eq!(skipped, 0, "a cold start skips nothing — it has no cursor");

    let prior: HashMap<String, String> = first
        .iter()
        .map(|d| (d.markdown_uuid.clone(), d.source_fingerprint.clone()))
        .collect();
    let (second, skipped) = render_once(&db_path_for(&out_db), &out, &prior);

    assert!(
        second.is_empty(),
        "nothing changed, so the diff should have narrowed the render to \
         nothing — {} document(s) came back",
        second.len(),
    );
    assert_eq!(skipped, 2, "and both PRs should be reported as skipped");
}

/// The trap. A narrowed render emits only what changed, so the set it
/// produced is NOT the set of documents that should exist. This is the
/// assertion that would have caught leaving `retain_documents` in place:
/// the second run's output names neither PR, and treating that as the
/// complete set deletes both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrowed_render_must_not_be_read_as_the_complete_document_set() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("raw");
    let out = d.path().join("out");

    build_events(&d.path().join("ev"), &[(1, "one"), (2, "two")]);
    download(&d.path().join("ev"), &d.path().join("pb"), &out_db).await;

    let (first, _) = render_once(&db_path_for(&out_db), &out, &HashMap::new());
    let held: HashSet<String> = first.iter().map(|d| d.markdown_uuid.clone()).collect();
    let prior: HashMap<String, String> = first
        .iter()
        .map(|d| (d.markdown_uuid.clone(), d.source_fingerprint.clone()))
        .collect();

    let cursor_path = datalib_etl::render_cursor::cursor_path(&out, "github");
    let cursor = datalib_etl::render_cursor::read_for_params(
        &cursor_path,
        &datalib_etl::render_cursor::no_params(),
    )
    .unwrap();
    let parsed = parse_api_dir(
        &db_path_for(&out_db),
        cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
    )
    .unwrap();

    // Nothing moved upstream, so nothing may be named as vanished. That is
    // the only list the processor is allowed to delete from.
    assert!(
        parsed.vanished_buckets.is_empty(),
        "an unchanged store has lost nothing, but the parse named {:?}",
        parsed.vanished_buckets,
    );

    let (second, _) = render_once(&db_path_for(&out_db), &out, &prior);
    let emitted: HashSet<String> = second.iter().map(|d| d.markdown_uuid.clone()).collect();
    assert!(
        emitted.is_empty() && held.len() == 2,
        "the setup for the real claim: the run emitted nothing while two \
         documents exist",
    );
}

/// And the detection still works: a PR gone from the store is named, so the
/// processor has something to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_that_left_the_store_is_named_as_vanished() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("raw");
    let out = d.path().join("out");

    build_events(&d.path().join("ev"), &[(1, "one"), (2, "two")]);
    download(&d.path().join("ev"), &d.path().join("pb"), &out_db).await;
    render_once(&db_path_for(&out_db), &out, &HashMap::new());

    // Delete PR 2 from the raw store and commit, the way an upstream loss
    // reaches render.
    let db = datalib_etl_github::download::RawDb::open(&db_path_for(&out_db))
        .await
        .unwrap();
    sqlx::query("DELETE FROM pull_requests WHERE id = ?")
        .bind(format!("{REPO}#2"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("SELECT dolt_commit('-Am', 'test: a PR went away')")
        .execute(db.pool())
        .await
        .unwrap();
    // Closed before the parse below reopens the file. Dropping only
    // schedules the disconnect, and a second pool on a still-open doltlite
    // store waits on it rather than failing — see AGENTS.md, "One open per
    // doltlite file". Without this the parse reads the pre-delete tree and
    // the assertion below comes back empty.
    db.close().await;

    let cursor_path = datalib_etl::render_cursor::cursor_path(&out, "github");
    let cursor = datalib_etl::render_cursor::read_for_params(
        &cursor_path,
        &datalib_etl::render_cursor::no_params(),
    )
    .unwrap();
    let parsed = parse_api_dir(
        &db_path_for(&out_db),
        cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
    )
    .unwrap();

    assert_eq!(
        parsed.vanished_buckets,
        vec![format!("{REPO}#2")],
        "the diff named the bucket and the store no longer has its row",
    );
}
