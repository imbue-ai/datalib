//! `beeper-translate` — placeholder CLI for Milestones C+ (per-bridge
//! translators). Milestone A: parses the raw doltlite store and
//! reports counts only; no markdown is rendered yet.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_beeper::translate;
use frankweiler_obs::{init as init_obs, ObsArgs};

#[derive(Parser, Debug)]
#[command(
    name = "beeper-translate",
    about = "Translate the Beeper raw store into rendered markdown + grid_rows sidecars."
)]
struct Args {
    /// Input doltlite raw store (the path `beeper-download --out` wrote to).
    #[arg(long, env = "BEEPER_IN")]
    input: PathBuf,

    /// Output root for `rendered_md/`. Milestone A: not used.
    #[arg(long)]
    out: Option<PathBuf>,

    #[command(flatten)]
    obs: ObsArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "beeper-translate")?;
    let parsed = translate::parse_raw_dir(&args.input)?;
    let _ = parsed;
    eprintln!("[beeper-translate] no translators wired up yet (Milestone A)");
    Ok(())
}
