//! `datalib-dag` — run a DAG config file (see `datalib_dag::config`
//! for the schema).
//!
//! ```sh
//! datalib-dag config.toml [--binary-dir DIR] [--sync STEP_ID[,…]]…
//!     [--now RFC3339] [--parallelism N]
//!     [--reset-and-redownload] [--refetch-blobs]
//! datalib-dag --check config.toml
//! ```
//!
//! * `--check` validates the config and runs nothing, printing *every*
//!   problem rather than the first — which is the difference between
//!   one round-trip and one per typo for whoever (or whatever) is
//!   editing the file. Exit 0 clean, 1 if the file is not a config at
//!   all, 2 if some entries were dropped.
//!
//! * `--binary-dir` is prepended to every step's `PATH`, so commands
//!   can name step binaries bare (`datalib-step …`). Defaults to the
//!   config `binary_dir`, then this executable's own directory.
//! * `--sync` runs a subgraph and only that subgraph: the named
//!   source steps (the steps with no inputs) plus everything
//!   downstream of them. Every other step is reported `not_selected`
//!   and never considered — including work pending elsewhere, like a
//!   source that downloaded yesterday but failed to render. "Sync
//!   yolink" means yolink; nothing happens for slack. Inside the
//!   subgraph the usual change propagation applies, so a fan-in
//!   re-runs only if a selected chain actually moved. This is the
//!   per-source "Sync now" mode; a full run (no `--sync`) picks up
//!   whatever was left pending.
//! * `--now` pins the run timestamp, exported to every step as
//!   `DATALIB_DAG_NOW` (downloads stamp it into raw bookkeeping,
//!   index into `markdowns.rendered_at`); omitted, the local clock is
//!   sampled once at startup so the whole run still agrees on one
//!   value.
//! * `--reset-and-redownload` / `--refetch-blobs` are exported as
//!   `DATALIB_DAG_RESET_AND_REDOWNLOAD` / `DATALIB_DAG_REFETCH_BLOBS`;
//!   steps that fetch from an origin honor them (see
//!   `datalib-step download --help`), everything else ignores them.
//!
//! Every step runs as a subprocess executing its config `command:`
//! (with the declared params/inputs/outputs appended as `--params` /
//! `--inputs` / `--outputs` JSON flags — see docs/dev/step_protocol.md).
//! Events stream to stderr as NDJSON — including one final
//! `run_summary` event, the machine-readable run record (tee stderr
//! to keep it). The per-step report prints to stdout.
//!
//! SIGINT/SIGTERM are forwarded to running steps as SIGINT so they
//! can checkpoint-commit and report a `cancelled` outcome; the
//! scheduler then drains, emits the run summary, and exits 130.
//! A config with a broken *entry* no longer stops the run: that entry
//! is dropped, everything else runs, and the diagnostics are printed
//! before the plan. See `datalib_dag::diagnostics`. A dropped entry is
//! not "all ok", so the run exits 2 even when every step that did run
//! succeeded.
//!
//! Exit codes: 0 all ok, 2 some step failed/blocked or some config
//! entry was dropped, 130 cancelled, 1 setup error (including a config
//! file that is not a config).
//!
//! Only one runner may hold a data root at a time (`system/runner-lock`,
//! `flock(2)`), so a sync started from a terminal and one started by the
//! app refuse rather than interleave. The refusal names the holder.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use datalib_dag::scheduler::StepStatus;

