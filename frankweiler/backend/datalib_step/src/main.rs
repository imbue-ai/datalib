//! `datalib-step` — the step-type host binary for the DAG runner.
//!
//! Each subcommand is one step type under the DAG step contract (see
//! `frankweiler_dag`): it reads artifacts under the data root, writes
//! its declared outputs, streams NDJSON progress events on stdout,
//! and finishes with one `{"event":"outcome",…}` line reporting
//! per-output change status. `datalib-dag` (the runner) spawns this
//! binary for every `step:`-typed config entry.
//!
//! Step types (the DAG config writes them as `<type>.<phase>`, e.g.
//! `slack_api.download`; the runner maps that onto the subcommand +
//! `--type` pair):
//!
//! * `download --type <source_type> --params-json {name, source}` —
//!   one source's extract wave, via the provider's own
//!   `DataProcessor`s. Writes `<name>/raw`. `source` is the
//!   provider's own config subtree (no `type:` tag — the step type
//!   names the provider).
//! * `render --type <source_type> --params-json {name, source}` —
//!   the source's translate wave. Writes `<name>/rendered_md` (`.md`
//!   files plus the `.grid_rows.json` sidecars the providers already
//!   emit). Incremental: docs whose sidecar fingerprint is unchanged
//!   are skipped, using the sidecar tree itself as the
//!   prior-fingerprint store (no index-DB peeking — that's the
//!   un-fused contract).
//! * `index` — rebuild/refresh `system/backend_index` from every
//!   stanza's sidecar tree (`load_all`, per-doc fingerprint skip),
//!   then `dolt_commit`. This is the load step un-fused from render.
//! * `qmd` — the qmd search index over every rendered_md tree,
//!   writing `system/qmd`.
//! * `synthesize` — dev utility, not a pipeline step: build HTTP
//!   playback fixtures for one source from its `input_path` raw
//!   fixture tree (the `--synthesize-playback-root` mode of the old
//!   sync binary, one source per invocation).
//!
//! Identity comes from the runner via `FRANKWEILER_DAG_STEP` /
//! `FRANKWEILER_DAG_DATA_ROOT` (falling back to the CWD, which the
//! runner also sets to the data root). Tracing goes to stderr;
//! stdout carries only the event stream.

mod dispatch;
mod download;
mod events;
mod hints;
mod index_step;
mod qmd_step;
mod render;
mod source;
mod synth;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use frankweiler_dag::subprocess::{ENV_DATA_ROOT, ENV_STEP};

use crate::events::Emitter;

#[derive(Parser)]
#[command(
    name = "datalib-step",
    about = "Step-type host for the datalib DAG runner"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Fixed "now" timestamp (RFC 3339), stamped wherever this step
    /// type records times (raw bookkeeping, `markdowns.rendered_at`).
    /// The runner passes one value to every step so the whole run
    /// agrees; standalone invocations default to the local clock.
    #[arg(long, global = true)]
    now: Option<String>,
    /// Download only: wipe every entity table (and its bookkeeping
    /// sidecar) before fetching, re-downloading every entity row.
    /// `blob_refs` is preserved — see `--refetch-blobs`.
    #[arg(long, global = true)]
    reset_and_redownload: bool,
    /// Download only: wipe `blob_refs` so every attachment re-fetches
    /// on the wire (the CAS itself is never truncated).
    #[arg(long, global = true)]
    refetch_blobs: bool,
    #[command(flatten)]
    obs: frankweiler_obs::ObsArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// One source's extract wave → `<name>/raw`.
    Download {
        /// Source type (`slack_api`, `claude_api`, …) — the provider
        /// this step dispatches to.
        #[arg(long = "type")]
        source_type: String,
        /// JSON params: {"name": …, "source": {…}} — `source` is the
        /// provider's own config subtree (no `type:` tag).
        #[arg(long)]
        params_json: String,
        /// HTTP playback fixture tree (hermetic runs); sets
        /// `FRANKWEILER_HTTP_PLAYBACK` for every provider transport.
        #[arg(long)]
        playback_root: Option<PathBuf>,
    },
    /// One source's translate wave → `<name>/rendered_md`.
    Render {
        #[arg(long = "type")]
        source_type: String,
        #[arg(long)]
        params_json: String,
    },
    /// Rebuild `system/backend_index` from every sidecar tree.
    Index,
    /// Build the qmd search index → `system/qmd`.
    Qmd {
        /// Directory where qmd caches its embedding model.
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Dev utility (not a pipeline step): build HTTP playback fixtures
    /// for one source from its `input_path` raw fixture tree, for
    /// later replay via `download --playback-root`.
    Synthesize {
        #[arg(long = "type")]
        source_type: String,
        #[arg(long)]
        params_json: String,
        /// Output directory for the playback fixture tree.
        #[arg(long)]
        out: PathBuf,
    },
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
        .unwrap_or_else(|| frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    let control = frankweiler_etl::control::ExtractControl {
        reset_and_redownload: cli.reset_and_redownload,
        refetch_blobs: cli.refetch_blobs,
    };

    match run(cli.cmd, &data_root, &now, &control, &emitter).await {
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

async fn run(
    cmd: Cmd,
    data_root: &PathBuf,
    now: &str,
    control: &frankweiler_etl::control::ExtractControl,
    emitter: &Emitter,
) -> Result<Vec<events::OutputClaim>> {
    match cmd {
        Cmd::Download {
            source_type,
            params_json,
            playback_root,
        } => {
            if let Some(pb) = playback_root {
                let pb = pb.canonicalize().context("playback root")?;
                std::env::set_var(frankweiler_etl::http::PLAYBACK_ENV, pb);
            }
            let p = source::parse_params(&params_json)?;
            let planned = dispatch::plan(
                &source_type,
                dispatch::Phase::Download,
                &p.name,
                p.source,
                data_root,
            )?;
            let res = download::run(&planned, data_root, now, control, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, planned.type_str, &res);
            res
        }
        Cmd::Render {
            source_type,
            params_json,
        } => {
            let p = source::parse_params(&params_json)?;
            let planned = dispatch::plan(
                &source_type,
                dispatch::Phase::Render,
                &p.name,
                p.source,
                data_root,
            )?;
            let type_str = planned.type_str;
            let res = render::run(planned, data_root, emitter).await;
            hints::emit_auth_hint_on_failure(emitter, type_str, &res);
            res
        }
        Cmd::Index => index_step::run(data_root, Some(now), emitter).await,
        Cmd::Qmd { models_dir } => qmd_step::run(data_root, models_dir, emitter).await,
        Cmd::Synthesize {
            source_type,
            params_json,
            out,
        } => {
            let p = source::parse_params(&params_json)?;
            synth::run(&source_type, &p.name, &p.source, data_root, &out, emitter)
        }
    }
}
