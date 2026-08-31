//! `slack-download` — drives [`datalib_etl_slack::download::fetch`] from
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
//! bazelisk run //datalib/backend/etl/providers/slack:slack_download -- \
//!     --out ~/backups/slack \
//!     --channel imbue-announce --channel chat-thad --channel chat-glenn
//! ```

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use datalib_etl_slack::download::{
    self as slack, FetchOptions, DEFAULT_REFRESH_WINDOW_DAYS, DEFAULT_SINCE,
};
use datalib_obs::{init as init_obs, ObsArgs};
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

    /// Also mirror direct messages — 1:1 DMs and group DMs. Off unless
    /// asked for: DMs are the most sensitive thing in a workspace.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    dms: bool,

    /// Restrict `--dms` to conversations with these people. Repeat the
    /// flag. Each value is a Slack user id or any of that user's names
    /// (handle, display name, real name), with an optional leading `@`.
    /// Group DMs are skipped while this is set — see the provider's
    /// DOWNLOAD.md.
    #[arg(long = "dm-user", value_name = "PERSON")]
    dm_users: Vec<String>,

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

    // Same rule the config path enforces in `SlackApiSync::validate`:
    // naming people to mirror DMs with, while DMs are off, would either
    // silently mirror nothing or silently turn the feature on.
    if !args.dms && !args.dm_users.is_empty() {
        anyhow::bail!(
            "--dm-user was given {} time(s) but --dms is false, so no direct messages \
             would be mirrored. Pass --dms true, or drop --dm-user.",
            args.dm_users.len(),
        );
    }

    let opts = FetchOptions {
        db_path: args.out.clone(),
        channels,
        since: args.since.clone(),
        refresh_window_days: args.refresh_window_days,
        members_only: args.members_only,
        media: args.media,
        dms: args.dms,
        dm_users: (!args.dm_users.is_empty()).then(|| args.dm_users.clone()),
        ..Default::default()
    };

    // Root span: every downstream span hangs off this, and OTLP gets a
    // single trace per CLI invocation.
    let span = info_span!(
        "slack_download",
        out = %args.out.display(),
        channels = ?opts.channels,
        media = opts.media,
        dms = opts.dms,
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
