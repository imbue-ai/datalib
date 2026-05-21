//! `anthropic-translate` — read the raw API capture written by
//! `anthropic-download` and emit one CommonMark `.md` per Claude
//! conversation plus a co-located `*.grid_rows.json` sidecar.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_anthropic::translate::parse::parse_export;
use frankweiler_etl_anthropic::translate::render::render_all;
use tracing::{info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "anthropic-translate",
    about = "Translate captured Anthropic raw_api into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Output root (same value passed to `anthropic-download --out`).
    /// The translator reads `<out>/raw_api/` and writes to
    /// `<out>/rendered_md/anthropic/...`.
    #[arg(long, env = "ANTHROPIC_OUT")]
    out: PathBuf,

    /// Source name (matches `sources[].name` in sync config). Used as
    /// the directory key under `raw/<source_name>/blobs/...` when
    /// resolving relative media links from rendered markdown.
    #[arg(long, default_value = "anthropic")]
    source_name: String,

    #[command(flatten)]
    obs: ObsArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "anthropic-translate")?;

    let span = info_span!("anthropic_translate", out = %args.out.display());
    let _enter = span.enter();

    info!(event = "anthropic_translate_start");
    let api_dir = args.out.join("raw_api");
    let parsed =
        parse_export(&api_dir).with_context(|| format!("parse_export({})", api_dir.display()))?;
    info!(
        event = "anthropic_translate_loaded",
        accounts = parsed.accounts.len(),
        conversations = parsed.conversations.len(),
        messages = parsed.messages.len(),
        blocks = parsed.content_blocks.len(),
        attachments = parsed.attachments.len(),
    );

    let written = render_all(&parsed, &args.out, &args.source_name)?;
    info!(
        event = "anthropic_translate_complete",
        documents = written.len()
    );
    Ok(())
}
