//! `datalib-step` — the step-type host binary for the DAG runner.
//!
//! Each subcommand is one step type under the DAG step contract (see
//! `frankweiler_dag` and docs/dev/step_protocol.md): it reads
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
//!   provider's own `DataProcessor`s. Writes `<name>/raw`, where the
//!   source name comes from the first declared output (`slack/raw` →
//!   `slack`). `--params` is the provider's own download config
//!   subtree (no `type:` tag — the subcommand names the provider, no
//!   `name:` — the outputs carry it).
//! * `render <source_type>` — the source's render wave. `--params`
//!   here is the provider's slim render config (render knobs only —
//!   the per-phase params split; see `dispatch.rs`).
//!   Writes `<name>/rendered_md` (`.md` files plus the
//!   `.grid_rows.json` sidecars the providers already emit).
//!   Incremental: docs whose sidecar fingerprint is unchanged are
//!   skipped, using the sidecar tree itself as the prior-fingerprint
//!   store (no index-DB peeking — that's the un-fused contract).
//! * `grid_index` — rebuild/refresh the unified grid table
//!   (`system/backend_index`) from every stanza's sidecar tree
//!   (`build_grid_index`, per-doc fingerprint skip), then `dolt_commit`. This
//!   is the load step un-fused from render.
//! * `qmd_index` — the qmd search index over every rendered_md tree,
//!   writing `system/qmd`.
//! * `synthesize` — dev utility, not a pipeline step: build HTTP
//!   playback fixtures for one source from its `input_path` raw
//!   fixture tree (the `--synthesize-playback-root` mode of the old
//!   sync binary, one source per invocation). Takes an explicit
//!   `--name` (no outputs to derive it from).
//!
//! Identity comes from the runner via `FRANKWEILER_DAG_STEP` /
//! `FRANKWEILER_DAG_DATA_ROOT` (falling back to the CWD, which the
//! runner also sets to the data root); run-wide settings via
//! `FRANKWEILER_DAG_NOW` and the reset env vars (each overridable by
//! the corresponding flag for standalone runs). Tracing goes to
//! stderr; stdout carries only the event stream.

mod dispatch;
mod download;
mod events;
mod grid_index;
mod hints;
mod qmd_index;
mod render;
mod source;
mod synth;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use frankweiler_dag::subprocess::{
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
    /// Declared output artifact paths (JSON string array), appended
    /// by the runner from the config entry's `outputs:`.
    /// Download/render derive their source name from the first entry
    /// (`slack/raw` → `slack`).
    #[arg(long, global = true)]
    outputs: Option<String>,
    /// Fixed "now" timestamp (RFC 3339), stamped wherever this step
    /// type records times (raw bookkeeping, `markdowns.rendered_at`).
    /// Falls back to `$FRANKWEILER_DAG_NOW` (the runner exports one
    /// value so the whole run agrees), then the local clock.
    #[arg(long, global = true)]
    now: Option<String>,
    /// Download only: wipe every entity table (and its bookkeeping
    /// sidecar) before fetching, re-downloading every entity row.
    /// `blob_refs` is preserved — see `--refetch-blobs`. Falls back
    /// to `$FRANKWEILER_DAG_RESET_AND_REDOWNLOAD=1`.
    #[arg(long, global = true)]
    reset_and_redownload: bool,
    /// Download only: wipe `blob_refs` so every attachment re-fetches
    /// on the wire (the CAS itself is never truncated). Falls back to
    /// `$FRANKWEILER_DAG_REFETCH_BLOBS=1`.
    #[arg(long, global = true)]
    refetch_blobs: bool,
    #[command(flatten)]
    obs: frankweiler_obs::ObsArgs,
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
        /// `FRANKWEILER_HTTP_PLAYBACK` for every provider transport.
        #[arg(long)]
        playback_root: Option<PathBuf>,
    },
    /// One source's render wave → `<name>/rendered_md`. Invoked
    /// `datalib-step render <source_type>`.
    Render { source_type: String },
    /// Rebuild the unified grid table (`system/backend_index`) from
    /// every sidecar tree.
    #[command(name = "grid_index")]
    GridIndex,
    /// Build the qmd search index → `system/qmd`.
    #[command(name = "qmd_index")]
    QmdIndex {
        /// Directory where qmd caches its embedding model.
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Dev utility (not a pipeline step): build HTTP playback fixtures
    /// for one source from its `input_path` raw fixture tree, for
    /// later replay via `download --playback-root`.
    Synthesize {
        /// Source type, same position as in `download <source_type>`.
        source_type: String,
        /// Source name (the `<name>/…` directory prefix). Explicit
        /// here — a dev invocation has no declared outputs to derive
        /// it from.
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
static CHECKPOINTS: std::sync::OnceLock<
    std::sync::Arc<frankweiler_etl::processor::CheckpointSink>,
> = std::sync::OnceLock::new();

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let _obs_guard = frankweiler_obs::init(&cli.obs, "datalib-step").ok();

    let step_id = std::env::var(ENV_STEP).unwrap_or_else(|_| "step".to_string());
    let data_root = std::env::var_os(ENV_DATA_ROOT)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .expect("no data root: set FRANKWEILER_DAG_DATA_ROOT or run inside one");
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
        .unwrap_or_else(|| frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    let control = frankweiler_etl::control::DownloadControl {
        reset_and_redownload: cli.reset_and_redownload || env_flag(ENV_RESET_AND_REDOWNLOAD),
        refetch_blobs: cli.refetch_blobs || env_flag(ENV_REFETCH_BLOBS),
    };

    let step_io = StepIo {
        params: cli.params,
        outputs: cli.outputs,
    };
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
                eprintln!("{prefix}: {cause}");
            }
            std::process::exit(1);
        }
    }
}

/// The runner-appended step declaration (`--params` / `--outputs`) as
/// received; parsed on demand by the step types that consume it.
struct StepIo {
    params: Option<String>,
    outputs: Option<String>,
}

async fn run(
    cmd: Cmd,
    io: &StepIo,
    data_root: &PathBuf,
    now: &str,
    control: &frankweiler_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<events::OutputClaim>> {
    match cmd {
        Cmd::Download {
            source_type,
            playback_root,
        } => {
            if let Some(pb) = playback_root {
                let pb = pb.canonicalize().context("playback root")?;
                std::env::set_var(frankweiler_etl::http::PLAYBACK_ENV, pb);
            }
            let name = source::name_from_outputs(io.outputs.as_deref())?;
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
            let name = source::name_from_outputs(io.outputs.as_deref())?;
            let params = source::parse_params(io.params.as_deref())?;
            let planned = dispatch::plan(
                &source_type,
                dispatch::Phase::Render,
                &name,
                params,
                data_root,
            )?;
            let type_str = planned.type_str;
            let res = render::run(planned, data_root, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, type_str, &res);
            res
        }
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
