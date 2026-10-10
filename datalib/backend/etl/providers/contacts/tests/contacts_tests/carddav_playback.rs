//! A CardDAV account shaped like Fastmail's, measured live on
//! 2026-09-24: the bare host answers 404, `/.well-known/carddav`
//! redirects to `/dav/addressbooks`, every value is CDATA, each
//! collection's missing properties come back in a 404 propstat, and the
//! cards are the `Bridge.vcf` fixture's own, CRLF and folded as served.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_contacts::ingest::{self, api, db_path_for, RawDb};
use datalib_etl_web::dav;
use datalib_etl_web::http::{HttpMethod, HttpResponse, LatchkeySettings};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::write_fixture;

pub(crate) const HOST: &str = "https://carddav.enterprise.test";
pub(crate) const PRINCIPAL: &str = "/dav/principals/user/picard@enterprise.test/";
pub(crate) const HOME: &str = "/dav/addressbooks/user/picard@enterprise.test/";
pub(crate) const BOOK: &str = "/dav/addressbooks/user/picard@enterprise.test/Default/";

pub(crate) const BRIDGE_V1: &str = include_str!("../fixtures/carddav_tng/Bridge.vcf");
pub(crate) const BRIDGE_V2: &str = include_str!("../fixtures/carddav_tng_v2/Bridge.vcf");

/// `(UID, the card as served)` for each card of a `.vcf`.
pub(crate) fn cards(vcf: &str) -> Vec<(String, String)> {
    vcf.split_inclusive("END:VCARD\r\n")
        .map(|card| {
            let uid = api::vcard_uid(card).expect("every Bridge card has a UID");
            (uid, card.to_string())
        })
        .collect()
}

pub(crate) fn card<'a>(cards: &'a [(String, String)], uid: &str) -> &'a str {
    &cards.iter().find(|(u, _)| u == uid).expect(uid).1
}

pub(crate) fn xml(status: u16, body: &str) -> HttpResponse {
    let mut headers = BTreeMap::new();
    headers.insert(
        "content-type".into(),
        "application/xml; charset=utf-8".into(),
    );
    HttpResponse {
        status,
        headers,
        body: body.as_bytes().to_vec(),
        duration_ms: 0,
    }
}

