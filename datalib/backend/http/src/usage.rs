//! Bytes on disk, over time.
//!
//! One background task walks the data root and keeps the newest measurement
//! of every tree, plus a short window of samples for the sparklines. The same
//! samples are appended to `system/usage.doltlite_db`, which nothing prunes.
//!
//! **It walks only while a run is in flight, and has no timer.** Nothing else
//! writes the data root, so between runs there is nothing to find, and an
//! idle server measuring forever would read tens of gigabytes an hour to
//! learn a number that cannot have moved. The cost is resolution, not
//! correctness: a change made from outside datalib carries the instant it was
//! next *measured*.
//!
//! **One walk, not one per tree.** Every declared tree is under the root, so
//! [`measure`] totals the root and records each subtree's subtotal on the way
//! back up.
//!
//! **The series is compacted**: a value equal to the last recorded one is
//! dropped, and two samples of one series are never closer than
//! [`MIN_SAMPLE_GAP`]. Readers must carry the last value forward rather than
//! assume a fixed interval.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_schema::disk_usage::{DiskUsageRow, ROOT_PATH};
use datalib_core::repo::DynAppRepo;
use serde::Serialize;
use tokio::sync::RwLock;

/// How often the root is walked *while a run is in flight*. Between
/// runs it isn't walked at all — see the module docs.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// The floor on the spacing between two recorded samples of one series.
/// Equal to [`SAMPLE_INTERVAL`] today, so in practice every changed
/// measurement is recorded; it is stated separately because it is a
/// rule about the *timeseries*, not about how often we happen to look.
pub const MIN_SAMPLE_GAP: Duration = Duration::from_secs(5);

/// How much history the API hands out, and how much is kept in memory.
/// The sparklines draw exactly this span.
pub const HISTORY_WINDOW: Duration = Duration::from_secs(5 * 60);

/// How many rows to read back at startup when seeding the window.
/// Generous: the read is one query, once, and the rows are three
/// columns each.
const SEED_ROWS: usize = 4_000;

/// The file name whose size splits a raw store's total in two.
const BLOBS_FILE: &str = "blobs.doltlite_db";

/// One measurement of one tree, as the API hands it out.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSample {
    /// ISO-8601 with explicit local offset, per AGENTS.md.
    pub at: String,
    pub bytes: u64,
}

/// A breakdown of one tree's total that is worth showing.
#[derive(Debug, Clone, Serialize)]
pub struct StoragePart {
    pub label: String,
    pub bytes: u64,
}

/// Bytes for one tree, with the recent history behind the number.
#[derive(Debug, Clone, Serialize)]
pub struct OutputStorage {
    /// The tree, data-root-relative: a step id, or `.` for the root.
    pub path: String,
    /// Resolved absolute path, for the desktop app's reveal IPC. The
    /// server is the side that knows where the root is.
    pub abs: String,
    /// The directory doesn't exist yet — nothing has written it.
    /// Distinct from a real zero, so the UI can show "—" rather than
    /// "0 B", which would read as "ran, and produced nothing".
    pub present: bool,
    pub bytes: u64,
    /// A breakdown worth showing, when the path has one. A raw store
    /// splits into entity rows and attachment blobs, and the split is
    /// the answer to "why is this so big" far more often than the
    /// total is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<StoragePart>,
    /// Samples inside the window, oldest first, plus (when there is
    /// one) the newest sample *before* the window — without that
    /// carry-in a series that hasn't moved in ten minutes would arrive
    /// empty and draw as nothing rather than as a flat line.
    pub history: Vec<UsageSample>,
}

/// The whole answer `GET /api/pipeline/storage` gives.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineStorage {
    /// The data root as a whole — every byte under it, including trees
    /// no step declares (`system/`, the stores, a stray download).
    pub root: OutputStorage,
    /// One entry per declared tree — every group's directory and every
    /// step's — in config order.
    pub outputs: Vec<OutputStorage>,
    /// The span `history` covers, in seconds. The UI scales its
    /// sparklines against this rather than against a constant of its
    /// own, so the two can't disagree about what "recent" means.
    pub window_secs: u64,
    /// When the last walk finished, or null when none has yet — the
    /// state a just-booted server is in for its first walk, and the
    /// only case where a zero here doesn't mean an empty disk.
    pub measured_at: Option<String>,
}

/// What one walk found.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    /// Every byte under the root.
    pub root_bytes: u64,
    /// Per requested tree, in the order requested.
    pub trees: BTreeMap<String, TreeUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct TreeUsage {
    pub present: bool,
    pub bytes: u64,
    /// Size of the tree's direct `blobs.doltlite_db`, when it has one.
    pub blob_bytes: u64,
}

