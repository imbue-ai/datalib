//! Embedding one group's collection: `qmd embed -c <group>` in a loop
//! until qmd says there is nothing left, under a lock, with a gauge read
//! from the store while it runs.
//!
//! Two facts about qmd 2.8.3 shape this file. A second `qmd embed` on a
//! store another one is embedding prints `Another embed process is
//! already running. Skipping.` and exits 0 — so the exit code cannot be
//! trusted, and the output is read instead. And an embed session stops
//! itself after `--timeout` minutes (30 by default) and also exits 0 —
//! so the loop asks again until the answer is "already have embeddings".

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use datalib_obs::status_line;

use crate::store::{embed_gauge, open_ro, EmbedGauge};

/// The lock every datalib embed takes before spawning qmd. Beside
/// qmd's own `.qmd-embed.lock`, which refuses rather than waits.
pub const EMBED_LOCK_FILE: &str = ".datalib-embed.lock";

/// What qmd prints when its own lock is held. Pinned by
/// `tests/index_group.rs`, so a qmd bump that rewords it fails a test
/// rather than turning a skipped embed into a reported success.
pub const QMD_EMBED_BUSY: &str = "Another embed process is already running";
/// What qmd prints when the collection has nothing pending.
pub const QMD_EMBED_DONE: &str = "already have embeddings";
const QMD_EMBED_EMPTY: &str = "No non-empty documents to embed";
const QMD_EMBED_GAVE_UP: &str = "still failed after retries";

pub struct EmbedOptions {
    pub root: PathBuf,
    pub group: String,
    pub qmd_version: String,
    /// Wall-clock cap for this call. `None` runs until the collection
    /// is fully embedded.
    pub budget: Option<Duration>,
    /// Run `qmd pull` first when a required model is missing from
    /// `models_dir`.
    pub pull_if_missing: bool,
    pub models_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedOutcome {
    /// `qmd embed` invocations that did work.
    pub sessions: u32,
    /// Nothing left to embed in this collection.
    pub complete: bool,
    pub gauge: EmbedGauge,
}

pub trait EmbedProgress: Sync {
    fn gauge(&self, _g: &EmbedGauge) {}
    fn message(&self, _m: &str) {}
}

pub struct NoEmbedProgress;
impl EmbedProgress for NoEmbedProgress {}

/// Embed `opts.group`'s collection. Blocking: holds the embed lock and
/// waits on qmd for as long as the collection needs, or the budget
/// allows.
pub fn embed_group(opts: &EmbedOptions, progress: &dyn EmbedProgress) -> Result<EmbedOutcome> {
    let cache_home = datalib_runtime::qmd::qmd_cache_home(&opts.root);
    let qmd_dir = datalib_runtime::qmd::qmd_state_dir(&opts.root);
    let index_path = datalib_runtime::qmd::qmd_index_path(&opts.root);
    if !index_path.exists() {
        bail!(
            "no qmd index at {} — index the group before embedding it",
            index_path.display()
        );
    }
    let started = Instant::now();

    let _lock = EmbedLock::take(&qmd_dir.join(EMBED_LOCK_FILE), progress)?;

    if opts.pull_if_missing && !crate::models_present(&opts.models_dir) {
        progress.message("fetching models");
        crate::run_qmd(&cache_home, &opts.qmd_version, &["pull"])?;
    }

    // The gauge reader borrows `progress` for exactly as long as the
    // loop runs, which is what a scoped thread is for.
    let stop = AtomicBool::new(false);
    let (loop_result, gauge) = std::thread::scope(|s| {
        let reader = s.spawn(|| gauge_loop(&index_path, &opts.group, &stop, progress));
        let result = embed_loop(opts, &cache_home, started, progress);
        stop.store(true, Ordering::Relaxed);
        (result, reader.join().ok().flatten())
    });
    let (sessions, complete) = loop_result?;

    let gauge = match gauge {
        Some(g) => g,
        None => read_gauge_once(&index_path, &opts.group)?,
    };
    if !complete {
        progress.message(&format!("budget spent, {} documents left", gauge.pending));
    }
    Ok(EmbedOutcome {
        sessions,
        complete,
        gauge,
    })
}

/// `(sessions that did work, whether the collection is complete)`.
fn embed_loop(
    opts: &EmbedOptions,
    cache_home: &Path,
    started: Instant,
    progress: &dyn EmbedProgress,
) -> Result<(u32, bool)> {
    let mut sessions = 0;
    loop {
        let remaining = match opts.budget {
            Some(b) => {
                let left = b.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Ok((sessions, false));
                }
                Some(left)
            }
            None => None,
        };
        progress.message("embedding");
        match run_embed_session(cache_home, &opts.qmd_version, &opts.group, remaining)? {
            Session::Done => return Ok((sessions, true)),
            Session::Worked => sessions += 1,
        }
    }
}

