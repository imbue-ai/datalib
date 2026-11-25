//! The Google Contacts download on fake TNG data, in the shapes `INGEST.md`
//! says the People API answers with.

use std::path::{Path, PathBuf};

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_contacts::ingest::google::{
    self, connections_url, groups_url, me_url, photos::sized,
};
use datalib_etl_contacts::ingest::{db_path_for, FetchSummary, RawDb};
use datalib_etl_contacts_config::ContactsConfig;
use datalib_etl_web::http::{HttpRequest, HttpResponse, LatchkeySettings};
use datalib_etl_web::playback::{self, Playback};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_probe::ProbeAsk;
use serde_json::{json, Value};

pub(crate) const ME: &str = "people/1701";
pub(crate) const BOOK: &str = "google!people/1701/connections";
const LOGIN: &str = "picard@enterprise.test";
/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
];

pub(crate) fn fixture(root: &Path, url: &str, resp: HttpResponse) {
    let req = HttpRequest::get(google::API.service, url).header("Accept", "application/json");
    write_fixture(root, &req, &resp).expect("write fixture");
}

/// A photo's link, as Google lists it and as the download asks for it. The
/// bytes are the link's own.
pub(crate) fn photo(root: &Path, url: &str, status: u16, content_type: &str) {
    photo_of(root, url, url, status, content_type);
}

fn photo_of(root: &Path, url: &str, bytes: &str, status: u16, content_type: &str) {
    let req = HttpRequest::get(google::API.service, sized(url)).plain();
    let mut resp = HttpResponse {
        status,
        headers: Default::default(),
        body: [PNG, bytes.as_bytes()].concat(),
        duration_ms: 0,
    };
    resp.headers
        .insert("content-type".into(), content_type.into());
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub(crate) fn account(root: &Path, resource_name: &str) {
    fixture(
        root,
        &me_url(),
        json_response(&json!({
            "resourceName": resource_name, "etag": "%Eg==",
            "names": [{"metadata": {"primary": true}, "displayName": "Jean-Luc Picard"}],
            "emailAddresses": [{"metadata": {"primary": true}, "value": LOGIN}]
        })),
    );
}

pub(crate) fn group(id: &str, name: &str, user: bool) -> Value {
    json!({
        "resourceName": format!("contactGroups/{id}"), "etag": "e1",
        "metadata": {"updateTime": "2026-09-01T10:00:00Z"},
        "groupType": if user { "USER_CONTACT_GROUP" } else { "SYSTEM_CONTACT_GROUP" },
        "name": id, "formattedName": name
    })
}

pub(crate) fn groups(root: &Path, labels: Value) {
    let mut all = vec![
        group("myContacts", "My Contacts", false),
        group("starred", "Starred", false),
    ];
    all.extend(labels.as_array().unwrap().iter().cloned());
    fixture(
        root,
        &groups_url(None),
        json_response(&json!({"contactGroups": all, "totalItems": all.len()})),
    );
}

pub(crate) fn person(id: &str, name: &str, photo: Option<&str>, labels: &[&str]) -> Value {
    let mut p = json!({
        "resourceName": format!("people/{id}"), "etag": "%EgUBAg==",
        "metadata": {"objectType": "PERSON", "sources": [
            {"type": "CONTACT", "id": id, "etag": "#1", "updateTime": "2026-09-01T10:00:00.000001Z"}
        ]},
        "names": [{"metadata": {"primary": true}, "displayName": name}],
        "memberships": labels.iter().map(|l| json!({
            "metadata": {"source": {"type": "CONTACT", "id": id}},
            "contactGroupMembership": {"contactGroupId": l, "contactGroupResourceName": format!("contactGroups/{l}")}
        })).collect::<Vec<_>>()
    });
    let mut letter = json!({"metadata": {"source": {"type": "CONTACT", "id": id}}, "default": true,
        "url": "https://lh3.googleusercontent.com/cm/letter=s100"});
    p["photos"] = match photo {
        Some(url) => json!([
            {"metadata": {"primary": true, "source": {"type": "PROFILE", "id": format!("1{id}")}},
             "url": url},
            letter
        ]),
        None => {
            letter["metadata"]["primary"] = json!(true);
            json!([letter])
        }
    };
    p
}

pub(crate) fn deleted(id: &str) -> Value {
    json!({"resourceName": format!("people/{id}"), "etag": "%Eg==",
        "metadata": {"deleted": true, "objectType": "PERSON"}})
}

/// A page of the listing; with nobody on it, Google leaves `connections`
/// out.
pub(crate) fn page(people: Vec<Value>, next: Option<&str>, sync: Option<&str>) -> HttpResponse {
    let mut v = json!({"totalItems": 4});
    if !people.is_empty() {
        v["connections"] = json!(people);
    }
    if let Some(p) = next {
        v["nextPageToken"] = json!(p);
    }
    if let Some(s) = sync {
        v["nextSyncToken"] = json!(s);
    }
    json_response(&v)
}

pub(crate) fn link(who: &str) -> String {
    format!("https://lh3.googleusercontent.com/contacts/{who}=s100")
}

pub(crate) async fn download(
    db: &RawDb,
    playback: impl Into<Playback>,
    control: datalib_etl::control::DownloadControl,
) -> anyhow::Result<FetchSummary> {
    let download = google::fetch(google::FetchOptions {
        db: db.clone(),
        blob_size_limit_bytes: None,
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control,
        sealer: None,
    });
    playback::scope(playback, download).await
}

async fn try_run(playback: impl Into<Playback>, store: &Path) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let summary = download(&db, playback, Default::default()).await;
    if summary.is_ok() {
        db.commit_all("test").await.expect("commit");
    }
    db.close().await;
    summary
}