pub(crate) fn fixture(
    root: &Path,
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    resp: HttpResponse,
) {
    let req = dav::http_request(
        api::HTTP_SERVICE,
        method,
        url,
        depth,
        body,
        &LatchkeySettings::default(),
    );
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub(crate) fn resource(uid: &str, etag: &str, vcard: &str) -> String {
    format!(
        "<response><href>{BOOK}{uid}.vcf</href><propstat><prop><getetag>{etag}</getetag>\
         <card:address-data><![CDATA[{vcard}]]></card:address-data></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

pub(crate) fn multistatus(inner: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">{inner}</multistatus>"#
    )
}

/// Discovery and the addressbook listing, shared by both runs.
pub(crate) fn account_fixtures(root: &Path) {
    account_fixtures_listing(root, true);
}

/// The same with the home listing holding no address book at all.
fn account_fixtures_listing(root: &Path, with_bridge: bool) {
    let find_principal = dav::BODY_CURRENT_USER_PRINCIPAL;
    fixture(
        root,
        HttpMethod::Propfind,
        &format!("{HOST}/"),
        "0",
        find_principal,
        xml(
            404,
            "<html><head><title>404 Not Found</title></head></html>",
        ),
    );
    let mut moved = xml(301, "");
    moved
        .headers
        .insert("location".into(), format!("{HOST}/dav/addressbooks"));
    fixture(
        root,
        HttpMethod::Propfind,
        &format!("{HOST}/.well-known/carddav"),
        "0",
        find_principal,
        moved,
    );
    fixture(root, HttpMethod::Propfind, &format!("{HOST}/dav/addressbooks"), "0", find_principal,
        xml(207, &multistatus(&format!(
            "<response><href>/dav/addressbooks/</href><propstat><prop><current-user-principal><href>{PRINCIPAL}</href></current-user-principal></prop><status>HTTP/1.1 200 OK</status></propstat></response>"
        ))));
    fixture(
        root,
        HttpMethod::Propfind,
        &format!("{HOST}{PRINCIPAL}"),
        "0",
        api::BODY_ADDRESSBOOK_HOME_SET,
        xml(
            207,
            &multistatus(&format!(
                "<response><href>{PRINCIPAL}</href><propstat><prop>\
             <card:addressbook-home-set><href>{HOME}</href></card:addressbook-home-set>\
             </prop><status>HTTP/1.1 200 OK</status></propstat></response>"
            )),
        ),
    );
    let bridge = format!(
        "<response><href>{BOOK}</href>\
         <propstat><prop><resourcetype><collection/><card:addressbook/></resourcetype><displayname><![CDATA[Bridge]]></displayname><cs:getctag>2370-1</cs:getctag></prop><status>HTTP/1.1 200 OK</status></propstat>\
         <propstat><prop><card:addressbook-description/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>"
    );
    fixture(root, HttpMethod::Propfind, &format!("{HOST}{HOME}"), "1", api::BODY_LIST_ADDRESSBOOKS,
        xml(207, &multistatus(&format!(
            "<response><href>{HOME}</href>\
             <propstat><prop><resourcetype><collection/></resourcetype><displayname><![CDATA[#addressbooks]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat>\
             <propstat><prop><card:addressbook-description/><cs:getctag/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>{}",
            if with_bridge { bridge.as_str() } else { "" }
        ))));
}

async fn run(playback: &Path, store: &Path) -> ingest::FetchSummary {
    run_with(playback, store, Default::default()).await
}

/// A run asked to stop before it syncs any address book.
async fn run_stopped(playback: &Path, store: &Path) -> ingest::FetchSummary {
    let control = datalib_etl::control::DownloadControl::default();
    control.stop.request();
    run_with(playback, store, control).await
}

async fn run_with(
    playback: &Path,
    store: &Path,
    control: datalib_etl::control::DownloadControl,
) -> ingest::FetchSummary {
    run_named(playback, store, control, &["Bridge"])
        .await
        .expect("carddav fetch under playback")
}

/// Commits only a fetch that returned `Ok`, as the processor does.
async fn run_named(
    playback: &Path,
    store: &Path,
    control: datalib_etl::control::DownloadControl,
    addressbooks: &[&str],
) -> anyhow::Result<ingest::FetchSummary> {
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let download = ingest::fetch(ingest::FetchOptions {
        latchkey: LatchkeySettings::default(),
        db: db.clone(),
        server_url: format!("{HOST}/"),
        addressbooks: addressbooks.iter().map(|s| s.to_string()).collect(),
        progress: Default::default(),
        control,
        sealer: None,
    });
    let summary = playback::scope(playback, download).await;
    if summary.is_ok() {
        db.commit_all("test").await.expect("commit");
    }
    db.close().await;
    summary
}

async fn scalar(store: &Path, sql: &'static str) -> Option<String> {
    let db = RawDb::open(&db_path_for(store))
        .await
        .expect("reopen store");
    let v: Option<String> = sqlx::query_scalar(sql)
        .fetch_optional(db.pool())
        .await
        .expect("query")
        .flatten();
    db.close().await;
    v
}

/// Discovery finds the addressbook the way Fastmail makes you find it,
/// the name filter matches its CDATA name, the first sync stores every
/// card as served, and the next applies an edit, a deletion and an
/// addition from the token it left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovers_through_well_known_and_syncs_incrementally() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let book = format!("{HOST}{BOOK}");
    for root in [&one, &two] {
        account_fixtures(root);
    }
    let v1 = cards(BRIDGE_V1);
    let v2 = cards(BRIDGE_V2);
    let listed: String = v1
        .iter()
        .map(|(uid, vcard)| resource(uid, &format!("\"{uid}-1\""), vcard))
        .collect();
    fixture(
        &one,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!("{listed}<sync-token>data:,1</sync-token>")),
        ),
    );
    fixture(&two, HttpMethod::Report, &book, "0", &api::body_sync_collection("data:,1"),
        xml(207, &multistatus(&format!(
            "{}{}<response><href>{BOOK}tng-data.vcf</href><status>HTTP/1.1 404 Not Found</status></response><sync-token>data:,2</sync-token>",
            resource("tng-picard", "\"tng-picard-2\"", card(&v2, "tng-picard")),
            resource("tng-worf", "\"tng-worf-1\"", card(&v2, "tng-worf")),
        ))));

    let first = run(&one, &store).await;
    assert_eq!(
        (
            first.addressbooks,
            first.contacts_new,
            first.errors,
            first.requests
        ),
        (1, v1.len(), 0, 5),
        "{first:?}"
    );
    assert_eq!(
        scalar(&store, "SELECT display_name FROM addressbooks")
            .await
            .as_deref(),
        Some("Bridge")
    );
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM addressbooks")
            .await
            .as_deref(),
        Some("data:,1")
    );
    assert_eq!(
        scalar(
            &store,
            "SELECT json_extract(payload, '$.vcard') FROM contacts WHERE uid = 'tng-senior-staff'"
        )
        .await
        .as_deref(),
        Some(card(&v1, "tng-senior-staff")),
        "the card is stored as served, CRLF and all"
    );

    let second = run(&two, &store).await;
    assert_eq!(
        (
            second.contacts_new,
            second.contacts_updated,
            second.contacts_deleted,
            second.errors
        ),
        (1, 1, 1, 0),
        "{second:?}"
    );
    assert_eq!(
        scalar(
            &store,
            "SELECT group_concat(uid, ',') FROM (SELECT uid FROM contacts ORDER BY uid)"
        )
        .await
        .as_deref(),
        Some("tng-picard,tng-riker,tng-senior-staff,tng-worf")
    );
    assert!(scalar(
        &store,
        "SELECT json_extract(payload, '$.vcard') FROM contacts WHERE uid = 'tng-picard'"
    )
    .await
    .unwrap()
    .contains("NCC-1701-E"));
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM addressbooks")
            .await
            .as_deref(),
        Some("data:,2")
    );
}

