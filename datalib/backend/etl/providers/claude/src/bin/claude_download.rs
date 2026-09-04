//! `claude-download` — mirror claude.ai conversations in the
//! Claude-export shape so the existing translator works against
//! either source indistinguishably.
//!
//! Requires `latchkey` (with the `claude-ai` service registered) and
//! a Cloudflare-clearing curl impersonator on `LATCHKEY_CURL`. See
//! `EXTRACT.md` in this crate.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use datalib_etl_claude::download::{self as claude, FetchOptions, DEFAULT_OVERLAP, SLEEP_BETWEEN};
use datalib_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "claude-download",
    about = "Mirror claude.ai conversations in the Claude-export shape."
)]
struct Args {
    /// Output directory. Created if missing.
    #[arg(long, env = "ANTHROPIC_OUT")]
    out: PathBuf,

    /// Optional bulk-export dir to seed listing/overlap and copy
    /// `users.json` from. The export format is deprecated upstream but
    /// existing local exports still work as a seed.
    #[arg(long)]
    export_dir: Option<PathBuf>,

    /// N most-recently-updated export conversations to refetch from the
    /// API as overlap (sanity-check the live API vs. export).
    #[arg(long, default_value_t = DEFAULT_OVERLAP)]
    overlap: usize,

    /// Seconds between successful conversation fetches.
    #[arg(long, default_value_t = SLEEP_BETWEEN.as_secs_f64())]
    sleep_between: f64,

    /// Only sync conversations whose `updated_at` is at or after this
    /// instant (RFC 3339 or YYYY-MM-DD, assumed UTC). Older
    /// conversations are never detail-fetched.
    #[arg(long)]
    since: Option<String>,

    /// Fetch only these conversation UUIDs instead of walking the full
    /// listing. Pass `--conv-uuid` once per target. Tries each org until
    /// one returns 200; 403/404 are treated as "wrong org, continue".
    /// Merges results into the existing `conversations.json` rather
    /// than replacing it.
    #[arg(long = "conv-uuid", value_name = "UUID")]
    conv_uuids: Vec<String>,

    /// Skip the Claude Projects mirror (project metadata + knowledge
    /// documents). Mirrors the config's `sync.projects = false`.
    #[arg(long)]
    no_projects: bool,

    /// Mirror only these projects instead of every one the account can
    /// see. Pass `--project-uuid` once per target (bare UUID or a
    /// paste-able `https://claude.ai/project/<uuid>` URL). Mirrors the
    /// config's `sync.project_uuids`.
    #[arg(long = "project-uuid", value_name = "UUID")]
    project_uuids: Vec<String>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "claude-download")?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        export_dir: args.export_dir.clone(),
        overlap: args.overlap,
        sleep_between: Duration::from_secs_f64(args.sleep_between.max(0.0)),
        since: args.since.clone(),
        conv_uuids: args.conv_uuids.clone(),
        projects: !args.no_projects,
        project_uuids: args.project_uuids.clone(),
        ..Default::default()
    };

    let span = info_span!("claude_download", out = %args.out.display());
    let summary = claude::fetch(opts).instrument(span).await?;
    info!(
        event = "claude_download_complete",
        total = summary.total,
        fetched = summary.fetched,
        skipped = summary.skipped,
        out_of_scope = summary.out_of_scope,
        forbidden_orgs = summary.forbidden_orgs,
        projects = summary.projects_fetched,
        project_docs = summary.project_docs_fetched,
        errors = summary.errors,
        requests = summary.requests,
        network_seconds = summary.network_seconds,
    );
    Ok(())
}
