//! CardDAV downloader entry point: discovery, then each address book
//! kept in step through `datalib_etl_web::dav::sync`, which lists, stores
//! and prunes; this crate says how a card becomes a row.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod vcf_dir;

pub use db::{db_path_for, RawDb};

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::dav::sync::{self, CollectionSync, ObjectStore};
use datalib_etl_web::http::LatchkeySettings;
use sqlx::{Sqlite, Transaction};
use tracing::info;

use api::ContactProps;
use datalib_etl_web::dav::absolutize;
use db::{addressbook_pk, ContactRow};

/// Options for one `fetch` run. Mirrors the FetchOptions shape every
/// other provider crate exposes.
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Root URL of the user's CardDAV server. We start discovery
    /// here (PROPFIND for `current-user-principal`), then at the host's
    /// `/.well-known/carddav`. Examples:
    /// `https://contacts.icloud.com/`,
    /// `https://carddav.fastmail.com/`,
    /// `https://www.googleapis.com/carddav/v1/principals/`.
    pub server_url: String,
    /// Restrict the run to the named addressbooks (matched against
    /// the addressbook's `displayname`). Empty = sync every
    /// addressbook the server lists under the principal.
    pub addressbooks: Vec<String>,
    pub progress: Progress,
    pub control: DownloadControl,
    /// Seals as pages land, when the step driver hands one over.
    pub sealer: Option<Sealer>,
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
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        sync_account(opts, found)
    })
    .await
}

async fn sync_account(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();

    let mut summary = FetchSummary::default();
    let account_id = host_for_account(&opts.server_url)?;

    let Reached {
        principal_url,
        home_set_url,
        books,
    } = reach(&opts.server_url, &mut summary, &opts.latchkey).await?;
    let server_url = opts.server_url.trim_end_matches('/').to_string();
    db.upsert_account(
        &account_id,
        &server_url,
        Some(principal_url.as_str()),
        Some(home_set_url.as_str()),
    )
    .await?;
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
    let listed: Vec<String> = books
        .iter()
        .map(|b| addressbook_pk(&account_id, &b.href))
        .collect();
    summary.contacts_deleted += db.delete_addressbooks_not_in(&account_id, &listed).await?;

    report_unmatched_names(&found, &opts.addressbooks, &books)?;

    let run = sync::Run {
        pool: db.pool(),
        stop: &opts.control.stop,
        found: &found,
        sealer: opts.sealer.as_ref(),
    };
    for book in &books {
        let named = book
            .display_name
            .as_deref()
            .is_some_and(|d| opts.addressbooks.iter().any(|w| w == d));
        if !opts.addressbooks.is_empty() && !named {
            continue;
        }
        if opts.control.stop.requested() {
            break;
        }
        let book_id = addressbook_pk(&account_id, &book.href);
        let label = book.display_name.as_deref().unwrap_or(&book.href);
        opts.progress
            .set_message(&format!("syncing addressbook {}", book.href));
        let token = db.sync_token(&book_id).await?;
        let listing = CollectionSync::new(&api::KIND, &book.url, token, &opts.latchkey);
        let listing_name = format!("addressbook {label}");
        match sync::sync_collection(&run, &db, &book_id, label, listing).await {
            Ok(synced) => {
                summary.contacts_new += synced.new;
                summary.contacts_updated += synced.updated;
                summary.contacts_deleted += synced.deleted;
                summary.errors += synced.unusable + synced.failed;
                summary.requests += synced.requests;
                if let Some(cut_short) = synced.cut_short {
                    found.listing(
                        &listing_name,
                        format!(
                            "{cut_short}; nothing it has not reached is deleted until it finishes"
                        ),
                    );
                }
            }
            Err(e) => {
                summary.errors += 1;
                found.listing(&listing_name, format!("{e:#}"));
            }
        }
    }
    Ok(summary)
}

#[async_trait]
impl ObjectStore for RawDb {
    type Props = ContactProps;
    type Row = ContactRow;

    fn row(
        &self,
        collection: &str,
        href: &str,
        etag: Option<&str>,
        vcard: &str,
    ) -> std::result::Result<ContactRow, String> {
        let uid = api::vcard_uid(vcard)
            .ok_or_else(|| "the vCard has no UID, so it cannot be stored".to_string())?;
        Ok(ContactRow::new(
            collection.to_string(),
            uid,
            href.to_string(),
            etag.map(String::from),
            api::vcard_fn(vcard),
            api::vcard_rev(vcard),
            vcard,
        ))
    }

    async fn put(&self, tx: &mut Transaction<'_, Sqlite>, rows: &[&ContactRow]) -> Result<()> {
        RawDb::upsert_contacts_in_tx(tx, rows).await
    }

    async fn remove(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        href: &str,
    ) -> Result<u64> {
        RawDb::delete_contact(tx, collection, href).await
    }

    async fn set_token(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        token: Option<&str>,
    ) -> Result<()> {
        RawDb::set_sync_token(tx, collection, token).await
    }
}

/// A configured name no address book has is reported and costs only
/// itself — unless none matches, which fails the run rather than
/// falling back to every address book the filter was there to exclude.
fn report_unmatched_names(
    found: &RunProblems,
    configured: &[String],
    books: &[Book],
) -> Result<()> {
    let names = || {
        books
            .iter()
            .map(|b| b.display_name.as_deref().unwrap_or(&b.href))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let resolution = download_problems::resolve_configured("addressbooks", configured, |want| {
        books
            .iter()
            .any(|b| b.display_name.as_deref() == Some(want))
            .then_some(())
            .ok_or_else(|| format!("no address book has that name; the account has {}", names()))
    });
    let nothing_resolved = resolution.nothing_resolved();
    found.config(resolution.problems);
    if nothing_resolved {
        anyhow::bail!(
            "none of the configured addressbooks ({}) exists; the account has {}",
            configured.join(", "),
            names()
        );
    }
    Ok(())
}

/// Resource the orchestrator carries around per addressbook —
/// `href` is the relative path stored on the row, `url` is the
/// absolute URL we hit for REPORTs (server URL + href, with the
/// usual care around already-absolute hrefs).
#[derive(Debug, Clone)]
pub(crate) struct Book {
    href: String,
    url: String,
    pub(crate) display_name: Option<String>,
    description: Option<String>,
    ctag: Option<String>,
}

/// What discovery and the addressbook listing found: everything a run
/// needs before it syncs, and all a probe reports.
pub(crate) struct Reached {
    pub(crate) principal_url: String,
    home_set_url: String,
    pub(crate) books: Vec<Book>,
}

pub(crate) async fn reach(
    server_url: &str,
    summary: &mut FetchSummary,
    latchkey: &LatchkeySettings,
) -> Result<Reached> {
    let (principal_url, home_set_url) = discover(server_url, summary, latchkey).await?;
    info!(
        event = "carddav_discovery",
        principal = %principal_url,
        addressbook_home_set = %home_set_url,
        "discovered the principal and the addressbook home"
    );
    let books = list_addressbooks(&home_set_url, summary, latchkey).await?;
    info!(
        event = "carddav_addressbook_count",
        n = books.len(),
        "listed the addressbooks"
    );
    Ok(Reached {
        principal_url,
        home_set_url,
        books,
    })
}

async fn discover(
    server_url: &str,
    summary: &mut FetchSummary,
    latchkey: &LatchkeySettings,
) -> Result<(String, String)> {
    let principal_url = datalib_etl_web::dav::find_principal(
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
