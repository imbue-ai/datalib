//! `notion-download` — mirror Notion pages via the official API, with
//! optional inbox discovery via the unofficial `getNotificationLog`.
//!
//! Requires `latchkey` with two services registered:
//!   - `notion` (Bearer token for `api.notion.com`)
//!   - `notion_unofficial` (cookie session for `www.notion.so/api/v3`),
//!     needed only when `--inbox` is used.
//!
//! A Cloudflare-clearing curl impersonator on `LATCHKEY_CURL` is required
//! for the unofficial API. See `EXTRACT.md` in this crate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_notion::extract::{self as notion, FetchOptions};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "notion-download",
    about = "Mirror Notion pages via the official API."
)]
struct Args {
    /// Output directory. Created if missing.
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

    /// `getNotificationLog` page size.
    #[arg(long, default_value_t = 40)]
    notification_page_size: u32,

    /// Safety bound on inbox pagination per space per type.
    #[arg(long, default_value_t = 50)]
    max_notification_pages: u32,

    /// Notification feed types to walk (repeatable). Common values:
    /// `unread_and_read`, `archived`.
    #[arg(long = "inbox-type", default_values_t = vec!["unread_and_read".to_string()])]
    inbox_type: Vec<String>,

    /// Safety bound on BFS page count.
    #[arg(long, default_value_t = 5000)]
    max_pages: usize,

    /// Fetch a single page by UUID instead of BFS-walking a subtree.
    #[arg(long, value_name = "UUID")]
    page: Option<String>,

    /// Seconds between successful page fetches.
    #[arg(long, default_value_t = 0.0)]
    sleep_between: f64,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "notion-download")?;

    if args.subtree_page.is_empty() && !args.inbox && args.page.is_none() {
        anyhow::bail!("must specify --inbox, one or more --subtree-page <id>, or --page <id>");
    }

    let opts = FetchOptions {
        out_dir: args.out.clone(),
        subtree_pages: args.subtree_page.clone(),
        inbox: args.inbox,
        space: args.space.clone(),
        notification_page_size: args.notification_page_size,
        max_notification_pages: args.max_notification_pages,
        inbox_types: args.inbox_type.clone(),
        max_pages: args.max_pages,
        page: args.page.clone(),
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
    };

    let span = info_span!("notion_download", out = %args.out.display());
    let summary = notion::fetch(opts).instrument(span).await?;
    info!(
        event = "notion_download_complete",
        new_pages = summary.new_pages,
        upd_pages = summary.upd_pages,
        new_blocks = summary.new_blocks,
        upd_blocks = summary.upd_blocks,
        new_comments = summary.new_comments,
        upd_comments = summary.upd_comments,
        official_requests = summary.official_requests,
        unofficial_requests = summary.unofficial_requests,
    );
    Ok(())
}
