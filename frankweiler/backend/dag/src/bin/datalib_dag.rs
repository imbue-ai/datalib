//! `datalib-dag` — run a DAG config file (the new-format config; see
//! `frankweiler_dag::config` for the schema).
//!
//! ```sh
//! datalib-dag pipeline.yaml [--step-bin PATH] [--sync STEP_ID[,…]]…
//!     [--now RFC3339] [--parallelism N]
//!     [--reset-and-redownload] [--refetch-blobs]
//! ```
//!
//! * `--sync` selects a subset of the download steps (the steps with
//!   no inputs) to actually sync; the rest are treated as up to date,
//!   so only the selected chains — plus any fan-in steps they dirty —
//!   do work. This is the per-source "Sync now" mode.
//! * `--now` pins the run timestamp threaded to every `step:`-typed
//!   entry (downloads stamp it into raw bookkeeping, index into
//!   `markdowns.rendered_at`); omitted, the local clock is sampled
//!   once at startup so the whole run still agrees on one value.
//! * `--reset-and-redownload` / `--refetch-blobs` are forwarded to
//!   the download steps (see `datalib-step download --help`).
//!
//! Every step runs as a subprocess: `step:`-typed entries invoke
//! `datalib-step <type> …`; `run:` entries execute their argv
//! verbatim. Events stream to stderr as NDJSON — including one final
//! `run_summary` event, the machine-readable run record (tee stderr
//! to keep it). The per-step report prints to stdout.
//!
//! SIGINT/SIGTERM are forwarded to running steps as SIGINT so they
//! can checkpoint-commit and report a `cancelled` outcome; the
//! scheduler then drains, emits the run summary, and exits 130.
//! Exit codes: 0 all ok, 2 some step failed/blocked, 130 cancelled,
//! 1 setup error.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use frankweiler_dag::scheduler::StepStatus;

// `FRANKWEILER_VERSION` is `git describe` at build time under Bazel
// release stamping (see BUILD.bazel `rustc_env_files`); dev builds and
// cargo builds see the unsubstituted placeholder / nothing, rendered
// as "dev".
const VERSION_RESOLVED: &str = {
    let raw = match option_env!("FRANKWEILER_VERSION") {
        Some(r) => r,
        None => "",
    };
    if raw.is_empty() || raw.as_bytes()[0] == b'{' {
        "dev"
    } else {
        raw
    }
};
use frankweiler_dag::step::FailureKind;
use frankweiler_dag::{config, subprocess, Graph, NdjsonSink, Runner};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    const USAGE: &str = "usage: datalib-dag <pipeline.yaml> [--step-bin PATH] \
         [--sync STEP_ID[,STEP_ID…]]… [--now RFC3339] [--parallelism N] \
         [--reset-and-redownload] [--refetch-blobs]";
    let mut config_path: Option<PathBuf> = None;
    let mut step_bin: Option<PathBuf> = None;
    let mut sync_only: Vec<String> = Vec::new();
    let mut now: Option<String> = None;
    let mut parallelism: Option<usize> = None;
    let mut reset_and_redownload = false;
    let mut refetch_blobs = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--step-bin" => {
                step_bin = Some(PathBuf::from(
                    args.next().context("--step-bin needs a value")?,
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

    let (cfg, data_root) = config::load(&config_path)?;
    let step_bin = config::resolve_step_bin(&cfg, step_bin.as_deref());
    // One timestamp for the whole run, whether given or sampled —
    // every stamped output (raw bookkeeping, rendered_at) agrees.
    let opts = config::StepTypeOpts {
        now: Some(now.unwrap_or_else(|| {
            frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs()
        })),
        reset_and_redownload,
        refetch_blobs,
    };
    let specs = config::to_specs(&cfg, &step_bin, &opts)?;
    let graph = Graph::build(specs)?;

    if !sync_only.is_empty() {
        let fringe = graph.fringe_ids();
        for id in &sync_only {
            if !fringe.contains(&id.as_str()) {
                bail!(
                    "--sync {id:?}: not a download step (a step with no inputs). \
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
    let mut runner = Runner::new(&data_root).sink(Arc::new(NdjsonSink::new(std::io::stderr())));
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
    let cancelled = report.steps.iter().any(|s| {
        matches!(
            s.status,
            StepStatus::Failed {
                kind: FailureKind::Cancelled
            }
        )
    });
    let code = if cancelled {
        130
    } else if report.all_ok() {
        0
    } else {
        2
    };
    std::process::exit(code);
}