async fn problem_keys(store: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store))
        .await
        .expect("reopen store");
    let keys: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .expect("query problems");
    db.close().await;
    keys
}

async fn stored_uids(store: &Path) -> Option<String> {
    scalar(
        store,
        "SELECT group_concat(uid, ',') FROM (SELECT uid FROM contacts ORDER BY uid)",
    )
    .await
}

/// RFC 6578 answers a sync token the server no longer honours with 403
/// `valid-sync-token`. That used to read as "sync-collection
/// unsupported": the run logged a fallback that did not exist, kept the
/// dead token, and the address book never synced again (audit
/// 2026-10-02 §5). Now the token is dropped and the book listed whole,
/// and a card the whole listing does not name — deleted while no token
/// was held — goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_lists_the_book_whole_again() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let book = format!("{HOST}{BOOK}");
    for root in [&one, &two] {
        account_fixtures(root);
    }
    let listing = |cards: &[(String, String)], token: &str| {
        let listed: String = cards
            .iter()
            .map(|(uid, vcard)| resource(uid, &format!("\"{uid}-{token}\""), vcard))
            .collect();
        xml(
            207,
            &multistatus(&format!("{listed}<sync-token>{token}</sync-token>")),
        )
    };
    let (v1, v2) = (cards(BRIDGE_V1), cards(BRIDGE_V2));
    fixture(
        &one,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection(""),
        listing(&v1, "data:,1"),
    );
    fixture(
        &two,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection("data:,1"),
        xml(
            403,
            r#"<?xml version="1.0"?><error xmlns="DAV:"><valid-sync-token/></error>"#,
        ),
    );
    fixture(
        &two,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection(""),
        listing(&v2, "data:,5"),
    );

    run(&one, &store).await;
    let second = run(&two, &store).await;
    assert_eq!(
        (second.contacts_new, second.contacts_deleted, second.errors),
        (1, 1, 0),
        "{second:?}"
    );
    assert_eq!(
        stored_uids(&store).await.as_deref(),
        Some("tng-picard,tng-riker,tng-senior-staff,tng-worf")
    );
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM addressbooks")
            .await
            .as_deref(),
        Some("data:,5")
    );
    assert!(scalar(
        &store,
        "SELECT json_extract(payload, '$.vcard') FROM contacts WHERE uid = 'tng-picard'"
    )
    .await
    .unwrap()
    .contains("NCC-1701-E"));
    assert_eq!(problem_keys(&store).await, Vec::<String>::new());
}

