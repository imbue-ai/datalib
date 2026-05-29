//! `notion-translate` — read the event-store JSONL written by
//! `notion-download` and emit one CommonMark `.md` per Notion page +
//! a co-located `*.grid_rows.json` sidecar per page and per discussion.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl_notion::translate::parse_api_dir;
use frankweiler_etl_notion::translate::render::render_notion_official;
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "notion-translate",
    about = "Translate captured Notion JSONL into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Input directory containing `notion_official_{page,block,comment}/`
    /// JSONL streams (the value passed to `notion-download --out`).
    #[arg(long, env = "NOTION_OUT")]
    out: PathBuf,

    /// Render root. Defaults to `<out>/rendered_md`.
    #[arg(long)]
    render_root: Option<PathBuf>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "notion-translate")?;

    let span = info_span!("notion_translate", out = %args.out.display());
    let _enter = span.enter();

    info!(event = "notion_translate_start");
    let parsed = parse_api_dir(&args.out)
        .with_context(|| format!("parse_api_dir({})", args.out.display()))?;
    info!(
        event = "notion_translate_loaded",
        pages = parsed.pages.len(),
        blocks = parsed.blocks.len(),
        comments = parsed.comments.len(),
    );

    let render_root = args
        .render_root
        .clone()
        .unwrap_or_else(|| args.out.join("rendered_md"));
    let summary = render_notion_official(
        &parsed,
        &render_root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )?;
    info!(
        event = "notion_translate_complete",
        rendered = summary.rendered,
        skipped = summary.skipped,
        orphans_removed = summary.orphans_removed,
    );
    Ok(())
}
