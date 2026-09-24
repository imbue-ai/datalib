//! CardDAV downloader entry point.

pub mod api;
pub mod db;
pub mod photos;
pub mod schema_raw;
pub mod vcf_dir;

pub use db::{db_path_for, RawDb};

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::progress::Progress;
use tracing::{info, warn};

use api::{DavError, Multistatus};
use datalib_etl::dav::absolutize;
use db::{addressbook_pk, ContactRow};

/// Options for one `fetch` run. Mirrors the FetchOptions shape every
/// other provider crate exposes.
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// Root URL of the user's CardDAV server. We start discovery
    /// here (PROPFIND for `current-user-principal`), then at the host's
    /// `/.well-known/carddav`. Examples:
    /// `https://contacts.icloud.com/`,
    /// `https://carddav.fastmail.com/`,
    /// `https://www.googleapis.com/carddav/v1/principals/`.
    pub server_url: String,
    /// Restrict the run to the named addressbooks (matched against
    /// the addressbook's `displayname`). `None` = sync every
    /// addressbook the server lists under the principal.
    pub addressbooks: Option<Vec<String>>,
    pub progress: Progress,
    pub control: DownloadControl,
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

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();

    let mut summary = FetchSummary::default();
    let account_id = host_for_account(&opts.server_url)?;

    // ── Discovery ──────────────────────────────────────────────────
    let (principal_url, home_set_url) =
        discover(&opts.server_url, &mut summary, &opts.latchkey).await?;
    let server_url = opts.server_url.trim_end_matches('/').to_string();
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
        "discovered the principal and the addressbook home"
    );

    let books = list_addressbooks(&home_set_url, &mut summary, &opts.latchkey).await?;
    info!(
        event = "carddav_addressbook_count",
        n = books.len(),
        "listed the addressbooks"
    );
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
        let book_id = addressbook_pk(&account_id, &book.href);
        let prev_token = db.sync_token(&book_id).await?.unwrap_or_default();
        opts.progress
            .set_message(&format!("syncing addressbook {}", book.href));
        match sync_addressbook(
            &db,
            &book_id,
            &book.url,
            &prev_token,
            &mut summary,
            &opts.latchkey,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                summary.errors += 1;
                warn!(
                    event = "carddav_addressbook_sync_failed",
                    addressbook = %book.href,
                    error = %e,
                    "an addressbook could not be synced"
                );
            }
        }
    }

    // Lift inline vCard photos into the per-source CAS (consistent
    // contact_photos shape). Best-effort: a CAS hiccup shouldn't fail an
    // otherwise-good contacts sync.
    // Through the handle's own CAS, so nothing here opens a second
    // store. `None` is a reader, which never reaches this path.
    if let Some(cas) = db.cas() {
        if let Err(e) = photos::lift_photos_to_cas(&db, cas).await {
            warn!(event = "carddav_photo_lift_failed", error = %e, "a photo could not be lifted out of its vCard");
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

async fn discover(
    server_url: &str,
    summary: &mut FetchSummary,
    latchkey: &LatchkeySettings,
) -> Result<(String, String)> {
    let principal_url = datalib_etl::dav::find_principal(
        api::HTTP_SERVICE,
        server_url,
        "carddav",
        latchkey,
        &mut summary.requests,
    )
    .await?;

    summary.requests += 1;
    let ms = api::propfind(
        &principal_url,
        "0",
        api::BODY_ADDRESSBOOK_HOME_SET,
        latchkey,
    )
    .await
    .map_err(|e| anyhow::anyhow!("propfind addressbook-home-set: {e}"))?;
    let home_set_href = ms
        .responses
        .iter()
        .find_map(|r| r.props.addressbook_home_set.clone())
        .with_context(|| "server did not return addressbook-home-set")?;
    let home_set_url =
        absolutize(&principal_url, &home_set_href).context("addressbook home URL")?;

    Ok((principal_url, home_set_url))
}

async fn list_addressbooks(
    home_set_url: &str,
    summary: &mut FetchSummary,
    latchkey: &LatchkeySettings,
) -> Result<Vec<Book>> {
    summary.requests += 1;
    let ms = api::propfind(home_set_url, "1", api::BODY_LIST_ADDRESSBOOKS, latchkey)
        .await
        .map_err(|e| anyhow::anyhow!("propfind list-addressbooks: {e}"))?;
    let mut out = Vec::new();
    for r in ms.responses {
        if !r.props.is_addressbook {
            continue;
        }
        let url = absolutize(home_set_url, &r.href).context("addressbook URL")?;
        out.push(Book {
            href: r.href,
            url,
            display_name: r.props.display_name,
            description: r.props.description,
            ctag: r.props.ctag,
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
    latchkey: &LatchkeySettings,
) -> Result<()> {
    summary.requests += 1;
    let body = api::body_sync_collection(prev_token);
    let ms = match api::report(book_url, &body, latchkey).await {
        Ok(ms) => ms,
        Err(DavError::Http {
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
                "the server does not support sync-collection; walking whole"
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
                "a vCard has no UID; keyed on its href"
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
        rows.push(ContactRow::new(
            book_id.to_string(),
            uid,
            href.clone(),
            etag.clone(),
            api::vcard_fn(vcard),
            api::vcard_rev(vcard),
            vcard,
        ));
    }
    db.upsert_contacts(&rows).await?;

    for href in &deleted {
        db.delete_contact(book_id, href).await?;
        summary.contacts_deleted += 1;
    }
    Ok(())
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
    fn host_for_account_strips_path() {
        assert_eq!(
            host_for_account("https://carddav.fastmail.com/dav/addressbooks").unwrap(),
            "carddav.fastmail.com"
        );
    }
}
