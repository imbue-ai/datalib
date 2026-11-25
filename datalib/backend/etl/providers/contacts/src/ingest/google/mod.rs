//! Google Contacts download, through the People API (latchkey's
//! `google-people` service). The listing carries whole contacts, so each
//! page is one transaction and the sync token moves with the prune that
//! ends a listing; what is owed after it is each contact's photo
//! ([`photos`]). `INGEST.md` has what the API was measured to do.

pub mod db;
pub mod photos;

use std::collections::HashSet;

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::google::{ApiError, GoogleApi};
use datalib_etl_web::http::{percent_encode, HttpService, LatchkeySettings};
use serde_json::Value;
use tracing::warn;

use super::db::{addressbook_pk, contact_pk, AccountRow, RawDb};
use super::schema_raw::{GoogleContactGroupRow, GooglePersonRow, GooglePhotoRow};
use super::FetchSummary;
use datalib_etl::doltlite_raw::WirePayload;

pub const API: GoogleApi = GoogleApi {
    service: HttpService::GooglePeople,
    latchkey_service: "google-people",
};
pub const BASE: &str = "https://people.googleapis.com/v1";
pub const ACCOUNT_ID: &str = "google";
/// What Google's own page calls the list `people/me/connections` is.
pub const ADDRESSBOOK_NAME: &str = "Contacts";

/// Every field Google keeps on a contact. A sync token is only good for
/// the fields it was asked for with, so a change here needs a rung on the
/// ladder that clears `addressbooks.sync_token`.
pub const PERSON_FIELDS: &str = "addresses,ageRanges,biographies,birthdays,calendarUrls,\
    clientData,emailAddresses,events,externalIds,genders,imClients,interests,locales,locations,\
    memberships,metadata,miscKeywords,names,nicknames,occupations,organizations,phoneNumbers,\
    photos,relations,sipAddresses,skills,urls,userDefined";

pub struct FetchOptions {
    pub db: RawDb,
    pub blob_size_limit_bytes: Option<u64>,
    pub latchkey: LatchkeySettings,
    pub progress: Progress,
    pub control: DownloadControl,
    /// Seals as pages land, when the step driver hands one over.
    pub sealer: Option<Sealer>,
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
    let (db, lk) = (&opts.db, &opts.latchkey);
    let mut summary = FetchSummary::default();

    let me = me(lk, &mut summary).await?;
    let book = store_account(db, &me, &mut summary).await?;
    summary.addressbooks = 1;

    opts.progress.set_message("listing labels");
    match list_groups(lk, &mut summary).await {
        Ok(groups) => store_groups(db, &book, groups, &found).await?,
        Err(e) => {
            summary.errors += 1;
            found.listing("labels", format!("{e:#}"));
        }
    }
    if opts.control.stop.requested() {
        return Ok(summary);
    }

    opts.progress.set_message("syncing contacts");
    let listing = Listing {
        db,
        book: &book,
        lk,
        sealer: opts.sealer.as_ref(),
    };
    match listing.sync(&mut summary).await {
        Ok(None) => {}
        Ok(Some(held_back)) => found.listing("contacts", held_back),
        Err(e) => {
            summary.errors += 1;
            found.listing("contacts", format!("{e:#}"));
        }
    }
    photos::fetch(&opts, &book, &found, &mut summary).await?;
    Ok(summary)
}

/// The signed-in account.
pub(crate) struct Me {
    pub(crate) resource_name: String,
    pub(crate) email: Option<String>,
    pub(crate) name: Option<String>,
}

pub fn me_url() -> String {
    format!("{BASE}/people/me?personFields=names,emailAddresses")
}

pub(crate) async fn me(lk: &LatchkeySettings, summary: &mut FetchSummary) -> Result<Me> {
    summary.requests += 1;
    let v = API
        .get_json(&me_url(), lk)
        .await
        .context("read the account")?;
    Ok(Me {
        resource_name: str_of(&v, "resourceName")
            .context("the account reply carried no `resourceName`")?,
        email: primary(&v, "emailAddresses").and_then(|e| str_of(e, "value")),
        name: primary(&v, "names").and_then(|n| str_of(n, "displayName")),
    })
}

/// The account and its one address book. A store that held another
/// account's loses it: the login is not the one it mirrored.
async fn store_account(db: &RawDb, me: &Me, summary: &mut FetchSummary) -> Result<String> {
    db.upsert_account(&AccountRow {
        id: ACCOUNT_ID.into(),
        server_url: Some(BASE.into()),
        principal_href: Some(me.resource_name.clone()),
        addressbook_home_set: None,
        login: me.email.clone(),
    })
    .await?;
    let href = format!("{}/connections", me.resource_name);
    db.upsert_addressbook(ACCOUNT_ID, &href, Some(ADDRESSBOOK_NAME), None, None)
        .await?;
    let book = addressbook_pk(ACCOUNT_ID, &href);
    summary.contacts_deleted += db
        .delete_addressbooks_not_in(ACCOUNT_ID, std::slice::from_ref(&book))
        .await?;
    Ok(book)
}

