//! Render may start on each seal of a running download, so a seal has to
//! be a store it can read: every pull request in it whole, and a render
//! that reads them one seal after another writes each document once.
//! The processor runs as the step runs it, sealing at every point it
//! calls consistent.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_etl::checkpointer::Cadence;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::content_commit_at_path;
use datalib_etl::download_metrics::DownloadMetrics;
use datalib_etl::processor::{PlanContext, RunCtx};
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl_github::ingest::{db_path_for, RawDb};
use datalib_etl_github::processor::plan_ingest;
use datalib_etl_github_config::{GithubApiSync, GithubConfig};
use datalib_etl_github_render::render::{parse_api_dir, render_github};
use datalib_etl_render::inputs::RawRange;
use datalib_etl_web::playback;
use datalib_obs::diagnostics::Diagnostics;

use crate::interrupt::{tape, Edition};
use crate::support::REPO;

const NOW: &str = "2369-04-15T00:00:00+00:00";
const LATER: &str = "2369-04-16T00:00:00+00:00";

struct Seals(Arc<Mutex<Vec<String>>>);

impl ProgressSink for Seals {
    fn checkpoint(&self, version: &str) {
        self.0.lock().unwrap().push(version.to_string());
    }
}

/// One run of the planned processor, sealing at every chance. Returns
/// the commit each seal announced, and the one the run ended on. Fails
/// unless the processor says consumers may read it as it writes.
async fn ingest(raw: &Path, playback_tree: &Path, now: &str) -> Vec<String> {
    let mut config = GithubConfig {
        api: Some(GithubApiSync::default()),
        ..Default::default()
    };
    config.common.resolve_paths(raw.to_path_buf());
    let planned = plan_ingest(
        PlanContext {
            name: "github".to_string(),
        },
        config,
    )
    .unwrap();
    assert_eq!(planned.len(), 1);
    let proc = &planned[0];
    assert!(
        proc.streams_output(),
        "the step tells the runner no consumer may start until the download ends"
    );
    let seals = Arc::new(Mutex::new(Vec::new()));
    let progress = Progress::new(Arc::new(Seals(seals.clone())));
    let control = DownloadControl {
        checkpoint_cadence: Some(Cadence {
            at_most_every: Duration::ZERO,
        }),
        ..Default::default()
    };
    let ctx = RunCtx::new(
        "github",
        raw,
        now,
        &progress,
        &control,
        DownloadMetrics::new(),
        Diagnostics::new(),
    );
    playback::scope(playback_tree, proc.run(&ctx))
        .await
        .unwrap();
    let mut seals = std::mem::take(&mut *seals.lock().unwrap());
    let ended_on = content_commit_at_path(&db_path_for(raw))
        .await
        .unwrap()
        .expect("the run committed");
    if seals.last() != Some(&ended_on) {
        seals.push(ended_on);
    }
    seals
}

/// What one commit holds: each stored PR's row, by key, with how many
/// children it has of each kind, and the PRs whose sidecar says they are
/// held.
async fn read_at(raw: &Path, commit: &str) -> (BTreeMap<String, [usize; 3]>, HashSet<String>) {
    let db = RawDb::open_reader(&db_path_for(raw), Some(commit))
        .await
        .unwrap()
        .expect("every seal is readable");
    let mut prs: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for pr in db.load_pull_requests().await.unwrap() {
        prs.insert(pr.id, [0; 3]);
    }
    for (i, table) in ["issue_comments", "pr_reviews", "pr_review_comments"]
        .iter()
        .enumerate()
    {
        for child in db.load_children(table).await.unwrap() {
            let key = format!("{}#{}", child.repo_full_name, child.pr_number);
            prs.get_mut(&key).unwrap_or_else(|| {
                panic!("a {table} row of {key}, with no such PR, at {commit}")
            })[i] += 1;
        }
    }
    let held: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM pull_requests_bookkeeping WHERE held_version IS NOT NULL")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;
    (prs, held.into_iter().map(|(id,)| id).collect())
}

/// A pull request and its comments and reviews land in one transaction,
/// and a seal comes only between transactions. So whichever seal a render
/// starts on, a PR is there with all it has, or not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_seal_holds_each_pull_request_whole() {
    let d = tempfile::tempdir().unwrap();
    let raw = d.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    let pb = tape(d.path(), Edition::Before);
    let seals = ingest(&raw, &pb, NOW).await;

    let (pr1, pr2) = (format!("{REPO}#1"), format!("{REPO}#2"));
    let whole = BTreeMap::from([(pr1.clone(), [1, 0, 0]), (pr2.clone(), [0, 1, 1])]);
    let mut sizes = Vec::new();
    for commit in &seals {
        let (prs, held) = read_at(&raw, commit).await;
        for (key, children) in &prs {
            assert_eq!(Some(children), whole.get(key), "{key} at {commit}");
        }
        let stored: HashSet<String> = prs.keys().cloned().collect();
        assert_eq!(stored, held, "a PR's sidecar lands with it, at {commit}");
        sizes.push(prs.len());
    }
    assert_eq!(sizes.last(), Some(&2), "the run ended with both: {sizes:?}");
    for size in [0, 1] {
        assert!(
            sizes.contains(&size),
            "a seal between the PRs is what this test is about: {sizes:?}"
        );
    }
}

/// One render pass after another, each from where the last stopped, as
/// the runner starts them on each seal. Returns each document it wrote,
/// in order, as `(bucket key, the markdown)`.
fn render_each_seal(
    raw: &Path,
    out: &Path,
    cursor: &mut Option<String>,
    seals: &[String],
) -> Vec<(String, String)> {
    let mut written = Vec::new();
    let none = HashSet::new();
    for commit in seals {
        let range = RawRange {
            cursor: cursor.as_deref(),
            pin: Some(commit),
            stale: Some(&none),
        };
        let parsed = parse_api_dir(raw, "github", range).unwrap();
        render_github(&parsed, out, "github", &Progress::noop(), &mut |doc| {
            written.push((
                doc.bucket_key.clone().unwrap_or_default(),
                std::fs::read_to_string(&doc.md_path).unwrap(),
            ));
            Ok(())
        })
        .unwrap();
        *cursor = Some(commit.clone());
    }
    written
}

/// A download that takes hours leaves nearly everything queued, and
/// render runs again on every seal. Only what a seal changed renders, so
/// each document is written once however many passes there are. A later
/// seal that changes a PR renders that PR again, with what it now has,
/// and leaves the others alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_render_on_every_seal_writes_each_document_once() {
    let d = tempfile::tempdir().unwrap();
    let raw = d.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    let out = d.path().join("out");

    let seals = ingest(&raw, &tape(d.path(), Edition::Before), NOW).await;
    let mut cursor = None;
    let first = render_each_seal(&raw, &out, &mut cursor, &seals);
    let keys: Vec<&str> = first.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, [format!("{REPO}#1"), format!("{REPO}#2")]);

    let seals = ingest(&raw, &tape(d.path(), Edition::After), LATER).await;
    let later = render_each_seal(&raw, &out, &mut cursor, &seals);
    let keys: Vec<&str> = later.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [format!("{REPO}#3"), format!("{REPO}#1")],
        "PR 2 did not move"
    );
    let (_, pr1) = &later[1];
    for said in ["Safety protocols, revised", "Looks good.", "Make it so."] {
        assert!(pr1.contains(said), "{said} is missing from {pr1}");
    }
}
