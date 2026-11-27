//! Who the index says a handle is, read from what the documents carried:
//! written the way the step writes it, read through the repo.

use std::collections::BTreeMap;
use std::sync::Arc;

use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
use datalib_etl_render::grid_index::{apply_one, seal, RenderedMarkdown, WriteLock};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use datalib_schema::search_terms::{SearchTerm, SearchTermKind};
use datalib_unified_grid_and_search_index::dolt_repo::{
    DoltRepo, AUTHORED_SQL, CONTACT_PAGES_SQL, FULL_NUMBERS_SQL, PEOPLE_SQL, RECORD_WEIGHTS_SQL,
    SAME_DIGITS_SQL,
};
use datalib_unified_grid_and_search_index::people::{Counted, People};
use datalib_unified_grid_and_search_index::repo::IndexRepo;

use crate::dolt_backend_integration::{unique_db_path, writer};
use datalib_sql::{test_sql_id, Sql};

const RIKER: &str = "email:riker@enterprise.org";
const TROI: &str = "email:troi@enterprise.org";

fn record(address: &str, names: &[&str]) -> NormalizedContact {
    let mut c = NormalizedContact::new("mail", format!("email:{address}"), ContactKind::Person);
    c.names = names.iter().map(|n| n.to_string()).collect();
    c.handles = vec![ContactHandle::email(None, address)];
    c
}

/// `md` with `n` messages `handle` wrote in it, the newest at `at`.
fn wrote(mut md: RenderedMarkdown, handle: &str, n: usize, at: &str) -> RenderedMarkdown {
    let document = md.markdown_uuid.clone();
    for i in 0..n {
        let uuid = format!("{document}-{handle}-{i}");
        let row = GridRow::builder()
            .uuid(uuid.as_str())
            .provider(Provider::Email)
            .kind("Email")
            .source_label("Mail")
            .created_at(Some(
                if i + 1 == n {
                    at
                } else {
                    "2300-01-01T00:00:00+00:00"
                }
                .to_string(),
            ))
            .author_handle(Some(handle.to_string()))
            .conversation_uuid(document.as_str())
            .entire_chat(format!("/chat/{document}"))
            .body("")
            .markdown_uuid(Some(document.clone()))
            .build()
            .expect("a valid row");
        md.rows.push(row);
    }
    md
}

fn document(uuid: &str, contacts: Vec<NormalizedContact>) -> RenderedMarkdown {
    let row = GridRow::builder()
        .uuid(uuid)
        .provider(Provider::Email)
        .kind("Email Thread")
        .source_label("Mail")
        .is_document(true)
        .created_at(Some("2369-01-01T10:00:00+00:00".to_string()))
        .conversation_uuid(uuid)
        .entire_chat(format!("/chat/{uuid}"))
        .body("")
        .markdown_uuid(Some(uuid.to_string()))
        .build()
        .expect("a valid row");
    RenderedMarkdown {
        searchable: true,
        content_hash: None,
        markdown_uuid: uuid.to_string(),
        source_id: "mail".into(),
        upstream_cursor: None,
        render_unit_key: None,
        render_version: 1,
        rows: vec![row],
        sections: Vec::new(),
        front: Default::default(),
        edges: Vec::new(),
        contacts,
        data_nodes: Vec::new(),
        filings: Vec::new(),
        photos: Vec::new(),
        problems: Vec::new(),
    }
}

async fn index_of(documents: &[RenderedMarkdown]) -> (DoltRepo, sqlx::SqlitePool) {
    let root = Arc::new(unique_db_path().parent().unwrap().to_path_buf());
    let pool = writer(&root).await;
    let lock = WriteLock::for_index(pool.clone());
    for md in documents {
        apply_one(&lock, md).await.expect("apply a document");
    }
    seal(&lock).await.expect("seal");
    (DoltRepo::open(root).await.expect("open the repo"), pool)
}

async fn people(repo: &DoltRepo, handles: &[&str]) -> BTreeMap<String, Vec<Counted>> {
    asked(repo, handles, &[]).await.people
}

async fn asked(repo: &DoltRepo, handles: &[&str], pages: &[&str]) -> People {
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let rows = repo.people(&owned(handles), &owned(pages)).await;
    People::of(rows.expect("people"))
}

