//! `datalib-step` — the step-type host binary for the DAG runner.
//!
//! Each subcommand is one step type under the DAG step contract (see
//! `datalib_dag` and docs/dev/step_protocol.md): it reads
//! artifacts under the data root, writes its declared outputs,
//! streams NDJSON progress events on stdout, and finishes with one
//! `{"event":"outcome",…}` line reporting per-output change status.
//! The DAG config invokes it as an ordinary step `command:`
//! (`command: datalib-step download slack_api`); the runner appends
//! the entry's declared `params`/`inputs`/`outputs` as
//! `--params`/`--inputs`/`--outputs` JSON flags.
//!
//! Step types:
//!
//! * `download <source_type>` — one source's download wave, via the
//!   provider's own `DataProcessor`s. Writes the tree its step id
//!   names, which the runner passes in `DATALIB_DAG_STEP`
//!   (`slack/raw`). `--params` is the provider's own download config
//!   subtree (no `type:` tag — the subcommand names the provider, no
//!   `name:` — the id carries it).
//! * `render <source_type>` — the source's render wave. `--params`
//!   here is the provider's slim render config (render knobs only —
//!   the per-phase params split; see `dispatch.rs`).
//!   Writes the tree its id names (`.md` files plus the
//!   `.grid_rows.json` sidecars the providers already emit).
//!   Incremental: docs whose sidecar fingerprint is unchanged are
//!   skipped, using the sidecar tree itself as the prior-fingerprint
//!   store (no index-DB peeking — that's the un-fused contract).
//! * `grid_index` — rebuild/refresh the unified grid table
//!   (`unified_index/grid`) from every source's sidecar tree
//!   (`build_grid_index`, per-doc fingerprint skip), then `dolt_commit`. This
//!   is the load step un-fused from render.
//! * `qmd_index` — the qmd search index over every rendered_md tree,
//!   writing `unified_index/qmd`.
//! * `probe <source_type>` — utility, not a pipeline step: ask a
//!   provider what a set of credentials can reach (the account, its
//!   labels), and print one JSON object. Backs the wizard's "Test
//!   connection" button; see `probe.rs`.
//! * `synthesize` — dev utility, not a pipeline step: build HTTP
//!   playback fixtures for one source from its `input_path` raw
//!   fixture tree (the `--synthesize-playback-root` mode of the old
//!   sync binary, one source per invocation). Takes an explicit
//!   `--name` (there is no step id to take it from).
//!
//! Identity comes from the runner via `DATALIB_DAG_STEP` /
//! `DATALIB_DAG_DATA_ROOT` (falling back to the CWD, which the
//! runner also sets to the data root); run-wide settings via
//! `DATALIB_DAG_NOW` and the reset env vars (each overridable by
//! the corresponding flag for standalone runs). Tracing goes to
//! stderr; stdout carries only the event stream.

mod dispatch;
mod download;
mod events;
mod grid_index;
mod hints;
mod probe;
mod qmd_index;
mod render;
mod source;
mod synth;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use datalib_dag::subprocess::{
    ENV_DATA_ROOT, ENV_NOW, ENV_REFETCH_BLOBS, ENV_RESET_AND_REDOWNLOAD, ENV_STEP,
};

use crate::events::Emitter;

