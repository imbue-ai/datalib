//! CardDAV downloader entry point.
//!
//! See [`super`] for the overall provider story. This module owns the
//! orchestration:
//!
//!   1. Discover the principal URL + addressbook-home-set (PROPFIND).
//!   2. List addressbooks under the home set (PROPFIND, depth=1).
//!   3. For each addressbook: incremental `sync-collection` REPORT
//!      against the persisted sync-token, falling back to a ctag
//!      check + etag walk if the server refuses.
//!   4. Upsert raw vCards + bookkeeping into the doltlite store.
//!
//! Auth headers are injected by latchkey based on the URL host (see
//! `frankweiler_etl::http`). The provider does NOT touch credentials
//! directly.

pub mod api;
pub mod db;

pub use db::{db_path_for, RawDb};

use std::path::PathBuf;

use anyhow::Result;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;

/// Options for one `fetch` run. The shape mirrors what other
/// providers expose to [`crate::extract::fetch`].
pub struct FetchOptions {
    /// Doltlite database path. May be either a `<...>.doltlite_db`
    /// file or the legacy directory shape; [`db_path_for`] resolves
    /// either form.
    pub db_path: PathBuf,
    /// Root URL of the user's CardDAV server. We start discovery here
    /// (PROPFIND for `current-user-principal`). Example values:
    /// `https://contacts.icloud.com/`,
    /// `https://carddav.fastmail.com/`,
    /// `https://www.googleapis.com/carddav/v1/principals/`.
    pub server_url: String,
    /// Restrict the run to the named addressbooks (matched against
    /// the addressbook's `displayname`). `None` = sync every
    /// addressbook the server lists under the principal.
    pub addressbooks: Option<Vec<String>>,
    pub progress: Progress,
    pub control: ExtractControl,
}

/// One-shot summary returned from a fetch run. Each provider crate
/// emits its own summary shape; this one tracks the CardDAV-specific
/// counters the sync runner stitches into its end-of-run line.
#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub addressbooks: usize,
    pub contacts_new: usize,
    pub contacts_updated: usize,
    pub contacts_deleted: usize,
    pub errors: usize,
    pub requests: usize,
}

/// Run one extract pass against `opts.server_url`. The actual
/// orchestration is filled in incrementally — this stub opens the DB
/// (so the doltlite file exists + DDL is applied) and honors
/// `control.reset_and_redownload`, but doesn't fetch contacts yet.
/// The CardDAV transport + sync-collection loop land in the next
/// commits.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(&opts.db_path)).await?;
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    let _ = (&opts.server_url, &opts.addressbooks, &opts.progress);
    Ok(FetchSummary::default())
}