pub fn measure(root: &Path, want: &BTreeSet<String>) -> Measurement {
    let mut trees: BTreeMap<String, TreeUsage> = want
        .iter()
        .map(|p| (p.clone(), TreeUsage::default()))
        .collect();
    let root_bytes = walk(root, "", want, &mut trees);
    Measurement { root_bytes, trees }
}

fn walk(
    dir: &Path,
    rel: &str,
    want: &BTreeSet<String>,
    out: &mut BTreeMap<String, TreeUsage>,
) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let wanted = !rel.is_empty() && want.contains(rel);
    let mut total = 0u64;
    let mut blob_bytes = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            let child = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            total += walk(&entry.path(), &child, want, out);
        } else {
            total += meta.len();
            if wanted && name == BLOBS_FILE {
                blob_bytes = meta.len();
            }
        }
    }
    if wanted {
        out.insert(
            rel.to_string(),
            TreeUsage {
                present: true,
                bytes: total,
                blob_bytes,
            },
        );
    }
    total
}

/// One series' newest value plus its recent samples.
#[derive(Debug, Default, Clone)]
struct Series {
    present: bool,
    bytes: u64,
    blob_bytes: u64,
    /// Oldest first. Holds the window, plus at most one sample from
    /// before it as the carry-in value.
    history: VecDeque<UsageSample>,
    /// Monotonic stamp of the last *recorded* sample. Monotonic rather
    /// than the wall clock in `history`, because it is what
    /// [`MIN_SAMPLE_GAP`] is measured against and a wall clock can step.
    last_recorded: Option<Instant>,
}

#[derive(Debug, Default)]
struct MonitorState {
    series: BTreeMap<String, Series>,
    measured_at: Option<String>,
    /// When a walk last *finished*. Finished, not started: the question
    /// a refresh asks is "has anything looked at the disk since I got
    /// here", and a walk that began before the caller arrived may have
    /// read the tree before the change the caller is asking about.
    walk_finished: Option<Instant>,
}

/// The live view of what the root weighs.
pub struct UsageMonitor {
    state: RwLock<MonitorState>,
    /// Held for the length of a walk, so two never overlap. Walks are
    /// I/O-bound over the same tree; running them concurrently would
    /// double the reads to produce one answer.
    walking: tokio::sync::Mutex<()>,
}

