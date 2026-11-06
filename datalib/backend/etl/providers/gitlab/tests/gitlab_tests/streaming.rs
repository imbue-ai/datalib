//! Render may start on each seal of a running download, so a seal has to
//! be a store it can read: every merge request in it whole, and a render
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
use datalib_etl_gitlab::ingest::{db_path_for, RawDb};
use datalib_etl_gitlab::processor::plan_ingest;
use datalib_etl_gitlab_config::{GitlabApiSync, GitlabConfig};
use datalib_etl_gitlab_render::render::{parse_api_dir, render_gitlab};
use datalib_etl_render::inputs::RawRange;
use datalib_etl_web::playback;
use datalib_obs::diagnostics::Diagnostics;

use crate::interrupt::{tape, Edition};
use crate::support::PROJECT;

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
    let mut config = GitlabConfig {
        api: Some(GitlabApiSync::default()),
        ..Default::default()
    };
    config.common.resolve_paths(raw.to_path_buf());
    let planned = plan_ingest(
        PlanContext {
            name: "gitlab".to_string(),
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
        "gitlab",
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

/// What one commit holds: each stored MR's row, by key, with how many
/// discussions it has, and the MRs whose sidecar says they are held.
async fn read_at(raw: &Path, commit: &str) -> (BTreeMap<String, usize>, HashSet<String>) {
    let db = RawDb::open_reader(&db_path_for(raw), Some(commit))
        .await
        .unwrap()
        .expect("every seal is readable");
    let mut mrs: BTreeMap<String, usize> = BTreeMap::new();
    for mr in db.load_merge_requests().await.unwrap() {
        mrs.insert(mr.id, 0);
    }
    for d in db.load_discussions().await.unwrap() {
        let key = format!("{}!{}", d.project_full_path, d.mr_iid);
        *mrs.get_mut(&key)
            .unwrap_or_else(|| panic!("a discussion of {key}, with no such MR, at {commit}")) += 1;
    }
    let held: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM merge_requests_bookkeeping WHERE held_version IS NOT NULL")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;
    (mrs, held.into_iter().map(|(id,)| id).collect())
}

/// A merge request and its discussions land in one transaction, and a
/// seal comes only between transactions. So whichever seal a render starts
/// on, an MR is there with all it has, or not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_seal_holds_each_merge_request_whole() {
    let d = tempfile::tempdir().unwrap();
    let raw = d.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    let pb = tape(d.path(), Edition::Before);
    let seals = ingest(&raw, &pb, NOW).await;

    let (mr1, mr2) = (format!("{PROJECT}!1"), format!("{PROJECT}!2"));
    let whole = BTreeMap::from([(mr1.clone(), 1), (mr2.clone(), 0)]);
    let mut sizes = Vec::new();
    for commit in &seals {
        let (mrs, held) = read_at(&raw, commit).await;
        for (key, discussions) in &mrs {
            assert_eq!(Some(discussions), whole.get(key), "{key} at {commit}");
        }
        let stored: HashSet<String> = mrs.keys().cloned().collect();
        assert_eq!(stored, held, "an MR's sidecar lands with it, at {commit}");
        sizes.push(mrs.len());
    }
    assert_eq!(sizes.last(), Some(&2), "the run ended with both: {sizes:?}");
    for size in [0, 1] {
        assert!(
            sizes.contains(&size),
            "a seal between the MRs is what this test is about: {sizes:?}"
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
        let parsed = parse_api_dir(raw, "gitlab", range).unwrap();
        render_gitlab(&parsed, out, "gitlab", &Progress::noop(), &mut |doc| {
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

/// Render runs again on every seal, and only what a seal changed renders:
/// each document is written once however many passes there are. A later
/// seal that changes an MR renders that MR again, with what it now has,
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
    assert_eq!(keys, [format!("{PROJECT}!1"), format!("{PROJECT}!2")]);

    let seals = ingest(&raw, &tape(d.path(), Edition::After), LATER).await;
    let later = render_each_seal(&raw, &out, &mut cursor, &seals);
    let keys: Vec<&str> = later.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [format!("{PROJECT}!3"), format!("{PROJECT}!1")],
        "MR 2 did not move"
    );
    let (_, mr1) = &later[1];
    for said in ["Safety protocols, revised", "Looks good.", "Make it so."] {
        assert!(mr1.contains(said), "{said} is missing from {mr1}");
    }
}
