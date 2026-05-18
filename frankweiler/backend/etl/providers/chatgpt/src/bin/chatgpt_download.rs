//! `chatgpt-download` — incrementally mirror chatgpt.com conversations
//! into `<out>/{me.json, conversations.json, conversations/<id>.json}`.
//!
//! Requires `latchkey` (with the `chatgpt` service registered) and a
//! Cloudflare-clearing curl impersonator on `LATCHKEY_CURL`. See
//! `EXTRACT.md` in this crate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_chatgpt::extract::{self as chatgpt, FetchOptions, SLEEP_BETWEEN};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "chatgpt-download",
    about = "Mirror chatgpt.com conversations to a local JSON cache."
)]
struct Args {
    /// Output directory. Created if missing.
    #[arg(long, env = "CHATGPT_OUT")]
    out: PathBuf,

    /// Cap the paginated listing walk (debugging).
    #[arg(long)]
    max_pages: Option<usize>,

    /// Stop after N successful conversation fetches (skipped/cached
    /// items don't count). Errors do count.
    #[arg(long)]
    limit: Option<usize>,

    /// Seconds between successful fetches. 0 disables.
    #[arg(long, default_value_t = SLEEP_BETWEEN.as_secs_f64())]
    sleep_between: f64,

    /// Fetch a single conversation by id, skipping the listing walk.
    /// Result lands at `<out>/conversations/<id>.json`.
    #[arg(long, value_name = "ID")]
    conv_uuid: Option<String>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "chatgpt-download")?;

    let opts = FetchOptions {
        out_dir: args.out.clone(),
        max_pages: args.max_pages,
        limit: args.limit,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        conv_uuid: args.conv_uuid.clone(),
    };

    let span = info_span!("chatgpt_download", out = %args.out.display());
    let summary = chatgpt::fetch(opts).instrument(span).await?;
    info!(
        event = "chatgpt_download_complete",
        listing = summary.listing,
        fetched = summary.fetched,
        skipped = summary.skipped,
        errors = summary.errors,
        requests = summary.requests,
        network_seconds = summary.network_seconds,
    );
    Ok(())
}
