//! Render incrementality for github: a second run over an unchanged
//! store renders nothing, and a PR that left the store is a bucket to
//! render with no rows, so the driver drops its document.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_github::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_github::synthesize::GithubSynth;
use datalib_etl_github_render::render::{parse_api_dir, render_github};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
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
        datalib_etl_github::ingest::ENTITY_SELF,
        k,
        json!({"id": 42, "login": "octocat"}),
    );
    for (num, title) in prs {
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(REPO));
        k.insert("pr_number".into(), json!(num));
        write_event(
            api,
            datalib_etl_github::ingest::ENTITY_PR,
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
    let _ = datalib_etl::doltlite_raw::commit_run(db.pool(), "test: download").await;
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

/// The range the driver hands a warm run: a cursor to diff from and its
/// own stale set — empty here, since nothing declared has moved.
fn warm<'a>(cursor: Option<&'a str>, none: &'a HashSet<String>) -> RawRange<'a> {
    match cursor {
        Some(cursor) => RawRange {
            cursor: Some(cursor),
            pin: None,
            stale: Some(none),
        },
        None => RawRange::cold(),
    }
}

/// One render pass from `cursor`. Returns the documents emitted, the
/// buckets the pass rendered, and the commit it consumed — what the
/// render step would record as the next cursor.
fn render_once(
    raw: &Path,
    out: &Path,
    cursor: Option<&str>,
) -> (
    Vec<RenderedMarkdown>,
    Option<HashSet<String>>,
    Option<String>,
) {
    let none = HashSet::new();
    let parsed = parse_api_dir(raw, "github", warm(cursor, &none)).unwrap();
    let mut docs = Vec::new();
    render_github(&parsed, out, "github", &Progress::noop(), &mut |d| {
        docs.push(d);
        Ok(())
    })
    .unwrap();
    (docs, parsed.render.clone(), parsed.head.clone())
}

/// The headline: a second render over an unchanged store does no work.
///
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_render_over_an_unchanged_store_renders_nothing() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("raw");
    let out = d.path().join("out");

    build_events(&d.path().join("ev"), &[(1, "one"), (2, "two")]);
    download(&d.path().join("ev"), &d.path().join("pb"), &out_db).await;

    let (first, render, cursor) = render_once(&db_path_for(&out_db), &out, None);
    assert_eq!(first.len(), 2, "cold start renders both PRs");
    assert!(
        render.is_none(),
        "a cold start has no cursor to narrow from"
    );
    assert!(first
        .iter()
        .all(|d| d.bucket_key.as_deref().is_some_and(|k| k.starts_with(REPO))));

    let (second, render, _) = render_once(&db_path_for(&out_db), &out, cursor.as_deref());

    assert!(
        second.is_empty(),
        "nothing changed, so the diff should have narrowed the render to \
         nothing — {} document(s) came back",
        second.len(),
    );
    assert_eq!(
        render,
        Some(HashSet::new()),
        "and no bucket is declared, so the driver sweeps nothing"
    );
}

/// A PR gone from the store is still a bucket to render — with no row,
/// so it is declared empty and the driver drops its document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_that_left_the_store_is_a_bucket_with_no_rows() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("raw");
    let out = d.path().join("out");

    build_events(&d.path().join("ev"), &[(1, "one"), (2, "two")]);
    download(&d.path().join("ev"), &d.path().join("pb"), &out_db).await;
    let (_, _, cursor) = render_once(&db_path_for(&out_db), &out, None);

    // Delete PR 2 from the raw store and commit, the way an upstream loss
    // reaches render.
    let db = datalib_etl_github::ingest::RawDb::open(&db_path_for(&out_db))
        .await
        .unwrap();
    sqlx::query("DELETE FROM pull_requests WHERE id = ?")
        .bind(format!("{REPO}#2"))
        .execute(db.pool())
        .await
        .unwrap();
    datalib_etl::doltlite_raw::commit_run(db.pool(), "test: a PR went away")
        .await
        .unwrap();
    // Closed before the parse below reopens the file. Dropping only
    // schedules the disconnect, and a second pool on a still-open doltlite
    // store waits on it rather than failing — see AGENTS.md, "One open per
    // doltlite file". Without this the parse reads the pre-delete tree and
    // the assertion below comes back empty.
    db.close().await;

    let (docs, render, _) = render_once(&db_path_for(&out_db), &out, cursor.as_deref());
    assert_eq!(
        render,
        Some([format!("{REPO}#2")].into_iter().collect()),
        "the diff named the bucket",
    );
    assert!(docs.is_empty(), "and the store no longer has its row");
}