// `DATALIB_VERSION` is `git describe` at build time under Bazel
// release stamping (see BUILD.bazel `rustc_env_files`); dev builds and
// cargo builds see the unsubstituted placeholder / nothing, rendered
// as "dev".
const VERSION_RESOLVED: &str = {
    let raw = match option_env!("DATALIB_VERSION") {
        Some(r) => r,
        None => "",
    };
    if raw.is_empty() || raw.as_bytes()[0] == b'{' {
        "dev"
    } else {
        raw
    }
};
use datalib_dag::events::FanOutSink;
use datalib_dag::progress_bus::ProgressBusSink;
use datalib_dag::step::FailureKind;
use datalib_dag::{config, subprocess, EventSink, NdjsonSink, Runner};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    const USAGE: &str = "usage: datalib-dag <config.toml> [--binary-dir DIR] \
         [--sync STEP_ID[,STEP_ID…]]… [--now RFC3339] [--parallelism N] \
         [--reset-and-redownload] [--refetch-blobs]\n       \
         datalib-dag --check <config.toml>";
    let mut config_path: Option<PathBuf> = None;
    let mut binary_dir: Option<PathBuf> = None;
    let mut sync_only: Vec<String> = Vec::new();
    let mut now: Option<String> = None;
    let mut parallelism: Option<usize> = None;
    let mut reset_and_redownload = false;
    let mut refetch_blobs = false;
    let mut check_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--binary-dir" => {
                binary_dir = Some(PathBuf::from(
                    args.next().context("--binary-dir needs a value")?,
                ))
            }
            "--sync" => {
                let v = args.next().context("--sync needs a step id")?;
                sync_only.extend(v.split(',').map(|s| s.trim().to_string()));
            }
            "--now" => now = Some(args.next().context("--now needs a value")?),
            "--parallelism" => {
                parallelism = Some(
                    args.next()
                        .context("--parallelism needs a value")?
                        .parse()
                        .context("--parallelism must be a positive integer")?,
                )
            }
            "--reset-and-redownload" => reset_and_redownload = true,
            "--refetch-blobs" => refetch_blobs = true,
            "--check" => check_only = true,
            "--version" | "-V" => {
                #[allow(clippy::disallowed_macros)]
                {
                    println!("datalib-dag {VERSION_RESOLVED}");
                }
                return Ok(());
            }
            "-h" | "--help" => {
                // stdout is this tool's interface; no bars in play.
                #[allow(clippy::disallowed_macros)]
                {
                    println!("{USAGE}");
                }
                return Ok(());
            }
            _ if config_path.is_none() => config_path = Some(PathBuf::from(a)),
            other => bail!("unexpected argument {other:?}"),
        }
    }
    let config_path = config_path.context(USAGE)?;
    if let Some(0) = parallelism {
        bail!("--parallelism must be at least 1");
    }

    let (checked, data_root) = config::load_graded(&config_path)?;

    // Say what is wrong before doing anything, whether or not we are
    // about to run. Printed to stderr so a `--check` used in a pipeline
    // and a real run both put diagnostics in the same place, and
    // neither mixes them into the per-step report on stdout.
    if !checked.is_clean() {
        #[allow(clippy::disallowed_macros)]
        {
            eprintln!("{}", checked.render(&config_path));
        }
    }
    if checked.is_fatal() {
        // Nothing loaded, so there is nothing to run and nothing more
        // to say. Exit 1: a setup error, the same as an unreadable
        // file, because that is what it is.
        bail!(
            "{} is not a config: nothing in it could be read",
            config_path.display()
        );
    }
    if check_only {
        #[allow(clippy::disallowed_macros)]
        {
            println!(
                "{}: {} step(s), {} applet(s){}",
                config_path.display(),
                checked.cfg.steps.len(),
                checked.cfg.applets.len(),
                match checked.dropped() {
                    0 => String::new(),
                    n => format!(", {n} entr{} dropped", if n == 1 { "y" } else { "ies" }),
                }
            );
        }
        std::process::exit(if checked.is_clean() { 0 } else { 2 });
    }
    let dropped_entries = checked.dropped();
    let cfg = checked.cfg;
    let graph = checked.graph;

    // One runner per data root, taken before anything is written.
    //
    // Two runners interleave `system/dag_state.json` — rewritten after
    // every terminal step — and interleave the raw stores their steps
    // write, whose doltlite working set is shared across processes. So
    // a sync started from a terminal while the app is syncing, or two
    // terminals, would corrupt bookkeeping quietly rather than loudly.
    //
    // Held until the process exits, however it exits: `flock(2)` is
    // released by the kernel, so a crashed run leaves nothing to clean
    // up. `_lock` and not `_` — binding to `_` would drop it here and
    // release the claim before the run starts.
    let _lock = datalib_dag::lock::FileLock::acquire_runner(&data_root).map_err(|e| {
        if e.is_held() {
            anyhow::anyhow!(
                "another datalib-dag is already running against {}{}.\n\
                 Two runners on one data root overwrite each other's scheduler state and \
                 each other's raw stores. Wait for it to finish, or point this one at a \
                 different root.\n(lock: {})",
                data_root.display(),
                match e.holder() {
                    Some(h) => format!(" — {h}"),
                    None => String::new(),
                },
                e.path().display()
            )
        } else {
            anyhow::anyhow!("{e}")
        }
    })?;

    // Run-wide environment for every step subprocess: PATH with the
    // binary dir prepended (so commands can say `datalib-step` bare),
    // one pinned timestamp for the whole run — whether given or
    // sampled, every stamped output (raw bookkeeping, rendered_at)
    // agrees — and the reset flags for steps that fetch from origin.
    let mut child_env: std::collections::BTreeMap<String, String> = Default::default();
    if let Some(dir) = config::resolve_binary_dir(&cfg, binary_dir.as_deref()) {
        let mut paths = vec![dir];
        if let Some(p) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&p));
        }
        let joined = std::env::join_paths(paths).context("prepend --binary-dir to PATH")?;
        child_env.insert("PATH".into(), joined.to_string_lossy().into_owned());
    }
    let now =
        now.unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    // The scheduler takes its run id from ENV_NOW, so this is the same
    // string it will stamp on the run — keep it for the progress bus.
    let run_id = now.clone();
    child_env.insert(subprocess::ENV_NOW.into(), now);
    if reset_and_redownload {
        child_env.insert(subprocess::ENV_RESET_AND_REDOWNLOAD.into(), "1".into());
    }
    if refetch_blobs {
        child_env.insert(subprocess::ENV_REFETCH_BLOBS.into(), "1".into());
    }

    if !sync_only.is_empty() {
        let fringe = graph.fringe_ids();
        for id in &sync_only {
            if !fringe.contains(&id.as_str()) {
                bail!(
                    "--sync {id:?}: not a source step (a step with no inputs). \
                     Available: {}",
                    fringe.join(", ")
                );
            }
        }
    }

    // Cancellation: forward the first SIGINT/SIGTERM to running steps
    // as SIGINT so each can checkpoint-commit and exit with a
    // `cancelled` outcome; the scheduler drains normally. A second
    // signal gives up waiting and exits hard (kill_on_drop reaps any
    // stragglers).
    tokio::spawn(async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut interrupts = 0u32;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            interrupts += 1;
            if interrupts >= 2 {
                std::process::exit(130);
            }
            subprocess::interrupt_children();
        }
    });

    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("create data_root {}", data_root.display()))?;
    // stderr stays the log; the bus is the live view. Both, not either:
    // the stream is a record of everything that happened, the bus is a
    // coalesced answer to "what is happening now" that a second process
    // can read. Publishing the bus here rather than inside `Runner`
    // means every way of starting a sync gets it — the http server's
    // worker shells out to this binary too — while a library caller
    // embedding `Runner` is not forced to own a file.
    //
    // The bus owns a writer thread, and this binary ends in
    // `std::process::exit`, which runs no destructors. So the bus gets
    // an explicit scope that computes the exit code, and the exit
    // happens *after* that scope closes.
    //
    // That is the nursery discipline from Nathaniel J. Smith's "Notes
    // on structured concurrency, or: Go statement considered harmful"
    // (https://vorpus.org/blog/notes-on-structured-concurrency-or-go-statement-considered-harmful/):
    // a spawned thing's lifetime is bracketed by a scope its parent
    // cannot leave early, so there is no path out that forgets it. The
    // first version of this called a `finish()` by hand instead, which
    // is precisely the unstructured spawn the essay argues against —
    // and it was already wrong, because the `?` on `runner.run` below
    // skips it. `//tests/fixtures:progress_bus_e2e_test` is what caught
    // the empty bus, and is what would catch it coming back.
    let code = {
        let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::new(NdjsonSink::new(std::io::stderr()))];
        match ProgressBusSink::start(&data_root, &run_id) {
            Some(bus) => sinks.push(Arc::new(bus)),
            None => {
                // This binary has no tracing subscriber and no indicatif
                // bars, so the macro's usual objection does not apply and
                // `tracing::warn!` would go nowhere at all. The NDJSON
                // reader tolerates non-JSON lines (worker.rs skips what it
                // cannot parse), so a plain line is safe here.
                #[allow(clippy::disallowed_macros)]
                {
                    eprintln!("datalib-dag: progress bus unavailable; no live progress this run");
                }
            }
        }
        let mut runner = Runner::new(&data_root)
            .sink(Arc::new(FanOutSink(sinks)))
            .child_env(child_env);
        if let Some(p) = parallelism {
            runner.parallelism = p;
        }
        if !sync_only.is_empty() {
            runner = runner.only_fringe(sync_only);
        }
        let report = runner.run(&graph).await?;

        #[allow(clippy::disallowed_macros)]
        for s in &report.steps {
            println!(
                "{:<32} {:?}{}",
                s.id,
                s.status,
                s.error
                    .as_deref()
                    .map(|e| format!("  ({e})"))
                    .unwrap_or_default()
            );
        }
        // A dropped config entry is reported here as well as before
        // the run, because the per-step report is the thing a person
        // actually reads at the end — and an entry that was dropped has
        // no row in it, so silence would read as "everything ran".
        if dropped_entries > 0 {
            #[allow(clippy::disallowed_macros)]
            {
                println!(
                    "\n{dropped_entries} config entr{} dropped and did not run; \
                     see the diagnostics above, or run `datalib-dag --check {}`",
                    if dropped_entries == 1 {
                        "y was"
                    } else {
                        "ies were"
                    },
                    config_path.display()
                );
            }
        }
        let cancelled = report.steps.iter().any(|s| {
            matches!(
                s.status,
                StepStatus::Failed {
                    kind: FailureKind::Cancelled
                }
            )
        });
        if cancelled {
            130
        } else if report.all_ok() && dropped_entries == 0 {
            0
        } else {
            2
        }
    }; // every sink drops here, so the bus flushes and joins before we exit
    std::process::exit(code);
}
