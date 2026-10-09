//! `notion-ingest` — mirror Notion pages via the official API into a
//! single doltlite database file.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use datalib_etl_notion::ingest::{self as notion, db_path_for, FetchOptions, RawDb};
use datalib_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "notion-ingest",
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

    /// Ignore what the search has covered and walk the whole workspace.
    #[arg(long)]
    full_sync: bool,

    /// List this many days of edits below the newest the store has
    /// looked at, so a page shared late is listed once more.
    #[arg(long, default_value_t = 0)]
    refresh_window_days: u32,

    /// Fetch a single page by UUID, and nothing under it.
    #[arg(long, value_name = "UUID")]
    page: Option<String>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "notion-ingest")?;

    // No flag at all is the whole-workspace mirror: search enumerates
    // what the token can see, so there is nothing to require.

    // The one writer: this process opens the store, hands the handle to
    // `fetch`, and closes it below.
    let db = RawDb::open(&db_path_for(&args.out)).await?;
    let opts = FetchOptions {
        subtree_pages: args.subtree_page.clone(),
        max_pages: args.max_pages,
        full_sync: args.full_sync,
        refresh_window_days: args.refresh_window_days,
        page: args.page.clone(),
        ..FetchOptions::new(db.clone())
    };

    let span = info_span!("notion_ingest", db = %args.out.display());
    let summary = notion::fetch(opts).instrument(span).await;
    db.close().await;
    let summary = summary?;
    info!(
        event = "notion_download_complete",
        listed = summary.listed,
        new_pages = summary.new_pages,
        upd_pages = summary.upd_pages,
        skipped_pages = summary.skipped_pages,
        bodies = summary.bodies,
        empty_bodies = summary.empty_bodies,
        failed_bodies = summary.failed_bodies,
        comments = summary.comments,
        new_blobs = summary.new_blobs,
        failed_blobs = summary.failed_blobs,
        official_requests = summary.official_requests,
        "the notion download is done"
    );
    Ok(())
}
