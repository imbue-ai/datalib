//! `datalib-step` — the built-in step program for the DAG runner.
//!
//! Run with no subcommand it is a step: it reads which function to
//! perform, which group it is under and what tree to write from the
//! environment the runner sets (`DATALIB_DAG_FUNCTION`, `DATALIB_DAG_GROUP`,
//! `DATALIB_DAG_GROUP_TYPE`, `DATALIB_DAG_STEP`). The two subcommands are
//! utilities that are not steps.

mod dispatch;
mod download;
mod events;
mod function;
mod grid_index;
mod hints;
mod introspect;
mod probe;
mod qmd_index;
mod render;
mod source;
mod source_type;
mod synth;

#[cfg(test)]
mod config_examples_test;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use datalib_dag::subprocess::{
    ENV_CHECKPOINT_CADENCE, ENV_DATA_ROOT, ENV_NOW, ENV_REFETCH_BLOBS, ENV_RESET_AND_REDOWNLOAD,
    ENV_STEP,
};
use datalib_dag::FailureKind;

use crate::events::Emitter;
use crate::function::Function;
use crate::source::StepEnv;

#[derive(Parser)]
#[command(
    name = "datalib-step",
    about = "The built-in step program for the datalib DAG runner"
)]
struct Cli {
    /// Absent means "be a step": the function, group and tree come from
    /// the environment the runner sets.
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Step params, as JSON — the runner appends this from the config
    /// entry's `params`. Phase-specific: for `ingest` it is the
    /// provider's download config subtree, for `render_markdown` the slim
    /// render config (render knobs only); absent means an empty one.
    #[arg(long, global = true)]
    params: Option<String>,
    /// Declared input step ids (JSON string array), appended by the
    /// runner from the config entry's `inputs`. Accepted so every step
    /// command shares one flag surface; the resolved list this binary
    /// acts on is `DATALIB_DAG_INPUTS`.
    #[arg(long, global = true)]
    inputs: Option<String>,
    /// Fixed "now" timestamp (RFC 3339), stamped wherever this step
    /// type records times (raw bookkeeping, `markdowns.rendered_at`).
    /// Falls back to `$DATALIB_DAG_NOW` (the runner exports one
    /// value so the whole run agrees), then the local clock.
    #[arg(long, global = true)]
    now: Option<String>,
    /// Ingest only: wipe every entity table (and its bookkeeping
    /// sidecar) before fetching, re-downloading every entity row. The
    /// provider's CAS edge table is preserved, so already-fetched
    /// attachment bytes are not re-pulled — see `--refetch-blobs`.
    /// Falls back to `$DATALIB_DAG_RESET_AND_REDOWNLOAD=1`.
    #[arg(long, global = true)]
    reset_and_redownload: bool,
    /// Ingest only: clear the `blake3` column on the provider's CAS
    /// edge table so every attachment re-fetches on the wire (the CAS
    /// itself is never truncated). Falls back to
    /// `$DATALIB_DAG_REFETCH_BLOBS=1`.
    #[arg(long, global = true)]
    refetch_blobs: bool,
    /// Ingest only: HTTP playback fixture tree (hermetic runs); sets
    /// `DATALIB_HTTP_PLAYBACK` for every provider transport.
    #[arg(long)]
    playback_root: Option<PathBuf>,
    /// `qmd_index` only: directory where qmd caches its embedding model.
    #[arg(long)]
    models_dir: Option<PathBuf>,
    #[command(flatten)]
    obs: datalib_obs::ObsArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Utility (not a pipeline step): ask a provider what these
    /// credentials can reach, and print one JSON object on stdout.
    /// Writes nothing and needs no data root.
    Probe {
        /// Source type (`slack_api`, `claude_api`, …): the provider to
        /// ask.
        source_type: String,
    },
    /// Dev utility (not a pipeline step): build HTTP playback fixtures
    /// for one source from its `input_path` raw fixture tree, for
    /// later replay via `--playback-root`.
    Synthesize {
        /// Source type, as a group's `type` would name it.
        source_type: String,
        /// Group id (the `<group>/…` directory prefix). Explicit
        /// here — a dev invocation has no step id to take it from.
        #[arg(long)]
        name: String,
        /// Output directory for the playback fixture tree.
        #[arg(long)]
        out: PathBuf,
    },
}

