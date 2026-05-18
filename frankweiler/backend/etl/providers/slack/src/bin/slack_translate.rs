//! `slack-translate` — Translate step of the Slack ETL: read the raw_api
//! capture written by `slack-download` and emit one CommonMark `.md` per
//! Slack thread plus a co-located `*.grid_rows.json` sidecar.
//!
//! Incremental: each `.md` carries a `source_fingerprint` derived from
//! the raw payloads of its messages. Re-running with no upstream
//! changes is a no-op (zero writes).
//!
//! Translate-only. The Load step is the provider-agnostic
//! `grid-rows-load` binary, which reads the `.grid_rows.json` tree and
//! never touches `raw_api/`.
//!
//! ```sh
//! slack-translate --out ~/slack-mirror
//! slack-translate --out ~/slack-mirror --otlp-endpoint http://localhost:4317
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::obs::{init as init_obs, ObsArgs};
use frankweiler_etl_slack::translate::render::render_all;
use frankweiler_etl_slack::translate::translate_raw_dir;
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

#[derive(Parser, Debug)]
#[command(
    name = "slack-translate",
    about = "Translate captured Slack raw_api into rendered_md/ + grid_rows sidecars."
)]
struct Args {
    /// Output root (same value passed to `slack-download --out`). The
    /// translator reads `<out>/raw_api/` and writes to
    /// `<out>/rendered_md/slack/...`.
    #[arg(long, env = "SLACK_OUT")]
    out: PathBuf,

    #[command(flatten)]
    obs: ObsArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "slack-translate")?;

    let span = info_span!(
        "slack_translate",
        out = %args.out.display(),
        threads_done = tracing::field::Empty,
        threads_total = tracing::field::Empty,
        indicatif.pb_show = tracing::field::Empty,
    );
    let _enter = span.enter();

    info!(event = "slack_translate_start");
    let t = translate_raw_dir(&args.out)?;
    info!(
        event = "slack_translate_loaded",
        users = t.users.len(),
        channels = t.channels.len(),
        messages = t.messages.len(),
    );

    // Indicatif progress bar driven by a closure passed into `render_all`.
    let done = AtomicUsize::new(0);
    let summary = render_all(&t, &args.out, |msg| {
        let _ = done.fetch_add(1, Ordering::Relaxed);
        tracing::Span::current().pb_set_message(msg);
    })?;

    info!(
        event = "slack_translate_complete",
        threads_total = summary.threads_total,
        threads_rendered = summary.threads_rendered,
        threads_skipped = summary.threads_skipped,
    );
    Ok(())
}
