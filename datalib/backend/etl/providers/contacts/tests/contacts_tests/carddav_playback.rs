//! A CardDAV account shaped like Fastmail's, measured live on
//! 2026-09-24: the bare host answers 404, `/.well-known/carddav`
//! redirects to `/dav/addressbooks`, every value is CDATA, each
//! collection's missing properties come back in a 404 propstat, and the
//! cards are the `Bridge.vcf` fixture's own, CRLF and folded as served.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl::dav;
use datalib_etl::http::{HttpMethod, HttpResponse, LatchkeySettings, PLAYBACK_ENV};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::write_fixture;
use datalib_etl_contacts::ingest::{self, api, db_path_for, RawDb};

const HOST: &str = "https://carddav.enterprise.test";
const PRINCIPAL: &str = "/dav/principals/user/picard@enterprise.test/";
const HOME: &str = "/dav/addressbooks/user/picard@enterprise.test/";
const BOOK: &str = "/dav/addressbooks/user/picard@enterprise.test/Default/";

const BRIDGE_V1: &str = include_str!("../fixtures/carddav_tng/Bridge.vcf");
const BRIDGE_V2: &str = include_str!("../fixtures/carddav_tng_v2/Bridge.vcf");

/// `(UID, the card as served)` for each card of a `.vcf`.
fn cards(vcf: &str) -> Vec<(String, String)> {
    vcf.split_inclusive("END:VCARD\r\n")
        .map(|card| {
            let uid = api::vcard_uid(card).expect("every Bridge card has a UID");
            (uid, card.to_string())
        })
        .collect()
}

fn card<'a>(cards: &'a [(String, String)], uid: &str) -> &'a str {
    &cards.iter().find(|(u, _)| u == uid).expect(uid).1
}

fn xml(status: u16, body: &str) -> HttpResponse {
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

fn fixture(
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

fn resource(uid: &str, etag: &str, vcard: &str) -> String {
    format!(
        "<response><href>{BOOK}{uid}.vcf</href><propstat><prop><getetag>{etag}</getetag>\
         <card:address-data><![CDATA[{vcard}]]></card:address-data></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

fn multistatus(inner: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">{inner}</multistatus>"#
    )
}

/// Discovery and the addressbook listing, shared by both runs.
fn account_fixtures(root: &Path) {
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
    fixture(root, HttpMethod::Propfind, &format!("{HOST}{HOME}"), "1", api::BODY_LIST_ADDRESSBOOKS,
        xml(207, &multistatus(&format!(
            "<response><href>{HOME}</href>\
             <propstat><prop><resourcetype><collection/></resourcetype><displayname><![CDATA[#addressbooks]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat>\
             <propstat><prop><card:addressbook-description/><cs:getctag/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>\
             <response><href>{BOOK}</href>\
             <propstat><prop><resourcetype><collection/><card:addressbook/></resourcetype><displayname><![CDATA[Bridge]]></displayname><cs:getctag>2370-1</cs:getctag></prop><status>HTTP/1.1 200 OK</status></propstat>\
             <propstat><prop><card:addressbook-description/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>"
        ))));
}

async fn run(playback: &Path, store: &Path) -> ingest::FetchSummary {
    std::env::set_var(PLAYBACK_ENV, playback);
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let summary = ingest::fetch(ingest::FetchOptions {
        latchkey: LatchkeySettings::default(),
        db: db.clone(),
        server_url: format!("{HOST}/"),
        addressbooks: Some(vec!["Bridge".to_string()]),
        progress: Default::default(),
        control: Default::default(),
    })
    .await;
    db.commit_all("test").await.expect("commit");
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);
    summary.expect("carddav fetch under playback")
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