/// A card with no UID cannot be stored, and an address book whose sync
/// fails is not synced; each was only a `warn!`, so nothing reached the
/// Manage row. Each is a `problems` row now: the card is held at its
/// etag with a warning on its listing row. The warning stays through a
/// failed run and a clean incremental one that does not mention it — it
/// is still not stored — and goes once the card is listed at a new etag
/// with a UID, which the mirror then holds for that href.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_sync_could_not_store_is_a_problem_row_until_it_is_stored() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, three, four, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("three"),
        d.path().join("four"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let book = format!("{HOST}{BOOK}");
    for root in [&one, &two, &three, &four] {
        account_fixtures(root);
    }
    let v1 = cards(BRIDGE_V1);
    let data = card(&v1, "tng-data");
    let no_uid = data.replace("UID:tng-data\r\n", "");
    let sync = |root: &Path, token: &str, resp: HttpResponse| {
        fixture(
            root,
            HttpMethod::Report,
            &book,
            "0",
            &api::body_sync_collection(token),
            resp,
        )
    };
    sync(
        &one,
        "",
        xml(
            207,
            &multistatus(&format!(
                "{}{}<sync-token>data:,1</sync-token>",
                resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
                resource("tng-data", "\"d1\"", &no_uid),
            )),
        ),
    );
    sync(&two, "data:,1", xml(500, "Internal Server Error"));
    sync(
        &three,
        "data:,1",
        xml(207, &multistatus("<sync-token>data:,2</sync-token>")),
    );
    sync(
        &four,
        "data:,2",
        xml(
            207,
            &multistatus(&format!(
                "{}<sync-token>data:,3</sync-token>",
                resource("tng-data", "\"d2\"", data),
            )),
        ),
    );

    let unstored = format!(
        "dav_resources:{}",
        datalib_etl_web::dav::state::resource_id(
            &datalib_etl_contacts::ingest::db::addressbook_pk("carddav.enterprise.test", BOOK),
            &format!("{BOOK}tng-data.vcf")
        )
    );
    let first = run(&one, &store).await;
    assert_eq!((first.contacts_new, first.errors), (1, 1), "{first:?}");
    assert_eq!(problem_keys(&store).await, vec![unstored.clone()]);
    let second = run(&two, &store).await;
    assert_eq!(second.errors, 1, "{second:?}");
    assert_eq!(
        problem_keys(&store).await,
        vec![unstored.clone(), "listing:addressbook Bridge".to_string()]
    );
    // A run that stopped before the address book has not synced it again.
    let before = problem_keys(&store).await;
    run_stopped(&two, &store).await;
    assert_eq!(problem_keys(&store).await, before);
    run(&three, &store).await;
    assert_eq!(problem_keys(&store).await, vec![unstored]);
    let fourth = run(&four, &store).await;
    assert_eq!(fourth.contacts_new, 1, "{fourth:?}");
    assert_eq!(problem_keys(&store).await, Vec::<String>::new());
    assert_eq!(
        stored_uids(&store).await.as_deref(),
        Some("tng-data,tng-picard")
    );
}

/// A configured `addressbooks` name no address book has was passed over
/// in silence, so a typo mirrored less than was asked for and said
/// nothing; and with every name wrong the run synced nothing and
/// reported success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_name_no_address_book_has_is_a_problem_row() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let book = format!("{HOST}{BOOK}");
    for root in [&one, &two] {
        account_fixtures(root);
    }
    let v1 = cards(BRIDGE_V1);
    fixture(
        &one,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!(
                "{}<sync-token>data:,1</sync-token>",
                resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
            )),
        ),
    );
    fixture(
        &two,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection("data:,1"),
        xml(207, &multistatus("<sync-token>data:,2</sync-token>")),
    );

    let first = run_named(&one, &store, Default::default(), &["Bridge", "Holodeck"])
        .await
        .expect("one name matched");
    assert_eq!(first.contacts_new, 1, "{first:?}");
    assert_eq!(
        problem_keys(&store).await,
        vec!["config:addressbooks:Holodeck".to_string()]
    );

    let none = run_named(
        &two,
        &store,
        Default::default(),
        &["Holodeck", "Ten Forward"],
    )
    .await
    .expect_err("no name matched, and an empty filter means every address book");
    assert!(
        format!("{none:#}").contains("none of the configured addressbooks"),
        "{none:#}"
    );
    assert_eq!(stored_uids(&store).await.as_deref(), Some("tng-picard"));

    run(&two, &store).await;
    assert_eq!(problem_keys(&store).await, Vec::<String>::new());
}

