//! `anthropic-download` — mirror claude.ai conversations in the
//! Anthropic-export shape so the existing translator works against
//! either source indistinguishably.
//!
//! Requires `latchkey` (with the `claude-ai` service registered) and
//! a Cloudflare-clearing curl impersonator on `LATCHKEY_CURL`. See
//! `EXTRACT.md` in this crate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_anthropic::extract::{
    self as anthropic, FetchOptions, DEFAULT_OVERLAP, SLEEP_BETWEEN,
};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "anthropic-download",
    about = "Mirror claude.ai conversations in the Anthropic-export shape."
)]
struct Args {
    /// Output directory. Created if missing.
    #[arg(long, env = "ANTHROPIC_OUT")]
    out: PathBuf,

    /// Optional bulk-export dir to seed listing/overlap and copy
    /// `users.json` from. The export format is deprecated upstream but
    /// existing local exports still work as a seed.
    #[arg(long)]
    export_dir: Option<PathBuf>,

    /// N most-recently-updated export conversations to refetch from the
    /// API as overlap (sanity-check the live API vs. export).
    #[arg(long, default_value_t = DEFAULT_OVERLAP)]
    overlap: usize,

    /// Seconds between successful conversation fetches.
    #[arg(long, default_value_t = SLEEP_BETWEEN.as_secs_f64())]
    sleep_between: f64,

    /// Only sync conversations whose `updated_at` is at or after this
    /// instant (RFC 3339 or YYYY-MM-DD, assumed UTC). Older
    /// conversations are never detail-fetched.
    #[arg(long)]
    since: Option<String>,

    /// Fetch only these conversation UUIDs instead of walking the full
    /// listing. Pass `--conv-uuid` once per target. Tries each org until
    /// one returns 200; 403/404 are treated as "wrong org, continue".
    /// Merges results into the existing `conversations.json` rather
    /// than replacing it.
    #[arg(long = "conv-uuid", value_name = "UUID")]
    conv_uuids: Vec<String>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "anthropic-download")?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        export_dir: args.export_dir.clone(),
        overlap: args.overlap,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        since: args.since.clone(),
        conv_uuids: args.conv_uuids.clone(),
        ..Default::default()
    };

    let span = info_span!("anthropic_download", out = %args.out.display());
    let summary = anthropic::fetch(opts).instrument(span).await?;
    info!(
        event = "anthropic_download_complete",
        total = summary.total,
        fetched = summary.fetched,
        skipped = summary.skipped,
        out_of_scope = summary.out_of_scope,
        forbidden_orgs = summary.forbidden_orgs,
        errors = summary.errors,
        requests = summary.requests,
        network_seconds = summary.network_seconds,
    );
    Ok(())
}
