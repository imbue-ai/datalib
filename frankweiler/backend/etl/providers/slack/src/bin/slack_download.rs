//! `slack-download` — drives [`frankweiler_etl_slack::extract::fetch`] from
//! the command line with structured tracing.
//!
//! On a TTY this renders progress bars (one per channel) plus pretty
//! event lines on stderr. When stderr is piped, it switches to NDJSON so
//! a pipeline orchestrator can scrape structured events without parsing
//! ANSI. Adding `--otlp-endpoint http://collector:4317` *also* exports
//! spans + events to OTLP for centralized monitoring.
//!
//! ```sh
//! slack-download --out ~/slack-mirror --channels thad-testing-channel
//! slack-download --out ~/slack-mirror --since 2025-01-01 --no-media \
//!     --otlp-endpoint http://localhost:4317
//! ```
//!
//! Manual live test via Bazel (talks to the real Slack workspace — needs
//! `latchkey` creds on the host):
//!
//! ```sh
//! bazelisk run //frankweiler/backend/etl/providers/slack:slack_download -- \
//!     --out ~/backups/slack \
//!     --channel imbue-announce --channel chat-thad --channel chat-glenn
//! ```

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_slack::extract::{
    self as slack, FetchOptions, DEFAULT_REFRESH_WINDOW_DAYS, DEFAULT_SINCE,
};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "slack-download",
    about = "Mirror a Slack workspace into JSONL event streams."
)]
struct Args {
    /// Output directory. Created if missing. Per-entity JSONL files
    /// land under `<out>/<entity>/{created,updated}/events.jsonl`.
    #[arg(long, env = "SLACK_OUT")]
    out: PathBuf,

    /// Channel names to mirror (without `#`). Repeat the flag for
    /// multiple. Omit to fan out across every channel the token can see.
    #[arg(long = "channel", value_name = "NAME")]
    channels: Vec<String>,

    /// ISO date or RFC3339 timestamp. Earliest message to fetch on the
    /// first pass; later runs pick up where the prior run left off.
    #[arg(long, default_value = DEFAULT_SINCE)]
    since: String,

    /// On each run, also re-fetch the trailing N days to pick up edits
    /// and reactions that landed on previously-stored messages. Set to
    /// 0 to skip the refresh pass.
    #[arg(long, default_value_t = DEFAULT_REFRESH_WINDOW_DAYS)]
    refresh_window_days: i64,

    /// Skip channels the bot/user isn't a member of. The Slack API
    /// returns them in `conversations.list` either way; this filter
    /// avoids hammering them.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    members_only: bool,

    /// Download file uploads inline. Off = JSON metadata only.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    media: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "slack-download")?;

    let channels = if args.channels.is_empty() {
        None
    } else {
        Some(args.channels.clone())
    };

    let opts = FetchOptions {
        out_dir: args.out.clone(),
        channels,
        since: args.since.clone(),
        refresh_window_days: args.refresh_window_days,
        members_only: args.members_only,
        media: args.media,
    };

    // Root span: every downstream span hangs off this, and OTLP gets a
    // single trace per CLI invocation.
    let span = info_span!(
        "slack_download",
        out = %args.out.display(),
        channels = ?opts.channels,
        media = opts.media,
    );
    let summary = slack::fetch(opts).instrument(span).await?;

    info!(
        event = "slack_download_complete",
        messages = summary.messages,
        replies = summary.replies,
        media_downloaded = summary.media.get("downloaded").copied().unwrap_or(0),
        media_skipped = summary.media.get("skipped").copied().unwrap_or(0),
        media_errors = summary.media.get("error").copied().unwrap_or(0),
    );
    Ok(())
}