/// A card the listing named without its data and the `multiget` did not
/// return was noted as "could not store" while the token advanced, so in
/// token mode no later run asked for it: the listing never named it
/// again. It is owed until it is held at the etag it was listed at, so
/// the next run asks for it although its listing says nothing changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_the_multiget_did_not_return_is_asked_for_again_next_run() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let book = format!("{HOST}{BOOK}");
    for root in [&one, &two] {
        account_fixtures(root);
    }
    let v1 = cards(BRIDGE_V1);
    let data_href = format!("{BOOK}tng-data.vcf");
    let listed_without_data = format!(
        "<response><href>{data_href}</href><propstat><prop><getetag>\"d1\"</getetag></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    );
    let multiget = api::KIND.body_multiget(std::slice::from_ref(&data_href));
    fixture(
        &one,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!(
                "{}{listed_without_data}<sync-token>data:,1</sync-token>",
                resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
            )),
        ),
    );
    fixture(
        &one,
        HttpMethod::Report,
        &book,
        "0",
        &multiget,
        xml(207, &multistatus("")),
    );
    fixture(
        &two,
        HttpMethod::Report,
        &book,
        "0",
        &api::body_sync_collection("data:,1"),
        xml(207, &multistatus("<sync-token>data:,2</sync-token>")),
    );
    fixture(
        &two,
        HttpMethod::Report,
        &book,
        "0",
        &multiget,
        xml(
            207,
            &multistatus(&resource("tng-data", "\"d1\"", card(&v1, "tng-data"))),
        ),
    );

    let first = run(&one, &store).await;
    assert_eq!(
        (first.contacts_new, first.errors, first.requests),
        (1, 1, 6),
        "{first:?}"
    );
    assert_eq!(stored_uids(&store).await.as_deref(), Some("tng-picard"));
    assert_eq!(
        problem_keys(&store).await.len(),
        1,
        "the card that did not come is a row"
    );

    let second = run(&two, &store).await;
    assert_eq!(
        stored_uids(&store).await.as_deref(),
        Some("tng-data,tng-picard"),
        "the card the listing no longer names is asked for again: {second:?}"
    );
    assert_eq!((second.contacts_new, second.errors), (1, 0), "{second:?}");
    assert_eq!(problem_keys(&store).await, Vec::<String>::new());
}

/// An address book the home listing no longer names goes with its
/// cards and everything it listed: the listing is one PROPFIND, whole
/// by nature, so absence from it is deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_address_book_the_server_no_longer_lists_goes_with_its_cards() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    account_fixtures(&one);
    let v1 = cards(BRIDGE_V1);
    fixture(
        &one,
        HttpMethod::Report,
        &format!("{HOST}{BOOK}"),
        "0",
        &api::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!(
                "{}<sync-token>data:,1</sync-token>",
                resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
            )),
        ),
    );
    account_fixtures_listing(&two, false);

    let first = run_named(&one, &store, Default::default(), &[])
        .await
        .expect("first run");
    assert_eq!(first.contacts_new, 1, "{first:?}");
    let second = run_named(&two, &store, Default::default(), &[])
        .await
        .expect("second run");
    assert_eq!(
        (second.addressbooks, second.contacts_deleted),
        (0, 1),
        "{second:?}"
    );
    assert_eq!(stored_uids(&store).await, None);
    for sql in [
        "SELECT CAST(count(*) AS TEXT) FROM addressbooks",
        "SELECT CAST(count(*) AS TEXT) FROM dav_resources",
        "SELECT CAST(count(*) AS TEXT) FROM dav_resources_bookkeeping",
    ] {
        assert_eq!(scalar(&store, sql).await.as_deref(), Some("0"), "{sql}");
    }
}

/// A home listing that names nothing is not an empty home: a `Depth: 1`
/// PROPFIND always answers for the home itself. A 200 with an empty body
/// parsed to no address books, and every stored book and its cards was
/// deleted (#991). Now the run fails and deletes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_home_listing_that_names_nothing_deletes_no_address_book() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    account_fixtures(&one);
    let v1 = cards(BRIDGE_V1);
    fixture(
        &one,
        HttpMethod::Report,
        &format!("{HOST}{BOOK}"),
        "0",
        &api::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!(
                "{}<sync-token>data:,1</sync-token>",
                resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
            )),
        ),
    );
    account_fixtures(&two);
    fixture(
        &two,
        HttpMethod::Propfind,
        &format!("{HOST}{HOME}"),
        "1",
        api::BODY_LIST_ADDRESSBOOKS,
        xml(200, ""),
    );

    let first = run_named(&one, &store, Default::default(), &[])
        .await
        .expect("first run");
    assert_eq!(first.contacts_new, 1, "{first:?}");
    let second = run_named(&two, &store, Default::default(), &[]).await;
    assert!(second.is_err(), "{second:?}");
    for sql in [
        "SELECT CAST(count(*) AS TEXT) FROM addressbooks",
        "SELECT CAST(count(*) AS TEXT) FROM dav_resources",
    ] {
        assert_eq!(scalar(&store, sql).await.as_deref(), Some("1"), "{sql}");
    }
}