enum Session {
    Done,
    Worked,
}

fn run_embed_session(
    cache_home: &Path,
    qmd_version: &str,
    group: &str,
    remaining: Option<Duration>,
) -> Result<Session> {
    let timeout_minutes = match remaining {
        // qmd reads `--timeout` as minutes and accepts fractions; 0 lifts
        // its own 30-minute default.
        Some(d) => format!("{:.2}", d.as_secs_f64() / 60.0),
        None => "0".to_string(),
    };
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version);
    cmd.args(["embed", "-c", group, "--timeout", &timeout_minutes]);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );
    let out = cmd
        .output()
        .with_context(|| "failed to spawn qmd; is Node.js installed?")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!(
            "qmd embed -c {group} failed: {}: {}",
            out.status,
            text.trim()
        );
    }
    if text.contains(QMD_EMBED_BUSY) {
        bail!(
            "qmd embed -c {group} did nothing: another qmd embed holds {} — \
             a qmd run outside datalib?",
            cache_home.join("qmd").join(".qmd-embed.lock").display()
        );
    }
    if text.contains(QMD_EMBED_DONE) || text.contains(QMD_EMBED_EMPTY) {
        return Ok(Session::Done);
    }
    // qmd retries a failing chunk on its own and then gives up, still
    // exiting 0. Asking again would ask forever, so the step fails here
    // with qmd's own account of what it could not embed.
    if text.contains(QMD_EMBED_GAVE_UP) {
        bail!(
            "qmd embed -c {group} gave up on some chunks:\n{}",
            text.trim()
        );
    }
    for line in text
        .lines()
        .filter(|l| l.contains("Done!") || l.contains("Session expired"))
    {
        status_line!("[qmd-indexer] {}", line.trim());
    }
    Ok(Session::Worked)
}

/// `flock(2)` on a file beside the index. Blocking, but says so first:
/// a step that sits here for an hour should read "waiting", not "hung".
struct EmbedLock {
    _file: File,
}

impl EmbedLock {
    fn take(path: &Path, progress: &dyn EmbedProgress) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        if !try_flock(&file, true)? {
            progress.message("waiting for another source's embedding to finish");
            status_line!("[qmd-indexer] waiting for {}", path.display());
            try_flock(&file, false)?;
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn try_flock(file: &File, non_blocking: bool) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let flags = libc::LOCK_EX | if non_blocking { libc::LOCK_NB } else { 0 };
    loop {
        // SAFETY: a valid open fd, and flock has no memory arguments.
        let rc = unsafe { libc::flock(file.as_raw_fd(), flags) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) if non_blocking => return Ok(false),
            Some(libc::EINTR) => continue,
            _ => return Err(err).context("flock"),
        }
    }
}

#[cfg(not(unix))]
fn try_flock(_file: &File, _non_blocking: bool) -> Result<bool> {
    Ok(true)
}

/// Reads the collection's gauge from a read-only connection about once
/// a second until told to stop, reporting each change. What it measures
/// is what landed in the store, which is the only progress qmd exposes
/// to anything that is not a terminal. Returns the last reading.
fn gauge_loop(
    index_path: &Path,
    group: &str,
    stop: &AtomicBool,
    progress: &dyn EmbedProgress,
) -> Option<EmbedGauge> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let pool = open_ro(index_path).await.ok()?;
        let mut last = None;
        loop {
            match embed_gauge(&pool, group).await {
                Ok(g) => {
                    if last != Some(g) {
                        progress.gauge(&g);
                        last = Some(g);
                    }
                }
                Err(e) => status_line!("[qmd-indexer] gauge read failed: {e:#}"),
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        pool.close().await;
        last
    })
}

fn read_gauge_once(index_path: &Path, group: &str) -> Result<EmbedGauge> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let pool = open_ro(index_path).await?;
        let g = embed_gauge(&pool, group).await;
        pool.close().await;
        g
    })
}