async fn run(playback: &Path, store: &Path) -> FetchSummary {
    try_run(playback, store)
        .await
        .expect("google fetch under playback")
}

async fn strings(store: &Path, sql: &'static str) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let v = sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    v
}

const PEOPLE: &str = "SELECT resource_name FROM google_people ORDER BY resource_name";
const LABELS: &str = "SELECT resource_name FROM google_contact_groups ORDER BY resource_name";
const PHOTOS: &str = "SELECT b.held_version FROM google_photos p \
     JOIN google_photos_bookkeeping b ON b.id = p.id WHERE p.blake3 IS NOT NULL ORDER BY p.id";
const PROBLEMS: &str = "SELECT scope_key FROM problems ORDER BY scope_key";
const TOKEN: &str = "SELECT coalesce(sync_token, 'none') FROM addressbooks";

fn dirs(d: &tempfile::TempDir, n: usize) -> (Vec<PathBuf>, PathBuf) {
    let tapes: Vec<PathBuf> = (0..n).map(|i| d.path().join(format!("run{i}"))).collect();
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    for t in &tapes {
        account(t, ME);
        groups(t, json!([]));
    }
    (tapes, store)
}

/// A paged first listing stores everyone, their labels and their photos;
/// the next asks only for what changed, and applies a deletion, an edit
/// with a new photo, a contact renamed by Google, and a label that went.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_listing_then_only_what_changed() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 3);
    groups(
        &tapes[0],
        json!([
            group("bridge", "Bridge crew", true),
            group("old", "Old crew", true)
        ]),
    );
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(
            vec![
                person(
                    "c1",
                    "William Riker",
                    Some(&link("riker-1")),
                    &["starred", "bridge", "myContacts"],
                ),
                person("c2", "Data", None, &["bridge", "myContacts"]),
            ],
            Some("p2"),
            None,
        ),
    );
    fixture(
        &tapes[0],
        &connections_url(None, Some("p2")),
        page(
            vec![
                person("c3", "Tasha Yar", Some(&link("yar-1")), &["myContacts"]),
                person("c4", "Wesley Crusher", None, &["myContacts"]),
            ],
            None,
            Some("s1"),
        ),
    );
    photo(&tapes[0], &link("riker-1"), 200, "image/png");
    photo(&tapes[0], &link("yar-1"), 200, "image/png");

    groups(&tapes[1], json!([group("bridge", "Bridge crew", true)]));
    let mut wesley = person("1999", "Wesley Crusher", None, &["myContacts"]);
    wesley["metadata"]["previousResourceNames"] = json!(["people/c4"]);
    fixture(
        &tapes[1],
        &connections_url(Some("s1"), None),
        page(
            vec![
                person(
                    "c1",
                    "William T. Riker",
                    Some(&link("riker-2")),
                    &["bridge", "myContacts"],
                ),
                deleted("c3"),
                wesley,
            ],
            None,
            Some("s2"),
        ),
    );
    photo(&tapes[1], &link("riker-2"), 200, "image/png");

    groups(&tapes[2], json!([group("bridge", "Bridge crew", true)]));
    fixture(
        &tapes[2],
        &connections_url(Some("s2"), None),
        page(vec![], None, Some("s3")),
    );

    let config: ContactsConfig = serde_json::from_value(json!({"google": {}})).unwrap();
    let probe = datalib_etl_contacts::probe::probe(&config, ProbeAsk::Account);
    let report = playback::scope(&tapes[0], probe).await.unwrap();
    assert_eq!(report.account.address.as_deref(), Some(LOGIN));
    assert_eq!(
        report.account.display_name.as_deref(),
        Some("Jean-Luc Picard")
    );

    let first = run(&tapes[0], &store).await;
    assert_eq!(
        (
            first.contacts_new,
            first.photos,
            first.errors,
            first.requests
        ),
        (4, 2, 0, 6),
        "{first:?}"
    );
    assert_eq!(
        strings(&store, PEOPLE).await,
        ["people/c1", "people/c2", "people/c3", "people/c4"]
    );
    assert_eq!(
        strings(&store, PHOTOS).await,
        [link("riker-1"), link("yar-1")]
    );
    assert_eq!(
        strings(&store, LABELS).await,
        [
            "contactGroups/bridge",
            "contactGroups/myContacts",
            "contactGroups/old",
            "contactGroups/starred"
        ]
    );
    assert_eq!(
        strings(&store, "SELECT DISTINCT typeof(payload) FROM google_people").await,
        ["blob"]
    );

    let second = run(&tapes[1], &store).await;
    assert_eq!(
        (
            second.contacts_new,
            second.contacts_updated,
            second.contacts_deleted,
            second.photos
        ),
        (1, 1, 2, 1),
        "{second:?}"
    );
    assert_eq!(
        strings(&store, PEOPLE).await,
        ["people/1999", "people/c1", "people/c2"]
    );
    assert_eq!(strings(&store, PHOTOS).await, [link("riker-2")]);
    assert_eq!(
        strings(&store, LABELS).await,
        [
            "contactGroups/bridge",
            "contactGroups/myContacts",
            "contactGroups/starred"
        ]
    );

    let third = run(&tapes[2], &store).await;
    assert_eq!(
        (
            third.contacts_new,
            third.contacts_updated,
            third.contacts_deleted,
            third.requests
        ),
        (0, 0, 0, 3),
        "nothing changed costs the account, the labels and one page: {third:?}"
    );
    assert_eq!(strings(&store, TOKEN).await, ["s3"]);
    assert_eq!(strings(&store, PROBLEMS).await, Vec::<String>::new());
}

