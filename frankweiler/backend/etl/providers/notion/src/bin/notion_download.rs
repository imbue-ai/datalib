//! `notion-download` — mirror Notion pages via the official API into a
//! single doltlite database file.
//!
//! Requires `latchkey` with two services registered:
//!   - `notion` (Bearer token for `api.notion.com`)
//!   - `notion_unofficial` (cookie session for `www.notion.so/api/v3`),
//!     needed only when `--inbox` is used.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_notion::extract::{self as notion, FetchOptions};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "notion-download",
    about = "Mirror Notion pages via the official API into a doltlite DB."
)]
struct Args {
    /// Path to the doltlite database file. If a directory or extensionless
    /// path is passed, `.doltlite_db` is appended automatically.
    #[arg(long, env = "NOTION_OUT")]
    out: PathBuf,

    /// Root page id (UUID, dashed or undashed) to BFS-mirror. Repeatable.
    #[arg(long = "subtree-page", value_name = "ID")]
    subtree_page: Vec<String>,

    /// Discover pages via the unofficial `getNotificationLog` endpoint.
    #[arg(long)]
    inbox: bool,

    /// Inbox mode: restrict to one space id (default: all visible spaces).
    #[arg(long)]
    space: Option<String>,

    #[arg(long, default_value_t = 40)]
    notification_page_size: u32,

    #[arg(long, default_value_t = 50)]
    max_notification_pages: u32,

    #[arg(long = "inbox-type", default_values_t = vec!["unread_and_read".to_string()])]
    inbox_type: Vec<String>,

    #[arg(long, default_value_t = 5000)]
    max_pages: usize,

    /// Fetch a single page by UUID instead of BFS-walking a subtree.
    #[arg(long, value_name = "UUID")]
    page: Option<String>,

    /// Re-fetch every page in the DB whose last attempt failed (or which
    /// has a NULL payload after at least one attempt). Ignores subtree /
    /// inbox / page.
    #[arg(long)]
    retry_failed: bool,

    #[arg(long, default_value_t = 0.0)]
    sleep_between: f64,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "notion-download")?;

    if !args.retry_failed && args.subtree_page.is_empty() && !args.inbox && args.page.is_none() {
        anyhow::bail!("must specify --inbox, --subtree-page, --page, or --retry-failed");
    }

    let opts = FetchOptions {
        db_path: args.out.clone(),
        subtree_pages: args.subtree_page.clone(),
        inbox: args.inbox,
        space: args.space.clone(),
        notification_page_size: args.notification_page_size,
        max_notification_pages: args.max_notification_pages,
        inbox_types: args.inbox_type.clone(),
        max_pages: args.max_pages,
        page: args.page.clone(),
        retry_failed: args.retry_failed,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        ..Default::default()
    };

    let span = info_span!("notion_download", db = %args.out.display());
    let summary = notion::fetch(opts).instrument(span).await?;
    info!(
        event = "notion_download_complete",
        new_pages = summary.new_pages,
        upd_pages = summary.upd_pages,
        new_blocks = summary.new_blocks,
        upd_blocks = summary.upd_blocks,
        new_comments = summary.new_comments,
        upd_comments = summary.upd_comments,
        skipped_pages = summary.skipped_pages,
        new_blobs = summary.new_blobs,
        skipped_blobs = summary.skipped_blobs,
        failed_blobs = summary.failed_blobs,
        official_requests = summary.official_requests,
        unofficial_requests = summary.unofficial_requests,
    );
    Ok(())
}
