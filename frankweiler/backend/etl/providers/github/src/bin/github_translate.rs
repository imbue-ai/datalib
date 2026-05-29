//! `github-translate` — read event-store JSONL written by
//! `github-download` and render one markdown doc per PR plus a
//! co-located `*.grid_rows.json` sidecar.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl_github::translate::{parse_api_dir, render_github};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "github-translate",
    about = "Translate captured GitHub JSONL into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Input directory (the value passed to `github-download --out`).
    #[arg(long, env = "GITHUB_OUT")]
    out: PathBuf,

    /// Render root. Defaults to `<out>`.
    #[arg(long)]
    render_root: Option<PathBuf>,

    #[command(flatten)]
    obs: ObsArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "github-translate")?;

    let span = info_span!("github_translate", out = %args.out.display());
    let _enter = span.enter();

    info!(event = "github_translate_start");
    let parsed = parse_api_dir(&args.out)
        .with_context(|| format!("parse_api_dir({})", args.out.display()))?;
    info!(
        event = "github_translate_loaded",
        pull_requests = parsed.pull_requests.len(),
        comments = parsed.comments.len(),
    );

    let render_root = args.render_root.clone().unwrap_or_else(|| args.out.clone());
    let summary = render_github(
        &parsed,
        &render_root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )?;
    info!(
        event = "github_translate_complete",
        rendered = summary.rendered,
        skipped = summary.skipped,
    );
    Ok(())
}
