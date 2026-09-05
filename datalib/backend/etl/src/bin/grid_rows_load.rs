//! `grid-rows-load` — provider-agnostic Load step. Stacks every
//! source's render store
//! (`<out>/<stanza>/rendered_md/indexed_markdown.doltlite_db`, written
//! by that source's render step) into the doltlite file at
//! `<out>/unified_index/grid/db.doltlite_db`.
//!
//! Incremental twice over: the index remembers, per source, the store
//! commit it last consumed (`source_cursors`) and asks that store's
//! `dolt_diff` what moved since; a document that does surface is still
//! skipped if its `source_fingerprint` already matches what the index
//! holds.
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
use datalib_etl::grid_index::{build_grid_index, init_schema};
use datalib_obs::{init as init_obs, ObsArgs};
use datalib_qmd_indexer::{run_index, IndexOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tracing::{debug, info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "grid-rows-load",
    about = "Stack every source's render store into the unified doltlite index."
)]
struct Args {
    /// Input root. The loader reads each stanza's render store at
    /// `<out>/<stanza>/rendered_md/indexed_markdown.doltlite_db`.
    #[arg(long, env = "FW_OUT")]
    out: PathBuf,

    /// Path to the doltlite database file. Defaults to
    /// `<out>/unified_index/grid/db.doltlite_db`.
    #[arg(long, env = "DOLT_DB_PATH")]
    dolt_db_path: Option<PathBuf>,

    /// After loading, run the qmd indexer over `<out>`. qmd
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
        .unwrap_or_else(|| datalib_core::layout::grid_index_db(&args.out));
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
    // Pool size 1: doltlite's per-connection HEAD pointer means
    // pool sizes >1 produce silent dolt_log dropouts and
    // `commit conflict` errors on interleaved writes. See
    // `datalib_etl::doltlite_raw` module docs.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .context("open doltlite file")?;
    init_schema(&pool).await?;

    let span = info_span!(
        "grid_rows_load",
        markdowns_total = tracing::field::Empty,
        markdowns_loaded = tracing::field::Empty,
        markdowns_skipped = tracing::field::Empty,
    );
    let _enter = span.enter();

    let done = AtomicUsize::new(0);
    let summary = build_grid_index(
        &pool,
        &args.out,
        |msg| {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            debug!(event = "grid_rows_load_progress", count = n, message = msg);
        },
        None,
    )
    .await?;

    info!(
        event = "grid_rows_load_complete",
        markdowns_total = summary.markdowns_total,
        markdowns_loaded = summary.markdowns_loaded,
        markdowns_skipped = summary.markdowns_skipped,
        rows_inserted = summary.rows_inserted,
    );

    drop(_enter);

    if args.qmd_index {
        let mut opts = IndexOptions::new(args.out.clone());
        opts.embed = !args.qmd_no_embed;
        info!(event = "qmd_index_start", root = %args.out.display(), embed = opts.embed);
        let outcome = tokio::task::spawn_blocking(move || run_index(&opts))
            .await
            .context("qmd-indexer task panicked")?
            .context("qmd-indexer failed")?;
        info!(event = "qmd_index_complete", index = %outcome.index_path.display());
    }
    Ok(())
}