impl Default for UsageMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageMonitor {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(MonitorState::default()),
            walking: tokio::sync::Mutex::default(),
        }
    }

    pub async fn walked_since(&self, arrived: Instant) -> bool {
        self.state
            .read()
            .await
            .walk_finished
            .is_some_and(|t| t >= arrived)
    }

    pub async fn observe(
        &self,
        m: &Measurement,
        now_mono: Instant,
        now_iso: &str,
    ) -> Vec<DiskUsageRow> {
        let mut st = self.state.write().await;
        st.measured_at = Some(now_iso.to_string());
        st.walk_finished = Some(now_mono);
        let mut rows = Vec::new();
        let mut record = |st: &mut MonitorState, path: &str, u: &TreeUsage| {
            let s = st.series.entry(path.to_string()).or_default();
            s.present = u.present;
            s.bytes = u.bytes;
            s.blob_bytes = u.blob_bytes;
            // Both rules, and both have to hold: a repeat says nothing,
            // and two samples closer than the floor are noise even when
            // they differ.
            let unchanged = s.history.back().is_some_and(|last| last.bytes == u.bytes);
            let too_soon = s
                .last_recorded
                .is_some_and(|t| now_mono.duration_since(t) < MIN_SAMPLE_GAP);
            if unchanged || too_soon {
                return;
            }
            s.history.push_back(UsageSample {
                at: now_iso.to_string(),
                bytes: u.bytes,
            });
            s.last_recorded = Some(now_mono);
            prune(&mut s.history);
            rows.push(DiskUsageRow {
                path: path.to_string(),
                measured_at: now_iso.to_string(),
                bytes: u.bytes as i64,
            });
        };
        record(
            &mut st,
            ROOT_PATH,
            &TreeUsage {
                present: true,
                bytes: m.root_bytes,
                blob_bytes: 0,
            },
        );
        // `record` borrows `st` mutably per call, so the tree loop can't
        // hold an iterator into `m.trees` and `st` at once — clone the
        // small map of names first.
        let trees: Vec<(String, TreeUsage)> = m
            .trees
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (path, u) in &trees {
            record(&mut st, path, u);
        }
        rows
    }

    pub async fn seed(&self, rows: Vec<DiskUsageRow>) {
        let mut by_series: BTreeMap<String, Vec<(i64, UsageSample)>> = BTreeMap::new();
        for r in rows {
            let Ok(at) = datalib_time::parse_strict(&r.measured_at) else {
                continue;
            };
            by_series.entry(r.path).or_default().push((
                at.inner().timestamp_millis(),
                UsageSample {
                    at: r.measured_at,
                    bytes: r.bytes.max(0) as u64,
                },
            ));
        }
        let cutoff = chrono::Utc::now().timestamp_millis() - HISTORY_WINDOW.as_millis() as i64;
        let mut st = self.state.write().await;
        for (path, mut samples) in by_series {
            samples.sort_by_key(|(ms, _)| *ms);
            // Everything inside the window, preceded by the newest
            // sample from before it — the value the window opens at.
            let first_inside = samples.iter().position(|(ms, _)| *ms >= cutoff);
            let start = match first_inside {
                Some(0) => 0,
                Some(i) => i - 1,
                // Nothing inside the window: keep only the last known
                // value, which draws as a flat line until a new sample
                // lands.
                None => samples.len().saturating_sub(1),
            };
            let s = st.series.entry(path).or_default();
            s.history = samples.into_iter().skip(start).map(|(_, x)| x).collect();
            // The newest seeded sample is also the value to show until
            // the first fresh walk lands — otherwise a restart reads as
            // an empty disk for a few seconds.
            if let Some(last) = s.history.back() {
                s.bytes = last.bytes;
                s.present = last.bytes > 0;
            }
            // `last_recorded` stays None: these samples are from a
            // previous process, so the gap rule has nothing to measure
            // against and the first fresh measurement should record.
        }
    }

    /// Build the API's answer for a config's declared trees — see
    /// [`declared_trees`] — in the order given.
    pub async fn snapshot(&self, root: &Path, tree_ids: &[String]) -> PipelineStorage {
        let st = self.state.read().await;
        let outputs = tree_ids
            .iter()
            .map(|id| st.output(id, root.join(id)))
            .collect();
        PipelineStorage {
            root: st.output(ROOT_PATH, root.to_path_buf()),
            outputs,
            window_secs: HISTORY_WINDOW.as_secs(),
            measured_at: st.measured_at.clone(),
        }
    }
}

