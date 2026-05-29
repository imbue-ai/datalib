//! `gitlab-translate` — read event-store JSONL written by
//! `gitlab-download` and render one markdown doc per MR plus a
//! co-located `*.grid_rows.json` sidecar.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl_gitlab::translate::{parse_api_dir, render_gitlab};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "gitlab-translate",
    about = "Translate captured GitLab JSONL into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Input directory (the value passed to `gitlab-download --out`).
    #[arg(long, env = "GITLAB_OUT")]
    out: PathBuf,

    /// Render root. Defaults to `<out>`.
    #[arg(long)]
    render_root: Option<PathBuf>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "gitlab-translate")?;

    let span = info_span!("gitlab_translate", out = %args.out.display());
    let _enter = span.enter();

    info!(event = "gitlab_translate_start");
    let parsed = parse_api_dir(&args.out)
        .with_context(|| format!("parse_api_dir({})", args.out.display()))?;
    info!(
        event = "gitlab_translate_loaded",
        merge_requests = parsed.merge_requests.len(),
        notes = parsed.notes.len(),
    );

    let render_root = args.render_root.clone().unwrap_or_else(|| args.out.clone());
    let summary = render_gitlab(
        &parsed,
        &render_root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )?;
    info!(
        event = "gitlab_translate_complete",
        rendered = summary.rendered,
        skipped = summary.skipped,
    );
    Ok(())
}
