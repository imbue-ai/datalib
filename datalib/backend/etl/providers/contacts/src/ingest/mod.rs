//! CardDAV downloader entry point.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod vcf_dir;

pub use db::{db_path_for, RawDb};

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::dav::state as dav_state;
use datalib_etl::dav::sync::{CollectionSync, Page};
use datalib_etl::download_problems;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use tracing::info;

use api::ContactProps;
use datalib_etl::dav::absolutize;
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
    run_problems::collecting(&pool, &stop, |found| sync_account(opts, found)).await
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

    report_unmatched_names(&found, &opts.addressbooks, &books)?;

    let mut synced_ids: Vec<String> = Vec::new();
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
        opts.progress
            .set_message(&format!("syncing addressbook {}", book.href));
        synced_ids.push(book_id.clone());
        let synced = sync_addressbook(&db, &book_id, &book.url, &mut summary, &opts.latchkey).await;
        let listing = format!(
            "addressbook {}",
            book.display_name.as_deref().unwrap_or(&book.href)
        );
        match synced {
            Ok(None) => {}
            Ok(Some(cut_short)) => found.listing(
                &listing,
                format!("{cut_short}; nothing it has not reached is deleted until it finishes"),
            ),
            Err(e) => {
                summary.errors += 1;
                found.listing(&listing, format!("{e:#}"));
            }
        }
    }
    dav_state::collect_unstored(db.pool(), &found, "contacts", &synced_ids).await;
    Ok(summary)
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

/// Keeps one address book in step with `sync-collection` from its
/// stored token. Returns why the listing stopped short, if it did.
async fn sync_addressbook(
    db: &RawDb,
    book_id: &str,
    book_url: &str,
    summary: &mut FetchSummary,
    latchkey: &LatchkeySettings,
) -> Result<Option<String>> {
    let token = db.sync_token(book_id).await?;
    let mut sync = CollectionSync::new(&api::KIND, book_url, token, latchkey);
    let stored = store_pages(db, book_id, &mut sync, summary).await;
    summary.requests += sync.requests();
    stored?;
    if let Some(cut_short) = sync.cut_short() {
        return Ok(Some(cut_short.to_string()));
    }
    for href in dav_state::finish_listing(db.pool(), book_id).await? {
        db.delete_contact(book_id, &href).await?;
        summary.contacts_deleted += 1;
    }
    Ok(None)
}

async fn store_pages(
    db: &RawDb,
    book_id: &str,
    sync: &mut CollectionSync<'_>,
    summary: &mut FetchSummary,
) -> Result<()> {
    while let Some(page) = sync.next_page::<ContactProps>().await? {
        if page.begins_whole {
            let stored = db.contact_hrefs(book_id).await?;
            dav_state::begin_whole(db.pool(), book_id, &stored).await?;
        }
        let token = page.token.clone();
        let (listed, deleted) = (page.listed.clone(), page.deleted.clone());
        let unstored = apply(db, book_id, page, summary).await?;
        dav_state::settle_page(db.pool(), book_id, &listed, &deleted, &unstored).await?;
        db.set_sync_token(book_id, token.as_deref()).await?;
    }
    Ok(())
}

/// Store what one page changed and drop what it deleted. Returns what
/// it named and could not store, with why.
async fn apply(
    db: &RawDb,
    book_id: &str,
    page: Page<ContactProps>,
    summary: &mut FetchSummary,
) -> Result<Vec<(String, String)>> {
    let existing = db.contact_etags_by_href(book_id).await?;
    let mut unstored: Vec<(String, String)> = page
        .unfetched
        .into_iter()
        .map(|href| {
            (
                href,
                "the address book listed this card, but did not return it when asked".to_string(),
            )
        })
        .collect();
    let mut rows: Vec<ContactRow> = Vec::with_capacity(page.changed.len());
    for r in &page.changed {
        let vcard = r.props.vcard.as_deref().unwrap_or_default();
        let Some(uid) = api::vcard_uid(vcard) else {
            unstored.push((
                r.href.clone(),
                "the vCard has no UID, so it cannot be stored".to_string(),
            ));
            continue;
        };
        if existing.contains_key(&r.href) {
            summary.contacts_updated += 1;
        } else {
            summary.contacts_new += 1;
        }
        rows.push(ContactRow::new(
            book_id.to_string(),
            uid,
            r.href.clone(),
            r.props.etag.clone(),
            api::vcard_fn(vcard),
            api::vcard_rev(vcard),
            vcard,
        ));
    }
    summary.errors += unstored.len();
    db.upsert_contacts(&rows).await?;
    for href in &page.deleted {
        db.delete_contact(book_id, href).await?;
        summary.contacts_deleted += 1;
    }
    Ok(unstored)
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