impl MonitorState {
    fn output(&self, path: &str, abs: PathBuf) -> OutputStorage {
        let s = self.series.get(path);
        let bytes = s.map(|s| s.bytes).unwrap_or(0);
        let blob_bytes = s.map(|s| s.blob_bytes).unwrap_or(0);
        let parts = if blob_bytes > 0 {
            vec![
                StoragePart {
                    label: "entities".into(),
                    bytes: bytes.saturating_sub(blob_bytes),
                },
                StoragePart {
                    label: "attachments".into(),
                    bytes: blob_bytes,
                },
            ]
        } else {
            Vec::new()
        };
        OutputStorage {
            path: path.to_string(),
            abs: abs.to_string_lossy().into_owned(),
            present: s.map(|s| s.present).unwrap_or(false),
            bytes,
            parts,
            history: s
                .map(|s| s.history.iter().cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// An ISO-8601 stamp as epoch milliseconds, or `None` if it doesn't
/// parse. Every stamp here is written by us, so a failure means a row
/// hand-edited or written by an older format — skip it rather than
/// guess.
fn parse_ms(at: &str) -> Option<i64> {
    datalib_time::parse_strict(at)
        .ok()
        .map(|t| t.inner().timestamp_millis())
}

fn prune(history: &mut VecDeque<UsageSample>) {
    let Some(newest) = history.back().and_then(|s| parse_ms(&s.at)) else {
        return;
    };
    let cutoff = newest - HISTORY_WINDOW.as_millis() as i64;
    // Drop the front while the one behind it would still serve as the
    // carry-in — i.e. while it too is at or before the cutoff.
    while history.len() > 1 {
        match history.get(1).and_then(|s| parse_ms(&s.at)) {
            Some(ms) if ms <= cutoff => {
                history.pop_front();
            }
            _ => break,
        }
    }
}

/// Every tree the config declares, in config order: each group's
/// directory, then each step's. A group's directory holds its steps'
/// trees, and the walk records a subtotal at both levels, so the Manage
/// screen's group row has a measured series of its own rather than a sum
/// of two step functions sampled at different instants.
pub fn declared_trees(config_path: &Path) -> Vec<String> {
    match datalib_dag::config::load(config_path) {
        Ok((cfg, _root)) => {
            let mut seen = BTreeSet::new();
            cfg.groups
                .iter()
                .map(|g| g.id.clone())
                .chain(cfg.steps.iter().map(|s| s.id.clone()))
                .filter(|id| seen.insert(id.clone()))
                .collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Walk the root once and fold what it finds into the monitor and the
/// store. Returns false when the walk did not happen.
pub async fn sample_once(
    monitor: &UsageMonitor,
    repo: &DynAppRepo,
    root: Arc<PathBuf>,
    coalesce_since: Option<Instant>,
) -> bool {
    // Serialize first, then re-check: while this was queued behind
    // another walk, that walk may have answered the question.
    let _walking = monitor.walking.lock().await;
    if let Some(arrived) = coalesce_since {
        if monitor.walked_since(arrived).await {
            return false;
        }
    }
    let config_path = datalib_dag::config::root_config_path(&root);
    // A recursive read_dir of a large root is not something to do on
    // the async executor.
    let measured = tokio::task::spawn_blocking(move || {
        let want: BTreeSet<String> = declared_trees(&config_path).into_iter().collect();
        measure(&root, &want)
    })
    .await;
    let Ok(m) = measured else {
        // The walk panicked. Skipping beats taking the server's whole
        // usage history down with it.
        return false;
    };
    let now_iso = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let rows = monitor.observe(&m, Instant::now(), &now_iso).await;
    if let Err(e) = repo.record_disk_usage(&rows).await {
        eprintln!("usage: could not record {} sample(s): {e}", rows.len());
    }
    true
}

/// A measurement asked for by a request, rather than by the clock.
pub async fn sample_on_demand(monitor: &UsageMonitor, repo: &DynAppRepo, root: Arc<PathBuf>) {
    let arrived = Instant::now();
    sample_once(monitor, repo, root, Some(arrived)).await;
}

pub fn pipeline_is_running(root: &Path) -> bool {
    datalib_dag::lock::FileLock::runner_is_held(root)
}

fn should_walk(running: bool, was_running: bool, since_last_walk: Duration) -> bool {
    let started = running && !was_running;
    let ended = !running && was_running;
    let due = running && since_last_walk >= SAMPLE_INTERVAL;
    started || ended || due
}

pub async fn run(
    monitor: Arc<UsageMonitor>,
    repo: DynAppRepo,
    root: Arc<PathBuf>,
    events: crate::watch::RootTx,
) {
    match repo.recent_disk_usage(SEED_ROWS).await {
        Ok(rows) => monitor.seed(rows).await,
        Err(e) => eprintln!("usage: could not read the recorded history: {e}"),
    }
    // Subscribe before the startup walk, so a run that begins during it
    // is not missed.
    let mut rx = events.subscribe();
    sample_once(&monitor, &repo, root.clone(), None).await;

    let mut was_running = false;
    let mut last_walk = Instant::now();
    loop {
        match rx.recv().await {
            Ok(_) => {}
            // Lagged means a burst outran this receiver, which is only
            // ever a reason to look sooner rather than later — the
            // events carry nothing this loop reads.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            // The watcher and its heartbeat are gone, which happens
            // only at shutdown. Nothing left to wake us.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
        let running = pipeline_is_running(&root);
        if should_walk(running, was_running, last_walk.elapsed()) {
            sample_once(&monitor, &repo, root.clone(), None).await;
            last_walk = Instant::now();
        }
        was_running = running;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(bytes: u64) -> TreeUsage {
        TreeUsage {
            present: true,
            bytes,
            blob_bytes: 0,
        }
    }

    /// One walk answers for the root *and* every tree asked about, and
    /// the subtotals nest — a tree's bytes are part of the root's.
    #[test]
    fn one_walk_totals_the_root_and_each_declared_tree() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("slack/ingest")).unwrap();
        std::fs::create_dir_all(root.join("slack/render_markdown")).unwrap();
        std::fs::create_dir_all(root.join("system")).unwrap();
        std::fs::write(root.join("slack/ingest").join(BLOBS_FILE), vec![7u8; 300]).unwrap();
        std::fs::write(
            root.join("slack/ingest/entities.doltlite_db"),
            vec![7u8; 100],
        )
        .unwrap();
        std::fs::write(root.join("slack/render_markdown/a.md"), vec![7u8; 50]).unwrap();
        std::fs::write(root.join("system/lock"), vec![7u8; 5]).unwrap();

        let want: BTreeSet<String> = [
            "slack",
            "slack/ingest",
            "slack/render_markdown",
            "pdfs/ingest",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let m = measure(root, &want);

        assert_eq!(m.root_bytes, 455);
        assert_eq!(m.trees["slack/ingest"].bytes, 400);
        assert_eq!(m.trees["slack/ingest"].blob_bytes, 300);
        assert_eq!(m.trees["slack/render_markdown"].bytes, 50);
        // The group's directory is wanted too, and it nests: its total
        // is both steps' trees, measured on the same walk, with no
        // blobs split of its own — that belongs to the raw store.
        assert_eq!(m.trees["slack"].bytes, 450);
        assert_eq!(m.trees["slack"].blob_bytes, 0);
        // Declared but never written: zero *and* absent, which is what
        // lets the UI draw "—" rather than "0 B".
        assert!(!m.trees["pdfs/ingest"].present);
        assert_eq!(m.trees["pdfs/ingest"].bytes, 0);
    }

    /// The wanted set is every group's directory and every step's tree,
    /// so the Manage screen's group row measures the folder it names.
    #[test]
    fn declared_trees_names_each_group_directory_and_each_step() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

[[steps]]
id = "custom/out"
command = "my-step"
"#,
        )
        .unwrap();
        assert_eq!(
            declared_trees(&config),
            [
                "slack",
                "slack/ingest",
                "slack/render_markdown",
                "custom/out"
            ]
            .map(String::from)
            .to_vec()
        );
    }

    /// A symlink is its own entry, never the tree it points at —
    /// otherwise a cycle never returns and a shared target is counted
    /// twice.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/big"), vec![7u8; 1000]).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("loop")).unwrap();
        let m = measure(root, &BTreeSet::new());
        assert!(m.root_bytes < 1100, "followed the link: {}", m.root_bytes);
    }

    /// The two compaction rules, which is the whole contract the stored
    /// timeseries has with its readers.
    #[tokio::test]
    async fn a_repeat_and_a_too_soon_sample_are_both_dropped() {
        let mon = UsageMonitor::new();
        let t0 = Instant::now();
        let mut m = Measurement {
            root_bytes: 100,
            ..Default::default()
        };
        m.trees.insert("a/raw".into(), tree(10));

        let first = mon.observe(&m, t0, "2026-09-02T10:00:00-07:00").await;
        assert_eq!(first.len(), 2, "the first sample of each series records");

        // Same numbers, a full gap later: nothing to say.
        let repeat = mon
            .observe(&m, t0 + MIN_SAMPLE_GAP, "2026-09-02T10:00:05-07:00")
            .await;
        assert!(repeat.is_empty(), "an unchanged measurement recorded");

        // Changed, but inside the gap: still nothing.
        m.root_bytes = 200;
        let hasty = mon
            .observe(&m, t0 + Duration::from_secs(1), "2026-09-02T10:00:01-07:00")
            .await;
        assert!(hasty.is_empty(), "a sample inside the gap recorded");

        // Changed, and far enough out.
        let good = mon
            .observe(
                &m,
                t0 + Duration::from_secs(30),
                "2026-09-02T10:00:30-07:00",
            )
            .await;
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].path, ROOT_PATH);
        assert_eq!(good[0].bytes, 200);
    }

    /// The newest value is always the last measurement, recorded or
    /// not — compaction bounds the *history*, never what the column
    /// says right now.
    #[tokio::test]
    async fn the_current_value_tracks_every_measurement() {
        let mon = UsageMonitor::new();
        let root = std::path::Path::new("/data");
        let t0 = Instant::now();
        let mut m = Measurement::default();
        m.trees.insert("a/raw".into(), tree(10));
        mon.observe(&m, t0, "2026-09-02T10:00:00-07:00").await;

        m.trees.insert("a/raw".into(), tree(99));
        // Inside the gap, so nothing is recorded…
        let rows = mon
            .observe(&m, t0 + Duration::from_secs(1), "2026-09-02T10:00:01-07:00")
            .await;
        assert!(rows.is_empty());

        // …but the number on screen is the fresh one.
        let snap = mon.snapshot(root, &["a/raw".to_string()]).await;
        assert_eq!(snap.outputs[0].bytes, 99);
        assert_eq!(snap.outputs[0].history.len(), 1, "history stays compacted");
        assert_eq!(snap.outputs[0].abs, "/data/a/raw");
    }

    /// A step the config declares but nothing has written yet is
    /// reported as absent rather than omitted — the row exists, it just
    /// has nothing on disk.
    #[tokio::test]
    async fn a_declared_but_unmeasured_step_is_absent_not_missing() {
        let mon = UsageMonitor::new();
        let snap = mon
            .snapshot(std::path::Path::new("/data"), &["never/raw".to_string()])
            .await;
        assert_eq!(snap.outputs.len(), 1);
        assert!(!snap.outputs[0].present);
        assert!(snap.outputs[0].history.is_empty());
        assert_eq!(snap.measured_at, None, "no walk has happened yet");
    }

    /// When the loop walks, stated as a table.
    #[test]
    fn a_walk_is_owed_at_the_edges_of_a_run_and_never_between_them() {
        let idle = Duration::ZERO;
        let long = SAMPLE_INTERVAL * 2;

        // Nothing running, however long it has been: no walk. This is
        // the whole point of the gate.
        assert!(!should_walk(false, false, idle));
        assert!(!should_walk(false, false, long));

        // A run starts: walk immediately, so the series has a point at
        // the beginning rather than one interval into it.
        assert!(should_walk(true, false, idle));

        // Continuing: on the interval, not on every predicate poll.
        assert!(!should_walk(true, true, idle));
        assert!(should_walk(true, true, SAMPLE_INTERVAL));

        // A run that began and ended inside one interval still gets
        // both its edges — which is the pair a short sync depends on.
        assert!(should_walk(false, true, idle));
    }

    /// The sampler runs while a run holds the root, and not otherwise.
    #[test]
    fn the_sampler_runs_only_while_a_runner_holds_the_root() {
        let td = tempfile::tempdir().unwrap();
        assert!(!pipeline_is_running(td.path()), "idle root");

        let held = datalib_dag::lock::FileLock::acquire_runner(td.path()).expect("claim");
        assert!(pipeline_is_running(td.path()), "a runner holds it");

        drop(held);
        assert!(!pipeline_is_running(td.path()), "the run let go");
    }

    /// Asking must not leave a trace. A probe on a timer that created
    /// the lock file would make every never-synced root sprout one, and
    /// one that rewrote it would erase what a live holder said about
    /// itself — which is the only thing a refused runner has to go on.
    #[test]
    fn asking_whether_a_run_is_in_flight_leaves_no_trace() {
        let td = tempfile::tempdir().unwrap();
        let lock = td.path().join(datalib_dag::lock::RUNNER_LOCK_REL_PATH);
        assert!(!pipeline_is_running(td.path()));
        assert!(!lock.exists(), "the probe created {}", lock.display());
    }

    /// Refreshes coalesce on *finished* walks, not on recent ones.
    #[tokio::test]
    async fn a_refresh_coalesces_only_on_a_walk_that_finished_after_it_arrived() {
        let mon = UsageMonitor::new();
        let t0 = Instant::now();
        // Nothing has ever walked: everyone walks.
        assert!(!mon.walked_since(t0).await);

        // A walk finishes at t0+1s.
        mon.observe(
            &Measurement::default(),
            t0 + Duration::from_secs(1),
            "2026-09-02T10:00:01-07:00",
        )
        .await;

        // Someone who arrived before it finished is covered by it…
        assert!(mon.walked_since(t0).await);
        // …and someone who arrived after it is not, however recent it is.
        assert!(!mon.walked_since(t0 + Duration::from_secs(2)).await);
    }

    /// Seeding keeps the newest sample from *before* the window, which
    /// is the value the window opens at. Without it a series that
    /// hasn't moved in an hour arrives empty and draws as nothing.
    #[tokio::test]
    async fn seeding_keeps_one_sample_from_before_the_window() {
        let mon = UsageMonitor::new();
        let old = chrono::Utc::now() - chrono::Duration::hours(2);
        let older = old - chrono::Duration::hours(1);
        mon.seed(vec![
            DiskUsageRow {
                path: "a/raw".into(),
                measured_at: old.to_rfc3339(),
                bytes: 42,
            },
            DiskUsageRow {
                path: "a/raw".into(),
                measured_at: older.to_rfc3339(),
                bytes: 41,
            },
        ])
        .await;
        let snap = mon
            .snapshot(std::path::Path::new("/data"), &["a/raw".to_string()])
            .await;
        assert_eq!(snap.outputs[0].history.len(), 1);
        assert_eq!(snap.outputs[0].history[0].bytes, 42, "kept the newest");
    }
}
