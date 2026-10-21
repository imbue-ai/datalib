//! A Doc written into a real raw store the way the download writes it,
//! then read back and rendered: the store reads (JSONB payloads, the
//! gzipped native JSON in the CAS, the image parts) are what the unit
//! tests cannot reach.

use datalib_etl::doltlite_raw::{commit_run, WirePayload};
use datalib_etl_google_drive::ingest::db::{store_captures, Capture, Part, Split};
use datalib_etl_google_drive::ingest::schema_raw::{
    image_part, CaptureRow, CommentRow, FileRow, PART_NATIVE,
};
use datalib_etl_google_drive::ingest::shape::gzip;
use datalib_etl_google_drive::ingest::RawDb;
use datalib_etl_google_drive_render::render::{render_doc, DocInput};
use datalib_etl_google_drive_render::{ids, store};
use datalib_etl_render::inputs::{Inputs, RawRange};
use serde_json::{json, Value};

const DOC: &str = "1PicardLogSupplemental";
const SHEET: &str = "1LaForgeDutyRoster";
const IMAGE_POINTER: &str = "/tabs/0/documentTab/inlineObjects/kix.enterprise/\
    inlineObjectProperties/embeddedObject/imageProperties/contentUri";
/// The smallest PNG: its signature and an empty IHDR are enough for the
/// blob store to name it `.png`.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

fn wire(id: &str, payload: &Value) -> WirePayload {
    WirePayload {
        id: id.to_string(),
        payload: payload.to_string(),
    }
}

fn native() -> Value {
    json!({
        "documentId": DOC,
        "title": "Captain's log, supplemental",
        "tabs": [{"tabProperties": {"tabId": "t.0", "title": "Tab 1"},
                  "documentTab": {
                      "body": {"content": [
                          {"paragraph": {"elements": [{"textRun": {"content": "Captain's log\n"}}],
                                         "paragraphStyle": {"namedStyleType": "HEADING_1"}}},
                          {"paragraph": {"elements": [
                              {"textRun": {"content": "We have reached Farpoint. "}},
                              {"inlineObjectElement": {"inlineObjectId": "kix.enterprise"}},
                              {"textRun": {"content": "\n"}}],
                                         "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"}}}]},
                      "inlineObjects": {"kix.enterprise": {"inlineObjectProperties": {
                          "embeddedObject": {"title": "NCC-1701-D", "imageProperties": {}}}}}}}]
    })
}

fn capture(id: &str, mime: &str, parts: Vec<Part>, comments: Vec<(&str, Value)>) -> Capture {
    let entry = json!({
        "id": id, "name": "Captain's log, supplemental", "mimeType": mime, "version": "12",
        "createdTime": "2364-03-01T08:00:00.000Z", "modifiedTime": "2364-03-02T09:00:00.000Z",
        "webViewLink": format!("https://docs.google.com/document/d/{id}/edit"),
        "owners": [{"displayName": "Jean-Luc Picard", "me": true,
                    "emailAddress": "picard@enterprise.test"}],
    });
    Capture {
        file: Split {
            row: FileRow {
                id_and_payload: wire(id, &entry),
            },
            volatile: None,
        },
        revisions: Vec::new(),
        comments: comments
            .into_iter()
            .map(|(cid, c)| Split {
                row: CommentRow {
                    id_and_payload: wire(&format!("{id}#{cid}"), &c),
                    file_id: id.to_string(),
                },
                volatile: None,
            })
            .collect(),
        row: Split {
            row: CaptureRow {
                id: CaptureRow::id_for(id, "12"),
                file_id: id.to_string(),
                version: "12".to_string(),
                modified_time: None,
                native_revision_id: None,
                head_revision_id: None,
            },
            volatile: None,
        },
        parts,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_doc_in_the_store_renders_with_its_image_and_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("ingest").join("entities.doltlite_db");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let db = RawDb::open(&store_path).await.unwrap();
    let doc = capture(
        DOC,
        store::DOC_MIME,
        vec![
            Part {
                name: PART_NATIVE.to_string(),
                bytes: Some(gzip(native().to_string().as_bytes()).unwrap()),
                content_type: Some("application/gzip".to_string()),
            },
            Part {
                name: image_part(IMAGE_POINTER),
                bytes: Some(PNG.to_vec()),
                content_type: Some("image/png".to_string()),
            },
        ],
        vec![(
            "AAA",
            json!({"id": "AAA", "createdTime": "2364-03-02T10:00:00.000Z",
                   "author": {"displayName": "Geordi La Forge", "me": false},
                   "content": "Sensors confirm.",
                   "quotedFileContent": {"mimeType": "text/html", "value": "reached Farpoint"}}),
        )],
    );
    let sheet = capture(
        SHEET,
        "application/vnd.google-apps.spreadsheet",
        Vec::new(),
        Vec::new(),
    );
    let mut tx = db.pool().begin().await.unwrap();
    store_captures(&mut tx, db.cas(), &[&doc, &sheet])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    commit_run(db.pool(), "test fixture").await.unwrap();
    db.close().await;

    let db = RawDb::open_reader(&store_path, None)
        .await
        .unwrap()
        .expect("committed");
    let listing = store::load(&db, RawRange::cold()).await.unwrap();
    assert_eq!(
        listing.docs.keys().collect::<Vec<_>>(),
        vec![DOC],
        "a Sheet does not render"
    );
    let capture = &listing.captures[DOC];
    let native = store::native(&db, capture).await.unwrap();
    let images = store::images(&db, capture).await.unwrap();
    let inputs = Inputs::default();
    let md = render_doc(
        dir.path(),
        "drive",
        &DocInput {
            file_id: DOC,
            entry: &listing.docs[DOC],
            capture,
            native: &native,
            images: &images,
            comments: &listing.comments[DOC],
        },
        &inputs,
    )
    .unwrap();
    db.close().await;

    let text = std::fs::read_to_string(&md.md_path).unwrap();
    let image = text
        .split("![NCC-1701-D](")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("no image link in {text}"));
    assert!(
        image.starts_with("blobs/") && image.ends_with(".png"),
        "{image}"
    );
    assert!(
        md.md_path.parent().unwrap().join(image).exists(),
        "{image} was not written beside the document"
    );
    let heading = text.find("# Captain's log\n").expect(&text);
    let thread = text
        .find("<details class=\"comment-thread\">")
        .expect(&text);
    assert!(heading < thread, "{text}");
    assert!(
        text.contains("## [Geordi La Forge](datalib:handle/google/name/"),
        "{text}"
    );

    assert_eq!(md.markdown_uuid, ids::file("drive", DOC).uuid);
    let kinds: Vec<(&str, Option<&str>, Option<&str>)> = md
        .rows
        .iter()
        .map(|r| {
            (
                r.kind.as_str(),
                r.author_handle.as_deref(),
                r.account.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                "Google Doc",
                Some("email:picard@enterprise.test"),
                Some("picard@enterprise.test")
            ),
            (
                "Google Doc Comment",
                Some("google:name/Geordi La Forge"),
                Some("picard@enterprise.test")
            ),
        ]
    );
    assert!(md.problems.is_empty(), "{:?}", md.problems);
    let read: Vec<String> = inputs
        .declared()
        .iter()
        .map(|i| format!("{}:{}", i.table, i.id))
        .collect();
    for table in ["files", "captures", "capture_parts", "comments"] {
        assert!(
            read.iter().any(|r| r.starts_with(&format!("{table}:"))),
            "{table} not declared: {read:?}"
        );
    }
}