pub fn groups_url(page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/contactGroups?pageSize=1000&groupFields=clientData,groupType,metadata,name"
    );
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", percent_encode(p)));
    }
    url
}

/// Every label, Google's own included. It is listed whole each run: one
/// request, and what it leaves out is gone.
async fn list_groups(lk: &LatchkeySettings, summary: &mut FetchSummary) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        summary.requests += 1;
        let v = API.get_json(&groups_url(page.as_deref()), lk).await?;
        // Google's own groups are always there, so a reply without any is
        // not a listing.
        let groups = v
            .get("contactGroups")
            .and_then(Value::as_array)
            .context("the labels reply carried no `contactGroups` list")?;
        out.extend(groups.iter().cloned());
        page = str_of(&v, "nextPageToken");
        if page.is_none() {
            return Ok(out);
        }
    }
}

async fn store_groups(
    db: &RawDb,
    book: &str,
    groups: Vec<Value>,
    found: &RunProblems,
) -> Result<()> {
    let rows: Vec<(GoogleContactGroupRow, Option<Value>)> =
        groups.iter().filter_map(|g| group_row(book, g)).collect();
    let unidentified = groups.len() - rows.len();
    db.store_google_groups(book, rows, unidentified == 0)
        .await?;
    if unidentified > 0 {
        found.listing(
            "labels",
            format!(
                "the listing named {unidentified} label(s) with no resource name, which could be \
                 any stored label, so none it did not name was deleted"
            ),
        );
    }
    Ok(())
}

pub fn connections_url(sync_token: Option<&str>, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/people/me/connections?pageSize=1000&requestSyncToken=true&personFields={PERSON_FIELDS}"
    );
    if let Some(t) = sync_token {
        url.push_str(&format!("&syncToken={}", percent_encode(t)));
    }
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", percent_encode(p)));
    }
    url
}

struct Listing<'a> {
    db: &'a RawDb,
    book: &'a str,
    lk: &'a LatchkeySettings,
    sealer: Option<&'a Sealer>,
}

impl Listing<'_> {
    /// The changes since the sync token, or every contact when there is
    /// none. The reason nothing was deleted and the token was left, when
    /// something was listed that could be any stored contact.
    async fn sync(&self, summary: &mut FetchSummary) -> Result<Option<String>> {
        let token = self.db.sync_token(self.book).await?;
        match self.list(token.as_deref(), summary).await {
            Err(e) if token.is_some() && is_expired(&e) => {
                warn!(
                    event = "google_contacts_sync_token_expired",
                    "Google expired the sync token; listing every contact"
                );
                let mut tx = self.db.pool().begin().await.context("begin")?;
                RawDb::set_sync_token(&mut tx, self.book, None).await?;
                tx.commit().await.context("commit")?;
                self.list(None, summary).await
            }
            other => other,
        }
    }

    async fn list(
        &self,
        token: Option<&str>,
        summary: &mut FetchSummary,
    ) -> Result<Option<String>> {
        let mut listed: HashSet<String> = HashSet::new();
        let mut unidentified = 0;
        let mut page: Option<String> = None;
        loop {
            summary.requests += 1;
            let url = connections_url(token, page.as_deref());
            let v = API.get_json(&url, self.lk).await?;
            page = str_of(&v, "nextPageToken");
            let next_token = str_of(&v, "nextSyncToken");
            // Google leaves `connections` out of a page with nobody on it,
            // so the tokens are what say a reply is a page.
            if page.is_none() && next_token.is_none() {
                anyhow::bail!("the contacts reply carried neither a next page nor a sync token");
            }
            let connections = v.get("connections").and_then(Value::as_array);
            let answer = read_page(self.book, connections.into_iter().flatten());
            unidentified += answer.unidentified;
            summary.errors += answer.unidentified;
            listed.extend(answer.rows.iter().map(|(r, _)| r.id_and_payload.id.clone()));

            let mut tx = self.db.pool().begin().await.context("begin a page")?;
            let mut wrote = RawDb::store_google_page(&mut tx, &answer, summary).await?;
            let held_back = held_back(unidentified);
            if let Some(next_token) = next_token.filter(|_| page.is_none()) {
                if token.is_none() && held_back.is_none() {
                    let gone = RawDb::prune_google_people(&mut tx, self.book, &listed).await?;
                    summary.contacts_deleted += gone;
                    wrote += gone as u64;
                }
                if held_back.is_none() {
                    RawDb::set_sync_token(&mut tx, self.book, Some(&next_token)).await?;
                }
            }
            tx.commit().await.context("commit a page")?;
            if let Some(sealer) = self.sealer {
                sealer.wrote(wrote).await;
            }
            if page.is_none() {
                return Ok(held_back);
            }
        }
    }
}

