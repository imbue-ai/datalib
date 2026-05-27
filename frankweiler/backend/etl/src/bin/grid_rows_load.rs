//! `grid-rows-load` — provider-agnostic Load step. Walks
//! `<out>/rendered_md/**/*.grid_rows.json` (written by any Translate
//! step) and inserts rows into the doltlite file at `<out>/<db_filename>`.
//!
//! Incremental: a `documents_loaded(qmd_path PK, source_fingerprint)`
//! table tracks which documents have already been ingested. Sidecars
//! whose fingerprint matches the recorded one are skipped — zero
//! writes.
//!
//! ```sh
//! grid-rows-load --out ~/mirror
//! grid-rows-load --out ~/mirror --otlp-endpoint http://localhost:4317
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl::load::{init_schema, load_all};
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_qmd_indexer::{run_index, IndexOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

#[derive(Parser, Debug)]
#[command(
    name = "grid-rows-load",
    about = "Load `*.grid_rows.json` sidecars (any provider) into the doltlite file."
)]
struct Args {
    /// Input root. The loader reads
    /// `<out>/rendered_md/**/*.grid_rows.json` across every provider
    /// subtree.
    #[arg(long, env = "FW_OUT")]
    out: PathBuf,

    /// Path to the doltlite database file. Defaults to `<out>/mirror.db`.
    #[arg(long, env = "DOLT_DB_PATH")]
    dolt_db_path: Option<PathBuf>,

    /// After loading, run the qmd indexer over `<out>/rendered_md/`. qmd
    /// update is incremental — repeated invocations only re-index changed
    /// `.md` files.
    #[arg(long, env = "FW_QMD_INDEX")]
    qmd_index: bool,

    /// Skip the embedding pass when running the qmd indexer. Useful for
    /// CI / smoke tests where the ~300MB model download isn't desired.
    #[arg(long, env = "FW_QMD_NO_EMBED")]
    qmd_no_embed: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "grid-rows-load")?;

    let db_path = args
        .dolt_db_path
        .clone()
        .unwrap_or_else(|| args.out.join("mirror.db"));
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    info!(
        event = "grid_rows_load_start",
        out = %args.out.display(),
        db = %db_path.display(),
    );

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .context("open doltlite file")?;
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
    let summary = load_all(
        &pool,
        &args.out,
        |msg| {
            let _ = done.fetch_add(1, Ordering::Relaxed);
            tracing::Span::current().pb_set_message(msg);
        },
        None,
    )
    .await?;

    info!(
        event = "grid_rows_load_complete",
        documents_total = summary.documents_total,
        documents_loaded = summary.documents_loaded,
        documents_skipped = summary.documents_skipped,
        rows_inserted = summary.rows_inserted,
    );

    drop(_enter);

    if args.qmd_index {
        let mut opts = IndexOptions::new(args.out.clone());
        opts.embed = !args.qmd_no_embed;
        info!(event = "qmd_index_start", root = %args.out.display(), embed = opts.embed);
        let index_path = tokio::task::spawn_blocking(move || run_index(&opts))
            .await
            .context("qmd-indexer task panicked")?
            .context("qmd-indexer failed")?;
        info!(event = "qmd_index_complete", index = %index_path.display());
    }
    Ok(())
}
