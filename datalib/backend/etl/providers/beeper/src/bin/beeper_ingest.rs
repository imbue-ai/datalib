//! `beeper-ingest` — drives [`datalib_etl_beeper::ingest::fetch`]
//! from the command line.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use datalib_etl_beeper::ingest::{self as beeper, db_path_for, FetchOptions, RawDb};
use datalib_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "beeper-ingest",
    about = "Mirror Beeper Texts' on-disk data for selected chat networks into a doltlite database."
)]
struct Args {
    /// Output doltlite path. The entity db lives inside the per-source
    /// directory as `entities.doltlite_db` (the dir is created if needed).
    #[arg(long, env = "BEEPER_OUT")]
    out: PathBuf,

    /// Canonical chat network names to ingest. Repeat for multiple.
    /// Currently supported: `signal`, `googlechat`. Other values
    /// (`slack`, `whatsapp`, `telegram`, …) will compile but are
    /// untested — open an issue with a sample if one of them gives
    /// you trouble.
    #[arg(long = "source", value_name = "NETWORK", required = true)]
    sources: Vec<String>,

    /// Override the Beeper Texts data directory. Defaults to
    /// `~/Library/Application Support/BeeperTexts` on macOS.
    #[arg(long, env = "BEEPER_DATA_DIR")]
    beeper_data_dir: Option<PathBuf>,

    /// Copy cached media bytes into the `blobs` table. Off = metadata
    /// + source URL only.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    media: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "beeper-ingest")?;

    // The one writer: this process opens the store, hands the handle to
    // `fetch`, and closes it below.
    let db = RawDb::open(&db_path_for(&args.out)).await?;
    let opts = FetchOptions {
        sources: args.sources.clone(),
        beeper_data_dir: args.beeper_data_dir.clone(),
        media: args.media,
        ..FetchOptions::new(db.clone())
    };

    let span = info_span!(
        "beeper_ingest",
        out = %args.out.display(),
        sources = ?opts.sources,
        media = opts.media,
    );
    let summary = beeper::fetch(opts).instrument(span).await;
    db.close().await;
    let summary = summary?;

    info!(
        event = "beeper_download_complete",
        rooms = summary.rooms,
        users = summary.users,
        events = summary.events,
        blobs = summary.blobs,
        blob_errors = summary.blob_errors,
        events_enriched = summary.events_enriched,
        events_orphaned = summary.events_orphaned,
    );
    Ok(())
}
