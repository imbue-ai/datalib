//! `grid-rows-load` — provider-agnostic Load step. Walks
//! `<out>/rendered_md/**/*.grid_rows.json` (written by any Translate
//! step) and inserts rows into Dolt.
//!
//! Incremental: a `documents_loaded(qmd_path PK, source_fingerprint)`
//! table tracks which documents have already been ingested. Sidecars
//! whose fingerprint matches the recorded one are skipped — zero
//! writes, zero DOLT_COMMITs.
//!
//! Connects to a running `dolt sql-server` if one is already listening
//! on `--dolt-host:--dolt-port`; otherwise spawns one under
//! `--dolt-repo-dir` (default `<out>/dolt_repo`). Same connect-or-spawn
//! semantics as the Python `DoltService`.
//!
//! The output schema is stable across providers so a web UI can
//! consume the `LoadSummary` JSON without per-provider branches.
//!
//! ```sh
//! grid-rows-load --out ~/mirror
//! grid-rows-load --out ~/mirror --otlp-endpoint http://localhost:4317
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_providers::grid_rows_load::{init_schema, load_all};
use frankweiler_providers::obs::{init as init_obs, ObsArgs};
use sqlx::mysql::MySqlPoolOptions;
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

#[derive(Parser, Debug)]
#[command(
    name = "grid-rows-load",
    about = "Load `*.grid_rows.json` sidecars (any provider) into Dolt."
)]
struct Args {
    /// Input root. The loader reads
    /// `<out>/rendered_md/**/*.grid_rows.json` across every provider
    /// subtree.
    #[arg(long, env = "FW_OUT")]
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
    let _guard = init_obs(&args.obs, "grid-rows-load")?;

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
        event = "grid_rows_load_start",
        out = %args.out.display(),
        dolt_repo = %dolt_repo_dir.display(),
    );
    let server = DoltServer::ensure(&dolt_repo_dir, &dolt_cfg).context("ensure dolt sql-server")?;
    info!(
        event = "grid_rows_load_dolt_ready",
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
        "grid_rows_load",
        documents_total = tracing::field::Empty,
        documents_loaded = tracing::field::Empty,
        documents_skipped = tracing::field::Empty,
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
        event = "grid_rows_load_complete",
        documents_total = summary.documents_total,
        documents_loaded = summary.documents_loaded,
        documents_skipped = summary.documents_skipped,
        rows_inserted = summary.rows_inserted,
    );
    Ok(())
}