/// A person in many documents is one answer, whichever of their names each
/// document showed, and how much they wrote is the grid's rows by their
/// handle in that source: the one count, which `from:` finds too. A
/// document they wrote nothing in credits the name it shows with one, and
/// someone who wrote nothing has no count.
#[tokio::test]
async fn a_person_is_counted_from_the_rows_they_wrote() {
    let a = "riker@enterprise.org";
    let (repo, _w) = index_of(&[
        wrote(
            document("t-1", vec![record(a, &["Will Riker"])]),
            RIKER,
            2,
            "2369-01-01T10:00:00+00:00",
        ),
        wrote(
            document("t-2", vec![record(a, &["Will Riker"])]),
            RIKER,
            3,
            "2369-05-01T10:00:00+00:00",
        ),
        wrote(
            document("t-3", vec![record(a, &["Number One"])]),
            RIKER,
            4,
            "2369-03-01T10:00:00+00:00",
        ),
        document("t-4", vec![record(a, &["Will Riker"])]),
        document("t-5", vec![record("troi@enterprise.org", &["Deanna Troi"])]),
        document("t-6", vec![record("troi@enterprise.org", &["Deanna Troi"])]),
    ])
    .await;
    let got = people(&repo, &[RIKER, TROI, "email:nobody@enterprise.org"]).await;
    assert_eq!(got.keys().collect::<Vec<_>>(), [RIKER, TROI]);
    let riker = &got[RIKER];
    assert_eq!(riker.len(), 1, "one per source and key: {riker:?}");
    assert_eq!(riker[0].contact.names, ["Will Riker", "Number One"]);
    let seen = riker[0].seen.as_ref().expect("a count");
    assert_eq!(seen.items, 9);
    assert!(
        seen.last_at
            .as_deref()
            .is_some_and(|at| at.starts_with("2369-05-01")),
        "{seen:?}"
    );
    assert_eq!(got[TROI].len(), 1);
    assert_eq!(got[TROI][0].seen, None);
}

/// A name a person only ever goes by unwritten (a recipient) counts once
/// per document, so five threads naming them outweigh four messages.
#[tokio::test]
async fn a_document_that_only_names_a_person_credits_the_name_once() {
    let a = "riker@enterprise.org";
    let mut documents: Vec<RenderedMarkdown> = (0..5)
        .map(|i| document(&format!("t-{i}"), vec![record(a, &["Will Riker"])]))
        .collect();
    documents.push(wrote(
        document("t-9", vec![record(a, &["Number One"])]),
        RIKER,
        4,
        "2369-01-01T10:00:00+00:00",
    ));
    let (repo, _w) = index_of(&documents).await;
    assert_eq!(
        people(&repo, &[RIKER]).await[RIKER][0].contact.names,
        ["Will Riker", "Number One"]
    );
}

/// A conversation named by its people gets each one's name and count in
/// that conversation, not over all of them, and not another person's; one
/// its document says nothing of is there by handle alone.
#[tokio::test]
async fn a_conversation_names_its_people_from_its_own_document() {
    let mut chat = document(
        "t-1",
        vec![
            record("riker@enterprise.org", &["Will Riker"]),
            record("troi@enterprise.org", &["Deanna Troi"]),
        ],
    );
    let worf = "email:worf@enterprise.org";
    chat.rows[0].set_supplied_terms(
        [RIKER, TROI, worf].map(|h| SearchTerm::new(SearchTermKind::Subject, h)),
    );
    let chat = wrote(chat, RIKER, 2, "2369-01-01T10:00:00+00:00");
    let chat = wrote(chat, TROI, 5, "2369-01-01T10:00:00+00:00");
    let other = wrote(
        document("t-2", vec![record("riker@enterprise.org", &["Will Riker"])]),
        RIKER,
        9,
        "2369-01-01T10:00:00+00:00",
    );
    let (repo, _w) = index_of(&[chat, other]).await;
    let rows = repo.rows_by_uuids(&["t-1".into()]).await.expect("rows");
    let named: Vec<(&str, Option<&str>, i64)> = rows[0]
        .subject_people
        .iter()
        .map(|p| (p.value.as_str(), p.name.as_deref(), p.items))
        .collect();
    assert_eq!(
        named,
        [
            (TROI, Some("Deanna Troi"), 5),
            (RIKER, Some("Will Riker"), 2),
            (worf, None, 0),
        ]
    );
}