/// An expired token is listed whole, and what the listing no longer names
/// goes with its photo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_lists_everyone_again() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 2);
    let riker = person("c1", "William Riker", None, &["myContacts"]);
    let yar = person("c3", "Tasha Yar", Some(&link("yar-1")), &["myContacts"]);
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(vec![riker.clone(), yar], None, Some("s1")),
    );
    photo(&tapes[0], &link("yar-1"), 200, "image/png");
    let mut expired = json_response(&json!({"error": {"code": 400,
        "message": "Sync token is expired. Clear local cache and retry call without the sync token.",
        "status": "FAILED_PRECONDITION",
        "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": "EXPIRED_SYNC_TOKEN", "domain": "people.googleapis.com"}]}}));
    expired.status = 400;
    fixture(&tapes[1], &connections_url(Some("s1"), None), expired);
    fixture(
        &tapes[1],
        &connections_url(None, None),
        page(vec![riker], None, Some("s9")),
    );

    run(&tapes[0], &store).await;
    let second = run(&tapes[1], &store).await;
    assert_eq!(
        (second.contacts_deleted, second.errors),
        (1, 0),
        "{second:?}"
    );
    assert_eq!(strings(&store, PEOPLE).await, ["people/c1"]);
    assert_eq!(
        strings(&store, "SELECT id FROM google_photos_bookkeeping").await,
        Vec::<String>::new()
    );
    assert_eq!(strings(&store, TOKEN).await, ["s9"]);
}

/// A whole listing may delete what it does not name only if it could read
/// everything it listed, and a reply with neither a next page nor a sync
/// token is not a page at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_it_cannot_fully_read_deletes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 3);
    let riker = person("c1", "William Riker", None, &["myContacts"]);
    let mut nameless = person("c2", "Data", None, &["myContacts"]);
    nameless.as_object_mut().unwrap().remove("resourceName");
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(vec![riker.clone(), nameless.clone()], None, Some("s1")),
    );
    fixture(
        &tapes[1],
        &connections_url(None, None),
        page(vec![nameless], None, Some("s2")),
    );
    fixture(
        &tapes[2],
        &connections_url(None, None),
        json_response(&json!({})),
    );

    for (n, tape) in tapes.iter().enumerate() {
        run(tape, &store).await;
        assert_eq!(strings(&store, PEOPLE).await, ["people/c1"], "run {n}");
        assert_eq!(strings(&store, TOKEN).await, ["none"], "run {n}");
        assert_eq!(
            strings(&store, PROBLEMS).await,
            ["listing:contacts"],
            "run {n}"
        );
    }
}