fn held_back(unidentified: usize) -> Option<String> {
    (unidentified > 0).then(|| {
        format!(
            "the listing named {unidentified} contact(s) with no resource name, which were not \
             stored and could be any stored contact, so nothing it did not name was deleted; the \
             next run asks again"
        )
    })
}

/// One page of the listing, read.
#[derive(Default)]
pub struct Page {
    pub rows: Vec<(GooglePersonRow, Option<Value>)>,
    /// The photo of each contact that has one.
    pub photos: Vec<GooglePhotoRow>,
    /// Keys of contacts deleted, and the former names of ones renamed.
    pub gone: Vec<String>,
    /// Contacts with no resource name, which no stored row can be matched to.
    pub unidentified: usize,
}

fn read_page<'a>(book: &str, people: impl Iterator<Item = &'a Value>) -> Page {
    let mut page = Page::default();
    for person in people {
        let Some(name) = str_of(person, "resourceName") else {
            page.unidentified += 1;
            continue;
        };
        let metadata = person.get("metadata");
        let deleted = metadata
            .and_then(|m| m.get("deleted"))
            .and_then(Value::as_bool);
        if deleted == Some(true) {
            page.gone.push(contact_pk(book, &name));
            continue;
        }
        let former = metadata.and_then(|m| m.get("previousResourceNames"));
        page.gone.extend(
            former
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|former| *former != name)
                .map(|former| contact_pk(book, former)),
        );
        page.photos
            .extend(photos::row(&contact_pk(book, &name), person));
        page.rows.push(person_row(book, &name, person));
    }
    page
}

fn person_row(book: &str, name: &str, person: &Value) -> (GooglePersonRow, Option<Value>) {
    let mut person = person.clone();
    sort_memberships(&mut person);
    let (content, volatile) = datalib_etl::doltlite_raw::split_volatile(
        &person,
        super::schema_raw::GOOGLE_PERSON_VOLATILE_PATHS,
    );
    let row = GooglePersonRow {
        id_and_payload: WirePayload {
            id: contact_pk(book, name),
            payload: content.to_string(),
        },
        addressbook_id: book.to_string(),
        resource_name: name.to_string(),
        display_name: primary(&content, "names").and_then(|n| str_of(n, "displayName")),
    };
    (row, volatile)
}

/// A contact's labels are a set sent as a list.
fn sort_memberships(person: &mut Value) {
    if let Some(memberships) = person.get_mut("memberships").and_then(Value::as_array_mut) {
        memberships.sort_by_cached_key(Value::to_string);
    }
}

fn group_row(book: &str, group: &Value) -> Option<(GoogleContactGroupRow, Option<Value>)> {
    let name = str_of(group, "resourceName")?;
    let (content, volatile) = datalib_etl::doltlite_raw::split_volatile(
        group,
        super::schema_raw::GOOGLE_GROUP_VOLATILE_PATHS,
    );
    let row = GoogleContactGroupRow {
        id_and_payload: WirePayload {
            id: contact_pk(book, &name),
            payload: content.to_string(),
        },
        addressbook_id: book.to_string(),
        resource_name: name,
        name: str_of(&content, "formattedName").or_else(|| str_of(&content, "name")),
    };
    Some((row, volatile))
}

fn is_expired(e: &anyhow::Error) -> bool {
    ApiError::in_chain(e).is_some_and(|e| e.reason.as_deref() == Some("EXPIRED_SYNC_TOKEN"))
}

/// The contact's page on contacts.google.com, as the account that has it.
pub fn person_page(resource_name: &str, login: Option<&str>) -> Option<String> {
    let id = resource_name.strip_prefix("people/")?;
    let url = format!("https://contacts.google.com/person/{}", percent_encode(id));
    Some(match login {
        Some(login) => format!("{url}?authuser={}", percent_encode(login)),
        None => url,
    })
}

/// The item of a person's `field` Google marks primary, else its first.
pub fn primary<'a>(person: &'a Value, field: &str) -> Option<&'a Value> {
    let items = person.get(field)?.as_array()?;
    items
        .iter()
        .find(|i| i.pointer("/metadata/primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| items.first())
}

pub fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
