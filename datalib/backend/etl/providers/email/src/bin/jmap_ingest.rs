//! `jmap-ingest` — CLI driver for the JMAP download step.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use datalib_etl::standalone::StandaloneRun;
use datalib_etl_email::ingest::{self as jmap, db_path_for, FetchOptions, RawDb};
use datalib_obs::{init as init_obs, ObsArgs};
use tracing::{info, info_span, Instrument};

#[derive(Parser, Debug)]
#[command(
    name = "jmap-ingest",
    about = "Mirror a JMAP mail account into a single doltlite db."
)]
struct Args {
    /// Output path. The entity db lives inside the per-source directory
    /// as `entities.doltlite_db` (the dir is created if needed).
    #[arg(long, env = "JMAP_OUT")]
    out: PathBuf,

    /// JMAP server hostname (`api.fastmail.com` for Fastmail). Session
    /// is discovered at `https://<hostname>/.well-known/jmap`.
    #[arg(long, env = "JMAP_HOSTNAME")]
    hostname: String,

    /// JMAP account id. Defaults to the session's primary mail account.
    #[arg(long, env = "JMAP_ACCOUNT_ID")]
    account_id: Option<String>,

    /// Restrict the sync to mailboxes whose full label path matches one
    /// of these (comma-separated, POSIX-like e.g. `Work/Projects`).
    /// Empty = every mailbox the account exposes.
    #[arg(long, value_delimiter = ',')]
    only_mailbox_labels: Vec<String>,

    /// Skip downloading any blob whose advertised size exceeds this.
    #[arg(long, env = "JMAP_BLOB_SIZE_LIMIT_BYTES")]
    blob_size_limit_bytes: Option<u64>,

    /// How many `.eml` downloads to keep in flight in the blob phase.
    /// Unset uses the built-in default; `1` is strictly serial.
    #[arg(long, env = "JMAP_BLOB_DOWNLOAD_CONCURRENCY")]
    blob_download_concurrency: Option<usize>,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "jmap-ingest")?;

    let run = StandaloneRun::start("email", &args.out);
    let db = RawDb::open(&db_path_for(&args.out)).await?;
    let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
    let span = info_span!(
        "jmap_ingest",
        hostname = %args.hostname,
        out = %args.out.display(),
    );
    let summary = run
        .run_store(pool, Some(cas_pool), |sealer| async {
            let opts = FetchOptions {
                hostname: args.hostname.clone(),
                account_id: args.account_id.clone(),
                only_mailbox_labels: args.only_mailbox_labels.clone(),
                blob_size_limit_bytes: args.blob_size_limit_bytes,
                blob_download_concurrency: args.blob_download_concurrency,
                sealer: Some(sealer),
                progress: run.progress(),
                control: run.control(),
                ..FetchOptions::new(db)
            };
            let s = jmap::fetch(opts).instrument(span).await?;
            Ok(format!(
                "mailboxes={} emails={} destroyed={} blobs(dl={} oversize={} err={})",
                s.mailboxes_upserted,
                s.emails_upserted,
                s.emails_destroyed,
                s.blobs_downloaded,
                s.blobs_oversize,
                s.blobs_errored,
            ))
        })
        .await?;
    info!(summary = %summary, "the jmap download is done");
    Ok(())
}