/// The cadence the config asked for, as the `etl` type.
///
/// `None` on anything unreadable rather than a failure: a malformed value
/// from a newer config should not take the run down, and the step's own
/// default is a safe answer. It is logged, though — a fallback that fires
/// silently is the kind this repo has been burned by.
fn checkpoint_cadence() -> Option<datalib_etl::checkpointer::Cadence> {
    let raw = std::env::var(ENV_CHECKPOINT_CADENCE).ok()?;
    match datalib_dag::config::CheckpointCadence::decode(&raw) {
        Some(c) => Some(datalib_etl::checkpointer::Cadence {
            quiet_for: std::time::Duration::from_secs_f64(c.quiet_for_secs),
            at_most_every: std::time::Duration::from_secs_f64(c.at_most_every_secs),
        }),
        None => {
            tracing::warn!(
                raw = %raw,
                "{ENV_CHECKPOINT_CADENCE} is not a readable cadence; using the default"
            );
            None
        }
    }
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Checkpoint hooks registered by the running step (today only
/// `ingest` populates it), fired from the SIGINT handler so partial
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
    if let Some(Cmd::Probe { source_type }) = &cli.cmd {
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
            sig_emitter.outcome(&[], Some(FailureKind::Cancelled));
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
        checkpoint_cadence: checkpoint_cadence(),
    };

    match run(cli, &data_root, &now, &control, &emitter).await {
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

async fn run(
    cli: Cli,
    data_root: &Path,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<events::OutputClaim>> {
    let params = source::parse_params(cli.params.as_deref())?;
    match cli.cmd {
        Some(Cmd::Synthesize {
            source_type,
            name,
            out,
        }) => synth::run(&source_type, &name, &params, data_root, &out, emitter),
        // Handled in `main` before the step machinery starts; see
        // there for why it cannot come through the outcome path.
        Some(Cmd::Probe { .. }) => unreachable!("probe is answered in main"),
        None => {
            let env = StepEnv::from_env()?;
            run_function(
                env,
                cli.playback_root,
                cli.models_dir,
                params,
                data_root,
                now,
                control,
                emitter,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_function(
    env: StepEnv,
    playback_root: Option<PathBuf>,
    models_dir: Option<PathBuf>,
    params: serde_json::Value,
    data_root: &Path,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<events::OutputClaim>> {
    match env.function {
        Function::Ingest => {
            if let Some(pb) = playback_root {
                let pb = pb.canonicalize().context("playback root")?;
                std::env::set_var(datalib_etl::http::PLAYBACK_ENV, pb);
            }
            let planned = dispatch::plan(
                env.source_type()?,
                dispatch::Phase::Ingest,
                &env.group,
                data_root.join(&env.step),
                params,
            )?;
            let res = download::run(&planned, &env.step, now, control, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, planned.source_type, &res);
            res
        }
        Function::RenderMarkdown => {
            let raw_rel = env.raw_store_rel();
            let planned = dispatch::plan(
                env.source_type()?,
                dispatch::Phase::Render,
                &env.group,
                data_root.join(&raw_rel),
                params,
            )?;
            let source_type = planned.source_type;
            let res = render::run(planned, &env, &raw_rel, data_root, now, emitter, control).await;
            hints::emit_auth_hint_on_failure(emitter, source_type, &res);
            res
        }
        Function::GridIndex => {
            writes_the_index_tree(&env, &grid_index::out_rel())?;
            grid_index::run(data_root, Some(now), emitter).await
        }
        Function::QmdIndex => {
            writes_the_index_tree(&env, &qmd_index::out_rel())?;
            qmd_index::run(data_root, models_dir, emitter).await
        }
    }
}

/// The two index steps have one reader each — the `unified_index`
/// applet — which finds them from the data root alone, so their trees
/// are fixed. A config that files them under another group would have
/// the runner tracking a tree nothing ever writes.
fn writes_the_index_tree(env: &StepEnv, expected: &str) -> Result<()> {
    anyhow::ensure!(
        env.step == expected,
        "`{}` writes {expected:?} and nothing else, but this step's id is {:?}: declare it \
         under the `unified_index` group",
        env.function,
        env.step
    );
    Ok(())
}
