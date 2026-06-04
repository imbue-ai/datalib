//! `jmap-download` — CLI driver for the JMAP extract step.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl_jmap::extract::{self as jmap, FetchOptions};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::{info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "jmap-download",
    about = "Mirror a JMAP mail account into a single doltlite db."
)]
struct Args {
    /// Output path. Resolves to `<path>.doltlite_db` if it doesn't
    /// already carry the extension.
    #[arg(long, env = "JMAP_OUT")]
    out: PathBuf,

    /// JMAP server hostname (`api.fastmail.com` for Fastmail). Session
    /// is discovered at `https://<hostname>/.well-known/jmap`.
    #[arg(long, env = "JMAP_HOSTNAME")]
    hostname: String,

    /// JMAP account id. Defaults to the session's primary mail account.
    #[arg(long, env = "JMAP_ACCOUNT_ID")]
    account_id: Option<String>,

    /// Force full enumeration even if a state token is present.
    #[arg(long, default_value_t = false)]
    full_resync: bool,

    /// Restrict the sync to these mailbox ids (comma-separated). Empty
    /// = every mailbox the account exposes.
    #[arg(long, value_delimiter = ',')]
    only_mailbox_ids: Vec<String>,

    /// Skip downloading any blob whose advertised size exceeds this.
    #[arg(long, env = "JMAP_BLOB_SIZE_LIMIT_BYTES")]
    blob_size_limit_bytes: Option<u64>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "jmap-download")?;

    let opts = FetchOptions {
        db_path: args.out.clone(),
        hostname: args.hostname.clone(),
        account_id: args.account_id.clone(),
        full_resync: args.full_resync,
        only_mailbox_ids: args.only_mailbox_ids.clone(),
        blob_size_limit_bytes: args.blob_size_limit_bytes,
        ..Default::default()
    };

    let span = info_span!(
        "jmap_download",
        hostname = %args.hostname,
        out = %args.out.display(),
    );
    jmap::fetch(opts).instrument(span).await?;
    Ok(())
}
