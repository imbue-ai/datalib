//! `github-download` — mirror PRs the user authored / commented on /
//! was @mentioned in, plus all their comments and reviews. Output is
//! event-store JSONL under `<out>/<entity>/{created,updated}/events.jsonl`.
//!
//! Requires `latchkey` with a `github` service registered (Bearer token).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_github::extract::{self as github, parse_pr_ref, FetchOptions, DEFAULT_SCOPES};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "github-download",
    about = "Mirror GitHub PRs + comments + reviews via the REST API."
)]
struct Args {
    /// Output directory. Created if missing.
    #[arg(long, env = "GITHUB_OUT")]
    out: PathBuf,

    /// Discovery scope, repeatable. Default: author:@me commenter:@me mentions:@me.
    #[arg(long = "scope")]
    scope: Vec<String>,

    /// Only refetch PRs updated in the last N days. 0 = unbounded.
    #[arg(long, default_value_t = 30)]
    refresh_window_days: u32,

    /// Safety cap on PR count.
    #[arg(long)]
    max_prs: Option<usize>,

    /// Fetch specific PRs only; repeatable. Accepts `owner/repo#NUM`,
    /// `owner/repo/pull/NUM`, or a github.com PR URL. When supplied,
    /// discovery is skipped and only the listed PRs are fetched.
    #[arg(long = "pull-request", value_name = "REF")]
    pull_request: Vec<String>,

    /// Ignore sync_state.json and walk the full refresh window.
    #[arg(long)]
    full: bool,

    /// Seconds between successful per-PR fetches.
    #[arg(long, default_value_t = 0.0)]
    sleep_between: f64,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "github-download")?;

    let scopes = if args.scope.is_empty() {
        DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()
    } else {
        args.scope.clone()
    };
    let targets: Vec<(String, u32)> = args
        .pull_request
        .iter()
        .map(|s| parse_pr_ref(s))
        .collect::<Result<Vec<_>>>()?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        scopes,
        refresh_window_days: args.refresh_window_days,
        max_prs: args.max_prs,
        targets,
        full_sync: args.full,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        ..Default::default()
    };

    let span = info_span!("github_download", out = %args.out.display());
    let summary = github::fetch(opts).instrument(span).await?;
    info!(
        event = "github_download_complete",
        new_prs = summary.new_prs,
        new_issue_comments = summary.new_issue_comments,
        new_reviews = summary.new_reviews,
        new_review_comments = summary.new_review_comments,
        requests = summary.requests,
    );
    Ok(())
}