#[derive(Parser)]
#[command(
    name = "datalib-step",
    about = "Step-type host for the datalib DAG runner"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Step params, as JSON — the runner appends this from the config
    /// entry's `params:`. Phase-specific: for download it is the
    /// provider's download config subtree, for render the slim render
    /// config (render knobs only); absent means an empty one.
    #[arg(long, global = true)]
    params: Option<String>,
    /// Declared input artifact patterns (JSON string array), appended
    /// by the runner from the config entry's `inputs:`. Accepted so
    /// every step command shares one flag surface; the fan-in step
    /// types rescan the data root rather than consuming it.
    #[arg(long, global = true)]
    inputs: Option<String>,
    /// Declared output artifact paths (JSON string array), appended by
    /// the runner as the single tree this step's id names. Accepted so
    /// every step command shares one flag surface, and so a step
    /// written against the older contract still parses — nothing here
    /// reads it. Identity comes from `DATALIB_DAG_STEP`.
    #[arg(long, global = true)]
    outputs: Option<String>,
    /// Fixed "now" timestamp (RFC 3339), stamped wherever this step
    /// type records times (raw bookkeeping, `markdowns.rendered_at`).
    /// Falls back to `$DATALIB_DAG_NOW` (the runner exports one
    /// value so the whole run agrees), then the local clock.
    #[arg(long, global = true)]
    now: Option<String>,
    /// Download only: wipe every entity table (and its bookkeeping
    /// sidecar) before fetching, re-downloading every entity row. The
    /// provider's CAS edge table is preserved, so already-fetched
    /// attachment bytes are not re-pulled — see `--refetch-blobs`.
    /// Falls back to `$DATALIB_DAG_RESET_AND_REDOWNLOAD=1`.
    #[arg(long, global = true)]
    reset_and_redownload: bool,
    /// Download only: clear the `blake3` column on the provider's CAS
    /// edge table so every attachment re-fetches on the wire (the CAS
    /// itself is never truncated). Falls back to
    /// `$DATALIB_DAG_REFETCH_BLOBS=1`.
    #[arg(long, global = true)]
    refetch_blobs: bool,
    #[command(flatten)]
    obs: datalib_obs::ObsArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// One source's download wave → `<name>/raw`.
    /// Invoked `datalib-step download <source_type>` — the provider
    /// is the next word, like a nested subcommand.
    Download {
        /// Source type (`slack_api`, `claude_api`, …) — the provider
        /// this step dispatches to.
        source_type: String,
        /// HTTP playback fixture tree (hermetic runs); sets
        /// `DATALIB_HTTP_PLAYBACK` for every provider transport.
        #[arg(long)]
        playback_root: Option<PathBuf>,
    },
    /// One source's render wave → `<name>/rendered_md`. Invoked
    /// `datalib-step render <source_type>`.
    Render { source_type: String },
    /// Rebuild the unified grid table (`unified_index/grid`) from
    /// every sidecar tree.
    #[command(name = "grid_index")]
    GridIndex,
    /// Build the qmd search index → `unified_index/qmd`.
    #[command(name = "qmd_index")]
    QmdIndex {
        /// Directory where qmd caches its embedding model.
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Utility (not a pipeline step): ask a provider what these
    /// credentials can reach, and print one JSON object on stdout.
    /// Writes nothing and needs no data root.
    Probe {
        /// Source type, same position as in `download <source_type>`.
        source_type: String,
    },
    /// Dev utility (not a pipeline step): build HTTP playback fixtures
    /// for one source from its `input_path` raw fixture tree, for
    /// later replay via `download --playback-root`.
    Synthesize {
        /// Source type, same position as in `download <source_type>`.
        source_type: String,
        /// Source name (the `<name>/…` directory prefix). Explicit
        /// here — a dev invocation has no step id to take it from.
        #[arg(long)]
        name: String,
        /// Output directory for the playback fixture tree.
        #[arg(long)]
        out: PathBuf,
    },
}

