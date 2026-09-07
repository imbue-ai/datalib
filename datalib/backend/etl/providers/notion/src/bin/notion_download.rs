//! `notion-download` — mirror Notion pages via the official API into a
//! single doltlite database file.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use datalib_etl_notion::download::{self as notion, FetchOptions};
use datalib_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "notion-download",
    about = "Mirror Notion pages via the official API into a doltlite DB."
)]
struct Args {
    /// Path to the doltlite database file. The entity db lives inside the
    /// per-source directory as `entities.doltlite_db` (the dir is created
    /// if needed).
    #[arg(long, env = "NOTION_OUT")]
    out: PathBuf,

    /// Root page id (UUID, dashed or undashed) to BFS-mirror. Repeatable.
    #[arg(long = "subtree-page", value_name = "ID")]
    subtree_page: Vec<String>,
    /// Stop after this many pages. Omit for no limit.
    #[arg(long)]
    max_pages: Option<usize>,

    /// Fetch a single page by UUID instead of BFS-walking a subtree.
    #[arg(long, value_name = "UUID")]
    page: Option<String>,

    /// Re-fetch every page in the DB whose last attempt failed (or which
    /// has a NULL payload after at least one attempt). Ignores subtree /
    /// roots / page.
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

    if !args.retry_failed && args.subtree_page.is_empty() && args.page.is_none() {
        anyhow::bail!("must specify --subtree-page, --page, or --retry-failed");
    }

    let opts = FetchOptions {
        db_path: args.out.clone(),
        subtree_pages: args.subtree_page.clone(),
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
        bodies = summary.bodies,
        empty_bodies = summary.empty_bodies,
        failed_bodies = summary.failed_bodies,
        new_comments = summary.new_comments,
        upd_comments = summary.upd_comments,
        skipped_pages = summary.skipped_pages,
        new_blobs = summary.new_blobs,
        skipped_blobs = summary.skipped_blobs,
        failed_blobs = summary.failed_blobs,
        official_requests = summary.official_requests,
    );
    Ok(())
}