/// A contact's own row gets the handles its document's contact carries.
#[tokio::test]
async fn a_contacts_row_gets_its_handles() {
    let mut card = record("riker@enterprise.org", &["Will Riker"]);
    card.handles
        .push(ContactHandle::phone(None, "+1 202 555 0101"));
    let mut md = document("card", vec![card]);
    md.rows[0].contact = Some("Will Riker".into());
    let (repo, _w) = index_of(&[md]).await;
    let rows = repo.rows_by_uuids(&["card".into()]).await.expect("rows");
    assert_eq!(rows[0].contact_handles, [RIKER, "tel:+12025550101"]);
}

/// What answering for Riker does when `n` documents carry them: the rows
/// the statements that read contacts answered with, and the steps they took.
async fn cost_of_a_person_in(n: usize) -> (Vec<(String, u64)>, u64) {
    let root = Arc::new(unique_db_path().parent().unwrap().to_path_buf());
    let recording = datalib_sql_work::record_under(&root);
    let pool = writer(&root).await;
    let lock = WriteLock::for_index(pool.clone());
    for i in 0..n {
        let riker = record("riker@enterprise.org", &["Will Riker"]);
        let md = wrote(
            document(&format!("t-{i}"), vec![riker]),
            RIKER,
            1 + i % 3,
            "2369-01-01T10:00:00+00:00",
        );
        apply_one(&lock, &md).await.expect("apply a document");
    }
    seal(&lock).await.expect("seal");
    let repo = DoltRepo::open(root).await.expect("open the repo");
    recording.take();
    assert_eq!(people(&repo, &[RIKER]).await[RIKER].len(), 1);
    // Not what opening the index runs, which may land on either side of
    // the take above.
    let mut ran = recording.take();
    ran.retain(|r| r.sql.contains("contact"));
    assert!(
        ran.iter().any(|r| r.sql == PEOPLE_SQL.as_str()),
        "the read was not recorded; this checks nothing: {ran:#?}"
    );
    (
        ran.iter().map(|r| (r.sql.clone(), r.work.rows)).collect(),
        ran.iter().map(|r| r.work.vm_steps).sum(),
    )
}

/// Guards the read that fetched and parsed a copy of a person's record
/// for every document that carries it: 31,303 copies of 22 records for
/// one handle. Each document is only weighed in SQL, its rows counted by
/// the author index: a few seeks, and no row answered for it.
#[tokio::test]
async fn a_person_in_many_documents_is_not_read_once_per_document() {
    let (few, few_steps) = cost_of_a_person_in(10).await;
    let (many, many_steps) = cost_of_a_person_in(500).await;
    assert_eq!(few, many, "rows answered for 10 and 500 documents");
    let each = (many_steps - few_steps) / 490;
    assert!(each < 200, "{each} steps for each document more");
}

