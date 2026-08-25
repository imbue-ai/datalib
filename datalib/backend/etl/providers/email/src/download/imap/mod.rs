//! IMAP downloader — the third mode of `type: email`, alongside JMAP and
//! `.mbox`.
//!
//! Writes the same raw schema as the other two (see
//! [`super::schema_raw`]), so render is mode-agnostic and a mailbox
//! ingested from a Google Takeout export and then switched to live IMAP
//! dedupes rather than doubling.
//!
//! ## State of play
//!
//! Connect, authenticate, capability detection and folder discovery are
//! implemented and exercised here. **The message pass is not yet wired**
//! — [`fetch`] performs discovery, reports what it found, and then
//! returns an error rather than writing a partial mirror. That makes it
//! a usable connectivity check for a new latchkey credential today, and
//! keeps the raw store untouched until the ingest path lands.
//!
//! See `docs/dev/email_imap_mode.md` §5 for the phases still to come:
//! UID enumeration with `BODY.PEEK[]`, the CONDSTORE incremental pass,
//! deletion reconciliation, and the byte budget.

pub mod conn;
pub mod folders;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::latchkey::extract_credential;
use datalib_etl::progress::Progress;
use futures::TryStreamExt;
use serde::Serialize;
use tracing::info;

use datalib_etl_email_config::EmailImap;

use super::db::RawDb;
use folders::Folder;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (the processor populates this so the
    /// post-download commit hits the same pool).
    pub db: Option<RawDb>,
    /// The source's `imap:` block.
    pub config: EmailImap,
    /// When non-empty, only ingest messages carrying at least one label
    /// whose full path exactly matches one of these. Same semantics and
    /// same matcher as the JMAP and mbox paths.
    pub only_labels: Vec<String>,
    /// Skip message bodies larger than this.
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: Progress,
    pub control: DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            config: EmailImap::default(),
            only_labels: Vec::new(),
            blob_size_limit_bytes: None,
            progress: Progress::noop(),
            control: DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub folders_upserted: usize,
    pub emails_upserted: usize,
    pub emails_destroyed: usize,
    pub blobs_stored: usize,
    pub blobs_skipped: usize,
    pub blobs_oversize: usize,
    /// Bytes of message body pulled this run, against
    /// `daily_download_budget_bytes`.
    pub bytes_downloaded: u64,
    /// True when the run stopped early because the byte budget ran out.
    /// A partial backfill is a successful outcome, not a failure — see
    /// the budget rationale on [`EmailImap::daily_download_budget_bytes`].
    pub budget_exhausted: bool,
}

/// Mirror an IMAP account into the raw store.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let cfg = &opts.config;
    let cred = extract_credential(&cfg.latchkey_service)
        .await
        .with_context(|| {
            format!(
                "reading the IMAP credential for {} out of latchkey",
                cfg.host
            )
        })?;

    let connected =
        conn::connect(&cfg.host, cfg.port(), &cred, cfg.email_address.as_deref()).await?;
    let mut session = connected.session;

    let discovered = list_folders(&mut session).await?;
    let all_mail = folders::all_mail(&discovered, cfg.all_mail_folder.as_deref());
    info!(
        event = "imap_folders_listed",
        host = %cfg.host,
        account = %connected.username,
        folders = discovered.len(),
        all_mail = all_mail.map(|f| f.name.as_str()).unwrap_or("(none)"),
        "discovered",
    );

    // Leave the server tidy even on the error path below; a dangling
    // session counts against Gmail's 15-connection ceiling until it
    // times out.
    let _ = session.logout().await;

    bail!(
        "IMAP connectivity to {host} is working: authenticated as {user}, {n} folders, \
         all-mail folder {all}, capabilities gmail={gmail} condstore={condstore}. \
         The message pass is not implemented yet, so nothing was written — see \
         docs/dev/email_imap_mode.md §5.",
        host = cfg.host,
        user = connected.username,
        n = discovered.len(),
        all = all_mail.map(|f| f.name.as_str()).unwrap_or("(none)"),
        gmail = connected.caps.gmail,
        condstore = connected.caps.condstore,
    )
}

/// `LIST "" "*"` — every folder the account exposes.
async fn list_folders(session: &mut conn::ImapSession) -> Result<Vec<Folder>> {
    let names: Vec<async_imap::types::Name> = session
        .list(None, Some("*"))
        .await
        .context("IMAP LIST")?
        .try_collect()
        .await
        .context("reading the IMAP LIST response")?;
    Ok(names
        .iter()
        .map(|n| Folder::from_list_entry(n.name(), n.delimiter(), n.attributes()))
        .collect())
}
