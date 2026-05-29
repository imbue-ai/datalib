//! `beeper-download` — drives [`frankweiler_etl_beeper::extract::fetch`]
//! from the command line.
//!
//! ```sh
//! beeper-download --out ~/beeper-mirror --networks imessage
//! ```
//!
//! Requires `latchkey auth set beeper` to have run beforehand with a
//! valid access token for `matrix.beeper.com`. See
//! `frankweiler/backend/etl/providers/beeper/EXTRACT.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_beeper::extract::{self as beeper, FetchOptions, DEFAULT_REFRESH_WINDOW_DAYS};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "beeper-download",
    about = "Mirror a Beeper account into a doltlite raw store via the Matrix Client-Server API."
)]
struct Args {
    /// Output doltlite path. If a legacy directory is passed, it's
    /// rewritten to `<dir>.doltlite_db`.
    #[arg(long, env = "BEEPER_OUT")]
    out: PathBuf,

    /// Bridge networks to mirror (e.g. `imessage`, `signal`). Repeat
    /// for multiple. Omit to fan out across every bridge the account
    /// is connected to.
    #[arg(long = "network", value_name = "NAME")]
    networks: Vec<String>,

    /// Specific Matrix room IDs to mirror (or paste-able matrix.to
    /// URLs). When non-empty, `--network` is ignored.
    #[arg(long = "room", value_name = "ROOM_ID")]
    rooms: Vec<String>,

    /// Trailing N days to re-walk on each run. Milestone A doesn't
    /// paginate `/messages` yet; this is kept for forward
    /// compatibility.
    #[arg(long, default_value_t = DEFAULT_REFRESH_WINDOW_DAYS)]
    refresh_window_days: i64,

    /// Download media (`mxc://`). Milestone A: ignored.
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
        networks: args.networks.clone(),
        rooms: args.rooms.clone(),
        refresh_window_days: args.refresh_window_days,
        media: args.media,
        ..Default::default()
    };

    let span = info_span!(
        "beeper_download",
        out = %args.out.display(),
        networks = ?opts.networks,
        rooms = ?opts.rooms,
    );
    let summary = beeper::fetch(opts).instrument(span).await?;

    info!(
        event = "beeper_download_complete",
        rooms = summary.rooms,
        users = summary.users,
        events = summary.events,
        requests = summary.requests,
    );
    Ok(())
}