/// Who a handle is reads that handle's records, the documents that carry
/// them and what they wrote by the author index, and no table whole.
#[tokio::test]
async fn who_a_handle_is_is_read_by_its_handle() {
    let (_repo, w) = index_of(&[]).await;
    for (sql, bound, wanted) in [
        (
            PEOPLE_SQL,
            vec![r#"["email:riker@enterprise.org"]"#],
            vec![
                "SEARCH h USING COVERING INDEX contact_record_handles_by_handle (handle=?",
                "SEARCH r USING INTEGER PRIMARY KEY (rowid=?)",
            ],
        ),
        (
            RECORD_WEIGHTS_SQL,
            vec!["[1]"],
            vec![
                "SEARCH d USING COVERING INDEX document_contacts_by_record (record_id=?)",
                "SEARCH g USING COVERING INDEX grid_rows_by_author \
                 (author_handle=? AND source_id=? AND markdown_uuid=?)",
            ],
        ),
        (
            AUTHORED_SQL,
            vec![r#"["email:riker@enterprise.org"]"#, r#"["mail"]"#],
            vec!["SEARCH grid_rows USING COVERING INDEX grid_rows_by_author (author_handle=? AND source_id=?)"],
        ),
    ] {
        // Audited: a constant statement of ours.
        let mut q = Sql::dynamic(test_sql_id!(), format!("EXPLAIN QUERY PLAN {sql}")).query_as();
        for b in bound {
            q = q.bind(b);
        }
        let rows: Vec<(i64, i64, i64, String)> = q.fetch_all(&w).await.expect("explain");
        let plan: Vec<String> = rows.into_iter().map(|r| r.3).collect();
        for wanted in wanted {
            assert!(
                plan.iter().any(|d| d.starts_with(wanted)),
                "no {wanted:?} in {plan:#?}"
            );
        }
        assert!(
            !plan.iter().any(|d| d.starts_with("SCAN ") && !d.contains("json_each")),
            "{plan:#?}"
        );
    }
}

/// Kalita's card writes her number the way it is dialled at home, so it
/// makes no handle. It is found from the full number a chat knows, and the
/// full number from it, by its digits. A number another country also ends
/// in the same digits is both, and one no source knows is neither.
#[tokio::test]
async fn a_number_with_no_country_code_is_matched_by_its_digits() {
    let card = |key: &str, name: &str, number: &str| {
        let mut c = NormalizedContact::new("book", key, ContactKind::Person);
        c.names = vec![name.into()];
        c.handles = vec![ContactHandle::phone(None, number)];
        let mut md = document(key, vec![c]);
        md.rows[0].contact = Some(name.into());
        md
    };
    let caller = |n: usize, number: &str| {
        let mut c = NormalizedContact::new("calls", number, ContactKind::Person);
        c.handles = vec![ContactHandle::phone(None, number)];
        document(&format!("call-{n}"), vec![c])
    };
    let (repo, _w) = index_of(&[
        card("kalita", "Kalita", "(202) 555-0147"),
        card("ro", "Ro Laren", "079 5550 0199"),
        card("hugh", "Hugh", "(907) 555-0111"),
        caller(1, "+12025550147"),
        caller(2, "+447955500199"),
        caller(3, "+337955500199"),
        // Ends in the same seven digits, and is another number.
        caller(4, "+19075550147"),
    ])
    .await;

    let got = asked(&repo, &["tel:+12025550147", "tel:+19075550147"], &[]).await;
    assert_eq!(
        got.same_digits.keys().collect::<Vec<_>>(),
        ["tel:+12025550147"]
    );
    assert_eq!(
        got.same_digits["tel:+12025550147"][0].contact.names,
        ["Kalita"]
    );
    assert_eq!(got.people["tel:+12025550147"].len(), 1, "not by a guess");

    let got = asked(&repo, &[], &["kalita", "ro", "hugh", "call-1", "nobody"]).await;
    assert_eq!(
        got.pages.keys().collect::<Vec<_>>(),
        ["hugh", "kalita", "ro"]
    );
    let full: BTreeMap<&str, Vec<&str>> = got
        .full_numbers
        .iter()
        .map(|(v, hs)| (v.as_str(), hs.iter().map(|h| h.as_str()).collect()))
        .collect();
    assert_eq!(
        full,
        BTreeMap::from([
            ("(202) 555-0147", vec!["tel:+12025550147"]),
            (
                "079 5550 0199",
                vec!["tel:+337955500199", "tel:+447955500199"]
            ),
        ])
    );
}

/// A number's digits are looked up, and a page by its id; no table is
/// read whole.
#[tokio::test]
async fn numbers_and_pages_are_read_by_index() {
    let (_repo, w) = index_of(&[]).await;
    for (sql, bound, wanted) in [
        (
            SAME_DIGITS_SQL,
            r#"["5550147"]"#,
            "SEARCH contact_record_numbers USING COVERING INDEX contact_record_numbers_by_last_digits",
        ),
        (
            FULL_NUMBERS_SQL,
            r#"["5550147"]"#,
            "SEARCH contact_record_handles USING COVERING INDEX contact_record_handles_by_last_digits",
        ),
        (CONTACT_PAGES_SQL, r#"["kalita"]"#, "SEARCH d USING"),
    ] {
        // Audited: a constant statement of ours.
        let rows: Vec<(i64, i64, i64, String)> =
            Sql::dynamic(test_sql_id!(), format!("EXPLAIN QUERY PLAN {sql}"))
                .query_as()
                .bind(bound)
                .fetch_all(&w)
                .await
                .expect("explain");
        let plan: Vec<String> = rows.into_iter().map(|r| r.3).collect();
        assert!(
            plan.iter().any(|d| d.starts_with(wanted)),
            "no {wanted:?} in {plan:#?}"
        );
        assert!(
            !plan.iter().any(|d| {
                [
                    "g",
                    "r",
                    "d",
                    "contact_record_numbers",
                    "contact_record_handles",
                ]
                .iter()
                .any(|t| d.starts_with(&format!("SCAN {t}")))
            }),
            "{plan:#?}"
        );
    }
}
