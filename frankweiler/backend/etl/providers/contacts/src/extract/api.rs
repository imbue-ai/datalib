//! Thin CardDAV client built on top of [`frankweiler_etl::http`].
//!
//! Status: STUB. The shapes and constants here are real (they're what
//! the orchestrator in `extract/mod.rs` will consume), but the
//! `PROPFIND` / `REPORT` flows + multistatus parsing land in a
//! follow-up commit.
//!
//! Design notes for the imminent fill-in:
//!
//! * Discovery walks `current-user-principal` → `principal_url` →
//!   `addressbook-home-set` → addressbook list. Each step is one
//!   `PROPFIND` with depth `0` or `1`. We follow `<href>` redirects
//!   manually rather than letting curl chase them, because some
//!   servers (Google) return cross-host hrefs and we need to keep
//!   the URL within the latchkey-keyed host.
//!
//! * Incremental sync is one `REPORT` per addressbook with a
//!   `<sync-collection>` body carrying the previously-stored
//!   `<sync-token>`. The response is a multistatus document with
//!   one `<response>` per changed contact (etag in `<getetag>`) or
//!   deletion (`<status>HTTP/1.1 404 Not Found</status>`). A new
//!   `<sync-token>` lives at the document root and is what we
//!   persist via [`super::db::RawDb::set_sync_token`].
//!
//! * Body fetch for the changed contacts is a second `REPORT` with a
//!   `<addressbook-multiget>` body listing the hrefs. Each
//!   `<response>` carries the vCard text inside
//!   `<address-data>` — the body we'll feed to translate.

use thiserror::Error;

/// XML namespaces we emit in request bodies + accept in responses.
/// Servers vary in prefix (`d:` vs `D:`) but the URIs are fixed.
pub const NS_DAV: &str = "DAV:";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";

#[derive(Error, Debug)]
pub enum CarddavError {
    /// Transport-level failure (DNS, TLS, latchkey).
    #[error("carddav transport: {0}")]
    Transport(String),
    /// Server responded with a non-success HTTP status that we don't
    /// know how to retry/recover.
    #[error("carddav http {status}: {url}")]
    Http { status: u16, url: String },
    /// Multistatus response we couldn't parse — usually a server
    /// bug or namespace surprise.
    #[error("carddav malformed response: {0}")]
    Malformed(String),
}
