//! `chatgpt-translate` — read the raw API capture written by
//! `chatgpt-download` and emit one CommonMark `.md` per ChatGPT
//! conversation plus a co-located `*.grid_rows.json` sidecar.
//!
//! Translate-only. The Load step is the provider-agnostic
//! `grid-rows-load` binary in `frankweiler-etl`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_chatgpt::translate::parse::parse_api_dir;
use frankweiler_etl_chatgpt::translate::render::render_all;
use tracing::{info, info_span};

#[derive(Parser, Debug)]
#[command(
    name = "chatgpt-translate",
    about = "Translate captured ChatGPT raw_api into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Output root (same value passed to `chatgpt-download --out`). The
    /// translator reads `<out>/raw_api/` and writes to
    /// `<out>/rendered_md/openai/...`.
    #[arg(long, env = "CHATGPT_OUT")]
    out: PathBuf,

    /// Source name (matches `sources[].name` in sync config). Used as
    /// the directory key under `raw/<source_name>/blobs/...` when
    /// resolving relative media links from rendered markdown.
    #[arg(long, default_value = "chatgpt")]
    source_name: String,

    #[command(flatten)]
    obs: ObsArgs,
}

// Multi-thread runtime: `parse_api_dir` -> `block_on_load_all` uses
// `tokio::task::block_in_place`, which requires a multi-thread flavor.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "chatgpt-translate")?;

    let span = info_span!("chatgpt_translate", out = %args.out.display());
    let _enter = span.enter();

    info!(event = "chatgpt_translate_start");
    let api_dir = args.out.join("raw_api");
    let parsed =
        parse_api_dir(&api_dir).with_context(|| format!("parse_api_dir({})", api_dir.display()))?;
    info!(
        event = "chatgpt_translate_loaded",
        accounts = parsed.accounts.len(),
        conversations = parsed.conversations.len(),
        messages = parsed.messages.len(),
        content_parts = parsed.content_parts.len(),
    );

    let written = render_all(&parsed, &args.out, &args.source_name)?;
    info!(
        event = "chatgpt_translate_complete",
        documents = written.len()
    );
    Ok(())
}