/// Truthy run-wide env flag exported by the runner.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Checkpoint hooks registered by the running step (today only
/// `download` populates it), fired from the SIGINT handler so partial
/// state gets a tidy commit before exit.
static CHECKPOINTS: std::sync::OnceLock<std::sync::Arc<datalib_etl::processor::CheckpointSink>> =
    std::sync::OnceLock::new();

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let _obs_guard = datalib_obs::init(&cli.obs, "datalib-step").ok();

    // `probe` is answered before any of the step machinery below: it
    // owns no tree, claims no outputs and must leave stdout holding
    // exactly one JSON object, so an `outcome` event line after it
    // would corrupt the only thing its caller reads.
    if let Cmd::Probe { source_type } = &cli.cmd {
        probe::run_cli(source_type, cli.params.as_deref()).await;
    }

    let step_id = std::env::var(ENV_STEP).unwrap_or_else(|_| "step".to_string());
    let data_root = std::env::var_os(ENV_DATA_ROOT)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .expect("no data root: set DATALIB_DAG_DATA_ROOT or run inside one");
    let emitter = Emitter::new(step_id);

    // SIGINT (terminal Ctrl-C, or forwarded by the runner on cancel):
    // fire any registered checkpoint hooks — each commits its store's
    // partial state and the providers' idempotency makes the next run
    // resume from there — then report a `cancelled` outcome and exit
    // 130. Steps without checkpoints (render/index/qmd) just stop;
    // their stores roll back or re-derive next run.
    let sig_emitter = emitter.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            if let Some(checkpoints) = CHECKPOINTS.get() {
                for entry in checkpoints.snapshot() {
                    match entry.hook.checkpoint().await {
                        Ok(_) => tracing::info!(source = %entry.name, "interrupt checkpoint: ok"),
                        Err(e) => {
                            tracing::warn!(source = %entry.name, "interrupt checkpoint: {e:#}")
                        }
                    }
                }
            }
            sig_emitter.outcome(&[], Some("cancelled"));
            std::process::exit(130);
        }
    });

    let now = cli
        .now
        .clone()
        .or_else(|| std::env::var(ENV_NOW).ok())
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    let control = datalib_etl::control::DownloadControl {
        reset_and_redownload: cli.reset_and_redownload || env_flag(ENV_RESET_AND_REDOWNLOAD),
        refetch_blobs: cli.refetch_blobs || env_flag(ENV_REFETCH_BLOBS),
    };

    let step_io = StepIo { params: cli.params };
    match run(cli.cmd, &step_io, &data_root, &now, &control, &emitter).await {
        Ok(outputs) => {
            emitter.outcome(&outputs, None);
        }
        Err(e) => {
            let kind = hints::classify(&e);
            // A failed-but-incremental step may still have committed
            // partial output; with no claims the scheduler re-hashes
            // the declared outputs and sees whatever landed.
            emitter.outcome(&[], Some(kind));
            for (i, cause) in e.chain().enumerate() {
                let prefix = if i == 0 { "error" } else { "caused by" };
                tracing::error!("{prefix}: {cause}");
                // `status_line!`, not `eprintln!`: it suspends the
                // progress bars across the write (and falls through to
                // raw stderr when the draw target is hidden, e.g. when
                // the http worker spawned us with stderr piped).
                datalib_obs::status_line!("{prefix}: {cause}");
            }
            std::process::exit(1);
        }
    }
}

/// The runner-appended step declaration as received; parsed on demand
/// by the step types that consume it.
struct StepIo {
    params: Option<String>,
}

async fn run(
    cmd: Cmd,
    io: &StepIo,
    data_root: &Path,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<events::OutputClaim>> {
    match cmd {
        Cmd::Download {
            source_type,
            playback_root,
        } => {
            if let Some(pb) = playback_root {
                let pb = pb.canonicalize().context("playback root")?;
                std::env::set_var(datalib_etl::http::PLAYBACK_ENV, pb);
            }
            let tree = source::tree_from_env()?;
            let name = source::source_name(&tree).to_string();
            let params = source::parse_params(io.params.as_deref())?;
            let planned = dispatch::plan(
                &source_type,
                dispatch::Phase::Download,
                &name,
                params,
                data_root,
            )?;
            let res = download::run(&planned, data_root, now, control, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, planned.type_str, &res);
            res
        }
        Cmd::Render { source_type } => {
            let tree = source::tree_from_env()?;
            let name = source::source_name(&tree).to_string();
            let params = source::parse_params(io.params.as_deref())?;
            let planned = dispatch::plan(
                &source_type,
                dispatch::Phase::Render,
                &name,
                params,
                data_root,
            )?;
            let type_str = planned.type_str;
            let res = render::run(planned, data_root, now, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, type_str, &res);
            res
        }
        // Handled in `main` before the step machinery starts; see
        // there for why it cannot come through the outcome path.
        Cmd::Probe { .. } => unreachable!("probe is answered in main"),
        Cmd::GridIndex => grid_index::run(data_root, Some(now), emitter).await,
        Cmd::QmdIndex { models_dir } => qmd_index::run(data_root, models_dir, emitter).await,
        Cmd::Synthesize {
            source_type,
            name,
            out,
        } => {
            let params = source::parse_params(io.params.as_deref())?;
            synth::run(&source_type, &name, &params, data_root, &out, emitter)
        }
    }
}
