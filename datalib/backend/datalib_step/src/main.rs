//! `datalib-step` — the built-in step program for the DAG runner.
//!
//! Run with no subcommand it is a step: it reads which function to
//! perform, which group it is under and what tree to write from the
//! environment the runner sets (`DATALIB_DAG_FUNCTION`, `DATALIB_DAG_GROUP`,
//! `DATALIB_DAG_GROUP_TYPE`, `DATALIB_DAG_STEP`). The subcommands are
//! utilities that are not steps.

mod dispatch;
mod embedding_map;
mod events;
mod exit_watchdog;
mod function;
mod grid_index;
mod hints;
mod ingest;
mod introspect;
mod login;
mod methods;
mod probe;
mod published;
mod qmd_index;
mod render;
mod render_diff;
#[cfg(test)]
mod render_model_test;
mod reset;
mod source;
mod source_type;
mod synth;

#[cfg(test)]
mod config_examples_test;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use datalib_dag::subprocess::{
    ENV_CHECKPOINT_CADENCE, ENV_DATA_ROOT, ENV_NOW, ENV_RESET, ENV_STEP,
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
    /// A JSON file holding the step's params — the runner writes the
    /// config entry's `params` there and appends the flag. Phase-specific:
    /// for `ingest` it is the provider's download config subtree, for
    /// `render_markdown` the slim render config (render knobs only);
    /// absent means an empty one. A file, not an argument: params carry
    /// tokens, and argv is readable by every user on the machine.
    #[arg(long = "params-file", global = true)]
    params_file: Option<PathBuf>,
    /// Declared input step ids (JSON string array), appended by the
    /// runner from the config entry's `inputs`. Accepted so every step
    /// command shares one flag surface; the resolved list this binary
    /// acts on is `DATALIB_DAG_INPUTS`.
    #[arg(long, global = true)]
    inputs: Option<String>,
    /// Fixed "now" timestamp (RFC 3339), stamped wherever this step
    /// type records times (raw bookkeeping, `markdowns.rendered_at_utc`).
    /// Falls back to `$DATALIB_DAG_NOW` (the runner exports one
    /// value so the whole run agrees), then the local clock.
    #[arg(long, global = true)]
    now: Option<String>,
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
        /// Source type (`slack`, `claude`, …): the provider to ask.
        source_type: String,
    },
    /// Utility (not a pipeline step): sign in to a service that holds
    /// its own credential rather than a latchkey one, and store it
    /// where that source's ingest step reads it. Interactive.
    Login {
        /// Source type; only `garmin` has a login of its own.
        source_type: String,
        /// Where to write the token files (garmin: `~/.garth`).
        #[arg(long)]
        token_dir: Option<String>,
        /// Account email, else prompted for.
        #[arg(long)]
        email: Option<String>,
        /// `garmin.com`, or `garmin.cn` for a China-region account.
        #[arg(long, default_value = "garmin.com")]
        domain: String,
    },
    /// Utility (not a pipeline step): put qmd's pinned GGUF models in
    /// place, sha256-verified — what the `qmd_index` step does before
    /// it indexes, runnable ahead of time (an image build, a first-run
    /// warmup). Needs no data root.
    PullModels {
        /// Where the models go; default is qmd's own cache,
        /// `$XDG_CACHE_HOME/qmd/models` or `~/.cache/qmd/models`.
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Utility (not a pipeline step): put the Node runtime for qmd and
    /// latchkey in place — the tree beside the binaries when one is
    /// shipped, else the release asset `runtime.manifest` names,
    /// fetched sha256-verified into `~/.cache/datalib/runtime` — and
    /// run `qmd --version` and `latchkey --version` through it. What
    /// every sync does on its first `qmd` or `latchkey`, runnable ahead
    /// of time. Needs no data root.
    PullRuntime,
    /// Dev utility (not a pipeline step): build HTTP playback fixtures
    /// for one source from a raw fixture tree (`--params-file` naming a
    /// `{"fixture_path": …}`), for later replay via `--playback-root`.
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
/// What a step that did not finish reports: the version its store is
/// at. Nothing for a command that is not a step.
async fn published_claims(data_root: &Path) -> Vec<events::OutputClaim> {
    match StepEnv::from_env() {
        Ok(env) => published::claims(&env, data_root).await,
        Err(_) => Vec::new(),
    }
}

fn checkpoint_cadence() -> Option<datalib_etl::checkpointer::Cadence> {
    let raw = std::env::var(ENV_CHECKPOINT_CADENCE).ok()?;
    match datalib_dag::config::CheckpointCadence::decode(&raw) {
        Some(c) => Some(datalib_etl::checkpointer::Cadence {
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

/// How long a step may keep running after SIGINT before it is exited
/// without its final commit. Inside the runner's 15s (`CANCEL_GRACE`),
/// with room for the commit itself.
const INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let _obs_guard = datalib_obs::init(&cli.obs, "datalib-step").ok();
    // Before the first `qmd` or `latchkey` spawn, wherever it comes
    // from: a release tarball without `runtime/` beside its binaries
    // fetches the one its manifest names, once per machine.
    datalib_fetch::enable_runtime_fetch();

    // `probe` is answered before any of the step machinery below: it
    // owns no tree, claims no outputs and must leave stdout holding
    // exactly one JSON object, so an `outcome` event line after it
    // would corrupt the only thing its caller reads.
    if let Some(Cmd::Probe { source_type }) = &cli.cmd {
        probe::run_cli(source_type, cli.params_file.as_deref()).await;
    }
    // `pull-models` likewise: nothing here is a step.
    if let Some(Cmd::PullModels { models_dir }) = &cli.cmd {
        let dir = models_dir
            .clone()
            .unwrap_or_else(datalib_qmd_indexer::default_models_dir);
        // Off the async runtime: the fetch is blocking I/O, and reqwest's
        // blocking client refuses to be dropped on a runtime thread.
        let ensure = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || {
                datalib_qmd_models::ensure_models(
                    &dir,
                    datalib_qmd_models::PINNED_MODELS,
                    datalib_qmd_models::Fetch::from_env(),
                )
            })
            .await
            .expect("pull-models task panicked")
        };
        match ensure {
            Ok(outcomes) => {
                for (model, outcome) in datalib_qmd_models::PINNED_MODELS.iter().zip(&outcomes) {
                    datalib_obs::status_line!(
                        "{:?}: {}",
                        outcome,
                        dir.join(model.cache_name()).display()
                    );
                }
                let all_present = outcomes
                    .iter()
                    .all(|o| *o != datalib_qmd_models::Outcome::Missing);
                std::process::exit(if all_present { 0 } else { 1 });
            }
            Err(e) => {
                datalib_obs::status_line!("error: {e:#}");
                std::process::exit(1);
            }
        }
    }
    // `pull-runtime` likewise. Off the async runtime for the same
    // reason as `pull-models`.
    if let Some(Cmd::PullRuntime) = &cli.cmd {
        let out = tokio::task::spawn_blocking(pull_runtime)
            .await
            .expect("pull-runtime task panicked");
        match out {
            Ok(report) => {
                datalib_obs::status_line!("{report}");
                std::process::exit(0);
            }
            Err(e) => {
                datalib_obs::status_line!("error: {e:#}");
                std::process::exit(1);
            }
        }
    }
    // `login` likewise: it talks to a terminal, not to the runner.
    if let Some(Cmd::Login {
        source_type,
        token_dir,
        email,
        domain,
    }) = &cli.cmd
    {
        login::run_cli(source_type, token_dir.as_deref(), email.as_deref(), domain).await;
    }

    let step_id = std::env::var(ENV_STEP).unwrap_or_else(|_| "step".to_string());
    let data_root = std::env::var_os(ENV_DATA_ROOT)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .expect("no data root: set DATALIB_DAG_DATA_ROOT or run inside one");
    let emitter = Emitter::new(step_id);

    // SIGINT (terminal Ctrl-C, or forwarded by the runner on cancel):
    // raise the stop flag and let the step end at a boundary of its own
    // choosing — a fetch loop stops taking units, the seal path seals at
    // the next consistent point, `finish` commits — then report
    // `cancelled`. Nothing is committed *from here*: a commit made by a
    // signal handler publishes whatever is half-written. A step that has
    // not ended by the grace is exited anyway; the runner kills what is
    // left at `CANCEL_GRACE` (15s), so this stays inside that.
    let stop = datalib_etl::stop::StopFlag::new();
    let sig_stop = stop.clone();
    let sig_emitter = emitter.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            sig_stop.request();
            tracing::info!("interrupted; stopping at the next consistent point");
            tokio::time::sleep(INTERRUPT_GRACE).await;
            tracing::warn!(
                "still running {}s after the interrupt; exiting without a final commit",
                INTERRUPT_GRACE.as_secs()
            );
            sig_emitter.outcome(&[], Some(FailureKind::Cancelled));
            std::process::exit(130);
        }
    });

    // The runner holds the other end of stdin. A runner that dies
    // without running any code of its own — SIGKILL, an abort, the OOM
    // killer — signals nothing, and nothing else ever signals a step, so
    // the step notices the pipe close itself and raises the SIGINT the
    // runner would have sent. Installed after the handler above, which
    // is what then seals and exits.
    if let Err(e) = datalib_parent_watch::exit_with_parent(interrupt_own_group) {
        datalib_obs::status_line!("error: {e}");
        std::process::exit(2);
    }

    let now = cli
        .now
        .clone()
        .or_else(|| std::env::var(ENV_NOW).ok())
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    // Every stamp a step writes is split off this one string, so a
    // shape it cannot split is refused here rather than stored as-is.
    if let Err(e) = datalib_time::validate_iso_offset(&now) {
        datalib_obs::status_line!("--now / ${ENV_NOW} must be RFC 3339 with an offset: {e}");
        std::process::exit(2);
    }
    let control = datalib_etl::control::DownloadControl {
        checkpoint_cadence: checkpoint_cadence(),
        stop: stop.clone(),
    };

    match run(cli, &data_root, &now, &control, &emitter).await {
        // A run that ended because it was asked to is not a success, even
        // though it committed: it did not finish, and saying so is how the
        // runner knows not to mark it done. What it committed stands, and
        // the version it reports is how that reaches its consumers.
        Ok(outputs) if stop.requested() => {
            emitter.outcome(&outputs, Some(FailureKind::Cancelled));
            std::process::exit(130);
        }
        // Likewise an error after the stop: the transport refuses new
        // requests once the flag is up, so a phase that does not read the
        // flag ends with `Interrupted`. That is the stop, not a failure.
        Err(e) if stop.requested() => {
            tracing::info!("stopped: {e:#}");
            emitter.outcome(
                &published_claims(&data_root).await,
                Some(FailureKind::Cancelled),
            );
            std::process::exit(130);
        }
        Ok(outputs) => {
            emitter.outcome(&outputs, None);
            exit_watchdog::arm(exit_watchdog::GRACE);
        }
        Err(e) => {
            let kind = hints::classify(&e);
            // A failed-but-incremental step may still have committed
            // partial output, and reports what it published.
            emitter.outcome(&published_claims(&data_root).await, Some(kind));
            // `tracing::error!` alone, never a `status_line!` beside it.
            // Both land on the same stderr, so a second copy is a second
            // row in the run store -- one with no `target`, because a
            // line that is not a tracing envelope is filed as plain text
            // -- and a second copy of every cause in the step's error
            // message, which the runner builds from those same lines.
            // The fmt layer writes through indicatif, so the progress
            // bars are already suspended across the write.
            for (i, cause) in e.chain().enumerate() {
                let prefix = if i == 0 { "error" } else { "caused by" };
                tracing::error!("{prefix}: {cause}");
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
    let params = source::read_params(cli.params_file.as_deref())?;
    match cli.cmd {
        Some(Cmd::Synthesize {
            source_type,
            name,
            out,
        }) => synth::run(&source_type, &name, &params, data_root, &out, emitter),
        // Handled in `main` before the step machinery starts; see
        // there for why it cannot come through the outcome path.
        Some(Cmd::Probe { .. }) => unreachable!("probe is answered in main"),
        Some(Cmd::Login { .. }) => unreachable!("login is answered in main"),
        Some(Cmd::PullModels { .. }) => unreachable!("pull-models is answered in main"),
        Some(Cmd::PullRuntime) => unreachable!("pull-runtime is answered in main"),
        None => {
            let env = StepEnv::from_env()?;
            if let Ok(part) = std::env::var(ENV_RESET) {
                return reset::run(&env, data_root, &part).await;
            }
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
            let res = ingest::run(&planned, &env.step, now, control, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, planned.source_type, &res);
            res
        }
        Function::RenderMarkdown if env.is_diff_group() => {
            let raw_rel = env.raw_store_rel();
            let (pair, params) = render_diff::split_params(params)?;
            let planned = dispatch::plan(
                env.diff_source_type()?,
                dispatch::Phase::Render,
                &env.group,
                data_root.join(&raw_rel),
                params,
            )?;
            render_diff::run(planned, &env, data_root, now, emitter, control, &pair).await
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
            grid_index::run(data_root, &env, Some(now), emitter).await
        }
        Function::QmdIndex => {
            writes_the_index_tree(&env, &qmd_index::out_rel())?;
            qmd_index::run(data_root, &env, models_dir, emitter).await
        }
        Function::EmbeddingMap => {
            writes_the_index_tree(&env, &embedding_map::out_rel())?;
            embedding_map::run(data_root, now, emitter).await
        }
    }
}

/// Resolve qmd through the runtime resolver — fetching on a miss, now
/// that the fetcher is enabled — and run `--version` through it, so
/// the report names the tree that will serve the next sync and proves
/// its Node starts.
fn pull_runtime() -> Result<String> {
    let qmd = version_through_runtime(datalib_runtime::qmd::qmd_command(
        datalib_runtime::qmd::DEFAULT_QMD_VERSION,
    )?)?;
    let root = datalib_runtime::node_runtime::runtime_root()
        .context("no runtime root after a successful resolution")?;
    let latchkey = version_through_runtime(datalib_runtime::node_runtime::latchkey_command()?)?;
    Ok(format!(
        "runtime: {}\nqmd --version: {qmd}\nlatchkey --version: {latchkey}",
        root.display()
    ))
}

// Both tools, because a tree can hold one and not the other, and
// because this is what the .app's post-bundle check runs
// (`datalib/tauri/check-app.sh`).
fn version_through_runtime(mut cmd: std::process::Command) -> Result<String> {
    let out = cmd
        .arg("--version")
        .output()
        .with_context(|| datalib_runtime::node_runtime::display_command(&cmd))?;
    anyhow::ensure!(
        out.status.success(),
        "`{}` failed ({}): {}",
        datalib_runtime::node_runtime::display_command(&cmd),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The index steps have one reader each — the `unified_index` applet
/// — which finds them from the data root alone, so their trees are
/// fixed. A config that files them under another group would have
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

/// SIGINT this process' own group — itself and whatever it spawned,
/// because the runner starts every step as its own group leader, so this
/// reaches a `node qmd embed` the step would otherwise leave behind.
fn interrupt_own_group() {
    // `report`, not `eprintln!`: by now stderr is a pipe to the dead
    // runner, and `eprintln!` panics on the failed write — which would
    // leave this process running, one line from the exit that ends it.
    //
    // Safety: plain getpgrp/getpid/kill(2). Group 0 is the caller's own
    // group; racing a member that has already exited is benign (ESRCH).
    let group_leader = unsafe { libc::getpgrp() == libc::getpid() };
    if !group_leader {
        // Only the runner sets the variable that got us here, and it
        // makes every step a leader. Signalling group 0 from anywhere
        // else would reach an unrelated group — a terminal's, say.
        datalib_parent_watch::report("the runner is gone; exiting");
        std::process::exit(130);
    }
    datalib_parent_watch::report("the runner is gone; stopping this step and what it spawned");
    unsafe { libc::kill(0, libc::SIGINT) };
}
