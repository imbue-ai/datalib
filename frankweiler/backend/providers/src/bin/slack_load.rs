//! `slack-load` — read the `.md` + `.grid_rows.json` tree written by
//! `slack-render` and load grid_rows into Dolt.
//!
//! Incremental: a `documents_loaded(qmd_path PK, source_fingerprint)`
//! table tracks which threads have already been ingested. Threads whose
//! sidecar fingerprint matches the recorded one are skipped (zero
//! writes, zero DOLT_COMMITs).
//!
//! Connects to a running `dolt sql-server` if one is already listening
//! on `--dolt-host:--dolt-port`; otherwise spawns one under
//! `--dolt-repo-dir` (default `<out>/dolt_repo`). Same connect-or-spawn
//! semantics as the Python `DoltService`.
//!
//! ```sh
//! slack-load --out ~/slack-mirror
//! slack-load --out ~/slack-mirror --otlp-endpoint http://localhost:4317
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_providers::obs::{init as init_obs, ObsArgs};
use frankweiler_providers::slack::load::{init_schema, load_all};
use sqlx::mysql::MySqlPoolOptions;
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

#[derive(Parser, Debug)]
#[command(
    name = "slack-load",
    about = "Load rendered Slack grid_rows sidecars into Dolt."
)]
struct Args {
    /// Input root (same value passed to `slack-render --out`). The
    /// loader reads `<out>/rendered_md/slack/**/*.grid_rows.json`.
    #[arg(long, env = "SLACK_OUT")]
    out: PathBuf,

    /// Dolt repo directory. Defaults to `<out>/dolt_repo`. If a
    /// `dolt sql-server` is already running on `--dolt-host:--dolt-port`,
    /// the loader attaches to it and ignores this path for spawning.
    #[arg(long, env = "DOLT_REPO_DIR")]
    dolt_repo_dir: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1", env = "DOLT_HOST")]
    dolt_host: String,
    #[arg(long, default_value_t = 3306, env = "DOLT_PORT")]
    dolt_port: u16,
    #[arg(long, default_value = "root", env = "DOLT_USER")]
    dolt_user: String,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "slack-load")?;

    let dolt_repo_dir = args
        .dolt_repo_dir
        .clone()
        .unwrap_or_else(|| args.out.join("dolt_repo"));

    let dolt_cfg = DoltConfig {
        host: args.dolt_host.clone(),
        port: args.dolt_port,
        user: args.dolt_user.clone(),
        repo_dirname: dolt_repo_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "dolt_repo".into()),
        binary: None,
    };

    info!(
        event = "slack_load_start",
        out = %args.out.display(),
        dolt_repo = %dolt_repo_dir.display(),
    );
    let server = DoltServer::ensure(&dolt_repo_dir, &dolt_cfg).context("ensure dolt sql-server")?;
    info!(
        event = "slack_load_dolt_ready",
        url = server.mysql_url(),
        owned = server.owns_server(),
    );

    let pool = MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&server.mysql_url())
        .await
        .context("connect to dolt")?;
    init_schema(&pool).await?;

    let span = info_span!(
        "slack_load",
        threads_total = tracing::field::Empty,
        threads_loaded = tracing::field::Empty,
        threads_skipped = tracing::field::Empty,
        indicatif.pb_show = tracing::field::Empty,
    );
    let _enter = span.enter();

    let done = AtomicUsize::new(0);
    let summary = load_all(&pool, &args.out, |msg| {
        let _ = done.fetch_add(1, Ordering::Relaxed);
        tracing::Span::current().pb_set_message(msg);
    })
    .await?;

    info!(
        event = "slack_load_complete",
        threads_total = summary.threads_total,
        threads_loaded = summary.threads_loaded,
        threads_skipped = summary.threads_skipped,
        rows_inserted = summary.rows_inserted,
    );
    Ok(())
}
