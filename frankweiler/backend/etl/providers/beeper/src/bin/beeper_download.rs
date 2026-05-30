//! `beeper-download` — drives [`frankweiler_etl_beeper::extract::fetch`]
//! from the command line.
//!
//! Reads Beeper Texts' on-disk SQLite stores under
//! `~/Library/Application Support/BeeperTexts/` and copies the rows
//! for the configured chat networks into a doltlite database. No
//! network, no auth.
//!
//! ```sh
//! beeper-download --out ~/beeper-mirror.doltlite_db \
//!     --source signal --source googlechat
//! ```

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_beeper::extract::{self as beeper, FetchOptions};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "beeper-download",
    about = "Mirror Beeper Texts' on-disk data for selected chat networks into a doltlite database."
)]
struct Args {
    /// Output doltlite path. A legacy directory path is rewritten to
    /// `<dir>.doltlite_db`.
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
    let _guard = init_obs(&args.obs, "beeper-download")?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        sources: args.sources.clone(),
        beeper_data_dir: args.beeper_data_dir.clone(),
        media: args.media,
        ..Default::default()
    };

    let span = info_span!(
        "beeper_download",
        out = %args.out.display(),
        sources = ?opts.sources,
        media = opts.media,
    );
    let summary = beeper::fetch(opts).instrument(span).await?;

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
