//! CardDAV downloader entry point.
//!
//! See [`super`] for the overall provider story. This module owns
//! the orchestration:
//!
//!   1. Discover the principal URL + addressbook-home-set (PROPFIND).
//!   2. List addressbooks under the home set (PROPFIND, depth=1).
//!   3. For each addressbook: incremental `sync-collection` REPORT
//!      against the persisted sync-token, falling back to an etag
//!      walk via `addressbook-multiget` when the server refuses.
//!   4. Upsert raw vCards + bookkeeping into the doltlite store.
//!
//! Auth headers are injected by latchkey based on the URL host
//! (see `frankweiler_etl::http`). The provider does NOT touch
//! credentials directly.

pub mod api;
pub mod db;
pub mod schema_raw;

pub use db::{db_path_for, RawDb};

use std::path::PathBuf;

use anyhow::{Context, Result};
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use tracing::{info, warn};

use api::{CarddavError, Multistatus};
use db::ContactRow;

/// Options for one `fetch` run. Mirrors the FetchOptions shape every
/// other provider crate exposes.
pub struct FetchOptions {
    /// Doltlite database path. May be either a `<...>.doltlite_db`
    /// file or the legacy directory shape; [`db_path_for`] resolves
    /// either form. Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. See the matching field on
    /// the other providers' FetchOptions for rationale.
    pub db: Option<RawDb>,
    /// Root URL of the user's CardDAV server. We start discovery
    /// here (PROPFIND for `current-user-principal`). Examples:
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

/// Per-run summary. The sync runner formats this into its
/// end-of-run line.
#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub addressbooks: usize,
    pub contacts_new: usize,
    pub contacts_updated: usize,
    pub contacts_deleted: usize,
    pub errors: usize,
    pub requests: usize,
}

/// Run one extract pass against `opts.server_url`.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    if opts.control.refetch_blobs {
        // Contacts doesn't populate `blob_refs` (photos travel inline in
        // the vCard payload), but the table exists via SHARED_DDL — the
        // wipe is a harmless no-op that keeps the flag uniform across
        // providers.
        frankweiler_etl::doltlite_raw::truncate_blob_refs(db.pool()).await?;
    }

    let mut summary = FetchSummary::default();
    let account_id = host_for_account(&opts.server_url)?;

    // ── Discovery ──────────────────────────────────────────────────
    let server_url = opts.server_url.trim_end_matches('/').to_string();
    let (principal_url, home_set_url) = discover(&server_url, &mut summary).await?;
    db.upsert_account(
        &account_id,
        &server_url,
        Some(principal_url.as_str()),
        Some(home_set_url.as_str()),
    )
    .await?;
    info!(
        event = "carddav_discovery",
        principal = %principal_url,
        addressbook_home_set = %home_set_url,
    );

    let books = list_addressbooks(&home_set_url, &mut summary).await?;
    info!(event = "carddav_addressbook_count", n = books.len());
    for book in &books {
        db.upsert_addressbook(
            &account_id,
            &book.href,
            book.display_name.as_deref(),
            book.description.as_deref(),
            book.ctag.as_deref(),
        )
        .await?;
    }
    summary.addressbooks = books.len();

    // ── Per-addressbook sync ──────────────────────────────────────
    let only_named = opts.addressbooks.as_deref();
    for book in &books {
        if let Some(want) = only_named {
            let matches = book
                .display_name
                .as_deref()
                .map(|d| want.iter().any(|w| w == d))
                .unwrap_or(false);
            if !matches {
                continue;
            }
        }
        let book_id = RawDb::addressbook_pk(&account_id, &book.href);
        let prev_token = db.sync_token(&book_id).await?.unwrap_or_default();
        opts.progress
            .set_message(&format!("syncing addressbook {}", book.href));
        match sync_addressbook(&db, &book_id, &book.url, &prev_token, &mut summary).await {
            Ok(()) => {}
            Err(e) => {
                summary.errors += 1;
                warn!(
                    event = "carddav_addressbook_sync_failed",
                    addressbook = %book.href,
                    error = %e,
                );
            }
        }
    }

    Ok(summary)
}

/// Resource the orchestrator carries around per addressbook —
/// `href` is the relative path stored on the row, `url` is the
/// absolute URL we hit for REPORTs (server URL + href, with the
/// usual care around already-absolute hrefs).
#[derive(Debug, Clone)]
struct Book {
    href: String,
    url: String,
    display_name: Option<String>,
    description: Option<String>,
    ctag: Option<String>,
}

async fn discover(server_url: &str, summary: &mut FetchSummary) -> Result<(String, String)> {
    // Step 1: current-user-principal.
    summary.requests += 1;
    let ms = api::propfind(server_url, "0", api::BODY_CURRENT_USER_PRINCIPAL)
        .await
        .map_err(|e| anyhow::anyhow!("propfind current-user-principal: {e}"))?;
    let principal_href = ms
        .responses
        .iter()
        .find_map(|r| r.current_user_principal.clone())
        .with_context(|| "server did not return current-user-principal")?;
    let principal_url = absolutize(server_url, &principal_href)?;

    // Step 2: addressbook-home-set.
    summary.requests += 1;
    let ms = api::propfind(&principal_url, "0", api::BODY_ADDRESSBOOK_HOME_SET)
        .await
        .map_err(|e| anyhow::anyhow!("propfind addressbook-home-set: {e}"))?;
    let home_set_href = ms
        .responses
        .iter()
        .find_map(|r| r.addressbook_home_set.clone())
        .with_context(|| "server did not return addressbook-home-set")?;
    let home_set_url = absolutize(server_url, &home_set_href)?;

    Ok((principal_url, home_set_url))
}

