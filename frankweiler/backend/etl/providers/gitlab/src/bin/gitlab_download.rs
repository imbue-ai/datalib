//! `gitlab-download` — mirror MRs the user authored / was assigned to /
//! is a reviewer on, plus every discussion + note. Output is event-store
//! JSONL under `<out>/<entity>/{created,updated}/events.jsonl`.
//!
//! Requires `latchkey` with a `gitlab` service registered (PRIVATE-TOKEN).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_gitlab::extract::{self as gitlab, parse_mr_ref, FetchOptions, DEFAULT_SCOPES};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "gitlab-download",
    about = "Mirror GitLab MRs + discussions via the REST API."
)]
struct Args {
    /// Output directory. Created if missing.
    #[arg(long, env = "GITLAB_OUT")]
    out: PathBuf,

    /// Discovery scope, repeatable. Default: created_by_me assigned_to_me reviewer.
    #[arg(long = "scope")]
    scope: Vec<String>,

    /// Only refetch MRs updated in the last N days. 0 = unbounded.
    #[arg(long, default_value_t = 30)]
    refresh_window_days: u32,

    /// Safety cap on MR count.
    #[arg(long)]
    max_mrs: Option<usize>,

    /// Fetch specific MRs only; repeatable. Accepts
    /// `namespace/project!IID` or a gitlab.com MR URL. When supplied,
    /// discovery is skipped and only the listed MRs are fetched.
    #[arg(long = "merge-request", value_name = "REF")]
    merge_request: Vec<String>,

    /// Ignore sync_state.json and walk the full refresh window.
    #[arg(long)]
    full: bool,

    /// Seconds between successful per-MR fetches.
    #[arg(long, default_value_t = 0.0)]
    sleep_between: f64,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "gitlab-download")?;

    let scopes = if args.scope.is_empty() {
        DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()
    } else {
        args.scope.clone()
    };
    let targets: Vec<(String, u32)> = args
        .merge_request
        .iter()
        .map(|s| parse_mr_ref(s))
        .collect::<Result<Vec<_>>>()?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        scopes,
        refresh_window_days: args.refresh_window_days,
        max_mrs: args.max_mrs,
        targets,
        full_sync: args.full,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        ..Default::default()
    };

    let span = info_span!("gitlab_download", out = %args.out.display());
    let summary = gitlab::fetch(opts).instrument(span).await?;
    info!(
        event = "gitlab_download_complete",
        new_mrs = summary.new_mrs,
        new_discussions = summary.new_discussions,
        requests = summary.requests,
    );
    Ok(())
}