/// Google re-stamps a contact nobody edited, signs its photo's link
/// afresh, and sends its labels in another order: no row render reads
/// changes, so nothing renders again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restamped_contact_is_not_an_update() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 2);
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(
            vec![person(
                "c1",
                "William Riker",
                Some(&link("signed-1")),
                &["starred", "myContacts"],
            )],
            None,
            Some("s1"),
        ),
    );
    photo_of(
        &tapes[0],
        &link("signed-1"),
        "riker's face",
        200,
        "image/png",
    );
    let mut touched = person(
        "c1",
        "William Riker",
        Some(&link("signed-2")),
        &["myContacts", "starred"],
    );
    photo_of(
        &tapes[1],
        &link("signed-2"),
        "riker's face",
        200,
        "image/png",
    );
    touched["etag"] = json!("%New==");
    touched["metadata"]["sources"][0]["etag"] = json!("#2");
    touched["metadata"]["sources"][0]["updateTime"] = json!("2026-10-01T10:00:00Z");
    fixture(
        &tapes[1],
        &connections_url(Some("s1"), None),
        page(vec![touched], None, Some("s2")),
    );
    const RENDERED: &str = "SELECT id || json(payload) || addressbook_id || resource_name \
         || display_name FROM google_people \
         UNION ALL SELECT id || person_id || photo_source || blake3 FROM google_photos";
    run(&tapes[0], &store).await;
    let before = strings(&store, RENDERED).await;
    assert_eq!(before.len(), 2, "{before:?}");
    let second = run(&tapes[1], &store).await;
    assert_eq!(strings(&store, RENDERED).await, before);
    assert_eq!(second.contacts_updated, 0, "{second:?}");
    assert_eq!(strings(&store, PHOTOS).await, [link("signed-2")]);

    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let loaded = db.load_google_people().await.unwrap();
    db.close().await;
    assert_eq!(
        loaded[0].person["metadata"]["sources"][0]["updateTime"],
        "2026-10-01T10:00:00Z"
    );
}

/// A photo that will not come is a problem on its own row and owed again;
/// the contact is stored without it, and the next run, with nothing new
/// listed, fetches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_photo_that_failed_is_asked_for_again() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 2);
    let elsewhere = "https://evil.example/q.jpg=s100";
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(
            vec![
                person(
                    "c1",
                    "William Riker",
                    Some(&link("riker-1")),
                    &["myContacts"],
                ),
                person("c2", "Q", Some(elsewhere), &["myContacts"]),
                person("c3", "Data", Some(&link("data-1")), &["myContacts"]),
            ],
            None,
            Some("s1"),
        ),
    );
    photo(&tapes[0], &link("riker-1"), 404, "text/html");
    photo(&tapes[0], &link("data-1"), 200, "text/html");
    fixture(
        &tapes[1],
        &connections_url(Some("s1"), None),
        page(vec![], None, Some("s2")),
    );
    photo(&tapes[1], &link("riker-1"), 200, "image/png");
    photo(&tapes[1], &link("data-1"), 200, "image/png");

    let first = run(&tapes[0], &store).await;
    assert_eq!(
        (first.contacts_new, first.photos, first.errors),
        (3, 0, 3),
        "{first:?}"
    );
    let key = |id: &str| format!("google_photos:{BOOK}#people/{id}#PROFILE/1{id}");
    assert_eq!(
        strings(&store, PROBLEMS).await,
        [key("c1"), key("c2"), key("c3")]
    );

    let asked = playback::Asked::default();
    let second = try_run(Playback::at(&tapes[1]).asked(asked.clone()), &store)
        .await
        .unwrap();
    assert_eq!(second.photos, 2, "{second:?}");
    assert_eq!(
        asked.count("evil.example"),
        0,
        "only Google's own host is asked"
    );
    assert_eq!(strings(&store, PROBLEMS).await, [key("c2")]);
}

/// A login for another account loses the contacts of the one before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_account_replaces_the_one_before() {
    let d = tempfile::tempdir().unwrap();
    let (tapes, store) = dirs(&d, 2);
    fixture(
        &tapes[0],
        &connections_url(None, None),
        page(
            vec![person("c1", "William Riker", None, &["myContacts"])],
            None,
            Some("s1"),
        ),
    );
    account(&tapes[1], "people/74656");
    fixture(
        &tapes[1],
        &connections_url(None, None),
        page(
            vec![person("c1", "Tuvok", None, &["myContacts"])],
            None,
            Some("v1"),
        ),
    );
    run(&tapes[0], &store).await;
    let second = run(&tapes[1], &store).await;
    assert_eq!(
        (second.contacts_new, second.contacts_deleted),
        (1, 1),
        "{second:?}"
    );
    assert_eq!(
        strings(&store, "SELECT display_name FROM google_people").await,
        ["Tuvok"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT DISTINCT addressbook_id FROM google_contact_groups"
        )
        .await,
        ["google!people/74656/connections"]
    );
}