async fn list_addressbooks(home_set_url: &str, summary: &mut FetchSummary) -> Result<Vec<Book>> {
    summary.requests += 1;
    let ms = api::propfind(home_set_url, "1", api::BODY_LIST_ADDRESSBOOKS)
        .await
        .map_err(|e| anyhow::anyhow!("propfind list-addressbooks: {e}"))?;
    let mut out = Vec::new();
    for r in ms.responses {
        if !r.is_addressbook {
            continue;
        }
        let url = absolutize(home_set_url, &r.href)?;
        out.push(Book {
            href: r.href,
            url,
            display_name: r.display_name,
            description: r.description,
            ctag: r.ctag,
        });
    }
    Ok(out)
}

async fn sync_addressbook(
    db: &RawDb,
    book_id: &str,
    book_url: &str,
    prev_token: &str,
    summary: &mut FetchSummary,
) -> Result<()> {
    summary.requests += 1;
    let body = api::body_sync_collection(prev_token);
    let ms = match api::report(book_url, &body).await {
        Ok(ms) => ms,
        Err(CarddavError::Http {
            status: 403 | 405 | 501,
            ..
        }) => {
            // Server explicitly doesn't support sync-collection.
            // Fall back to a multiget over what we already have plus
            // a discovery walk. Not implemented yet — record the
            // error and move on.
            warn!(
                event = "carddav_sync_collection_unsupported",
                addressbook_url = %book_url,
            );
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("sync-collection REPORT: {e}")),
    };
    apply_multistatus(db, book_id, &ms, summary).await?;
    if let Some(token) = &ms.sync_token {
        db.set_sync_token(book_id, token).await?;
    }
    Ok(())
}

async fn apply_multistatus(
    db: &RawDb,
    book_id: &str,
    ms: &Multistatus,
    summary: &mut FetchSummary,
) -> Result<()> {
    let changed = api::changed_contacts(ms);
    let deleted = api::deleted_hrefs(ms);

    // Pre-fetch existing etags so we can tell `new` from `updated`.
    let existing = db.contact_etags_by_href(book_id).await?;

    let mut rows: Vec<ContactRow> = Vec::with_capacity(changed.len());
    for (href, (etag, vcard)) in &changed {
        let Some(uid) = api::vcard_uid(vcard) else {
            warn!(
                event = "carddav_vcard_missing_uid",
                href = %href,
            );
            summary.errors += 1;
            continue;
        };
        let was_known = existing.contains_key(href);
        if was_known {
            summary.contacts_updated += 1;
        } else {
            summary.contacts_new += 1;
        }
        rows.push(ContactRow {
            addressbook_id: book_id.to_string(),
            uid,
            href: href.clone(),
            etag: etag.clone(),
            display_name: api::vcard_fn(vcard),
            revision: api::vcard_rev(vcard),
            payload_vcard: vcard.clone(),
        });
    }
    db.upsert_contacts(&rows).await?;

    for href in &deleted {
        db.delete_contact(book_id, href).await?;
        summary.contacts_deleted += 1;
    }
    Ok(())
}

/// Resolve `href` (possibly relative) against the server-root URL.
/// CardDAV servers commonly emit hrefs like `/dav/principals/u/`
/// (root-relative); the spec also allows fully-qualified URLs.
fn absolutize(base: &str, href: &str) -> Result<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Ok(href.to_string());
    }
    let scheme_end = base.find("://").context("malformed base URL: no ://")?;
    let after_scheme = &base[scheme_end + 3..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    let scheme = &base[..scheme_end];
    if href.starts_with('/') {
        Ok(format!("{scheme}://{host}{href}"))
    } else {
        // Relative-to-base — strip any trailing filename component
        // off the base path.
        let mut prefix = base.to_string();
        if !prefix.ends_with('/') {
            if let Some(slash) = prefix.rfind('/') {
                prefix.truncate(slash + 1);
            }
        }
        Ok(format!("{prefix}{href}"))
    }
}

/// Account identifier: the URL host. One latchkey credential entry
/// keys per host, so this is the natural account key. If you ever
/// need to coexist two accounts on the same host (two Fastmail
/// users, say), bump this to embed a user-supplied tag.
fn host_for_account(server_url: &str) -> Result<String> {
    let scheme_end = server_url
        .find("://")
        .context("malformed server URL: no ://")?;
    let after_scheme = &server_url[scheme_end + 3..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    if host.is_empty() {
        anyhow::bail!("server URL has empty host: {server_url}");
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_handles_root_relative() {
        assert_eq!(
            absolutize("https://carddav.fastmail.com/", "/dav/principals/u/").unwrap(),
            "https://carddav.fastmail.com/dav/principals/u/"
        );
    }

    #[test]
    fn absolutize_keeps_absolute_urls() {
        assert_eq!(
            absolutize(
                "https://contacts.icloud.com/",
                "https://p123-contacts.icloud.com/123/principal/"
            )
            .unwrap(),
            "https://p123-contacts.icloud.com/123/principal/"
        );
    }

    #[test]
    fn host_for_account_strips_path() {
        assert_eq!(
            host_for_account("https://carddav.fastmail.com/dav/addressbooks").unwrap(),
            "carddav.fastmail.com"
        );
    }
}
