//! A TNG account's Drive, as playback fixtures. The envelopes and field
//! names are the ones a live account returned on 2026-10-21; the values
//! are made up.
//!
//! - a Doc the account owns, with two revisions, a comment thread (one
//!   reply) and a deleted comment, one inline image, and an ODT export;
//! - a Sheet shared with it read-only, so Drive lists no revisions;
//! - a deck whose ODP export Google declines as too large.
//!
//! [`view_only_doc`] adds a fourth file, a Doc the account may only view.
//!
//! The listing spans two pages. [`Edition::After`] moves the Doc to a new
//! version with a new comment, and lists the other two unchanged except
//! for the fields Drive changes on every read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_etl_google_drive::ingest::api;
use datalib_etl_google_drive_config::{ExportFormat, FileKind};
use datalib_etl_web::http::{HttpRequest, HttpResponse};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

pub const DOC: &str = "1PicardLogSupplemental";
pub const SHEET: &str = "1LaForgeDutyRoster";
pub const DECK: &str = "1RikerBriefingDeck";
pub const VIEW_ONLY_DOC: &str = "1TroiCounselingNotes";

pub const KINDS: &[FileKind] = &[
    FileKind::Document,
    FileKind::Spreadsheet,
    FileKind::Presentation,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Before,
    After,
}

pub fn put(root: &Path, req: HttpRequest, resp: HttpResponse) {
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub fn bytes(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: BTreeMap::new(),
        body: body.to_vec(),
        duration_ms: 0,
    }
}

fn picard() -> Value {
    json!({"kind": "drive#user", "displayName": "Jean-Luc Picard", "me": true,
           "permissionId": "07001", "emailAddress": "picard@enterprise.test",
           "photoLink": "https://lh3.googleusercontent.com/a/picard=s64"})
}

fn laforge() -> Value {
    json!({"kind": "drive#user", "displayName": "Geordi La Forge", "me": false,
           "permissionId": "07002", "emailAddress": "laforge@enterprise.test",
           "photoLink": "https://lh3.googleusercontent.com/a/laforge=s64"})
}

/// A listing entry. `read` stands for one read of it: the thumbnail is
/// signed afresh and the permission ids come in a different order.
pub fn entry(id: &str, kind: FileKind, version: u32, name: &str, read: u32) -> Value {
    let owned = kind != FileKind::Spreadsheet;
    let mut ids = vec!["07001", "07002", "anyoneWithLink"];
    if read % 2 == 1 {
        ids.reverse();
    }
    json!({
        "kind": "drive#file",
        "id": id,
        "name": name,
        "mimeType": api::kind_info(kind).mime_type,
        "version": version.to_string(),
        "modifiedTime": format!("2026-10-{:02}T09:00:00.000Z", 1 + version % 20),
        "createdTime": "2026-09-01T08:00:00.000Z",
        "ownedByMe": owned,
        "owners": [if owned { picard() } else { laforge() }],
        "lastModifyingUser": if owned { picard() } else { laforge() },
        "capabilities": {"canEdit": owned, "canComment": true, "canReadRevisions": owned},
        "permissionIds": ids,
        "thumbnailLink": format!("https://lh3.googleusercontent.com/drive-storage/{id}-{read}"),
        "webViewLink": format!("https://docs.google.com/d/{id}/edit"),
        "trashed": false,
    })
}

pub fn doc_native(edition: Edition) -> Value {
    let text = match edition {
        Edition::Before => "Captain's log, stardate 47457.1.\n",
        Edition::After => "Captain's log, stardate 47457.1. We have reached Farpoint.\n",
    };
    json!({
        "documentId": DOC,
        "title": "Captain's log, supplemental",
        "revisionId": match edition { Edition::Before => "ALm37BW-rev12", Edition::After => "ALm37BW-rev13" },
        "suggestionsViewMode": "SUGGESTIONS_INLINE",
        "commentsViewMode": "COMMENTS_VIEW_MODE_OMITTED",
        "tabs": [{"tabProperties": {"tabId": "t.0", "title": "Tab 1", "index": 0},
                  "documentTab": {
                      "body": {"content": [{"startIndex": 1, "endIndex": 40, "paragraph": {
                          "elements": [{"startIndex": 1, "endIndex": 40, "textRun": {
                              "content": text,
                              "suggestedInsertionIds": ["suggest.worf1"]}}]}}]},
                      "inlineObjects": {"kix.enterprise": {"objectId": "kix.enterprise",
                          "inlineObjectProperties": {"embeddedObject": {"imageProperties": {
                              "contentUri": "https://docs.google.com/docs-images-rt/enterprise-d"}}}}}}}]
    })
}

pub fn sheet_native() -> Value {
    json!({
        "spreadsheetId": SHEET,
        "properties": {"title": "Engineering duty roster", "locale": "en_US"},
        "sheets": [{"properties": {"sheetId": 0, "title": "Shifts",
                                   "gridProperties": {"rowCount": 2, "columnCount": 2}},
                    "data": [{"rowData": [
                        {"values": [{"userEnteredValue": {"stringValue": "Warp core"},
                                     "formattedValue": "Warp core"},
                                    {"userEnteredValue": {"formulaValue": "=UPPER(\"barclay\")"},
                                     "effectiveValue": {"stringValue": "BARCLAY"},
                                     "formattedValue": "BARCLAY"}]}]}]}],
        "spreadsheetUrl": format!("https://docs.google.com/spreadsheets/d/{SHEET}/edit")
    })
}

pub fn deck_native() -> Value {
    json!({
        "presentationId": DECK,
        "title": "Away team briefing",
        "revisionId": "rikerRev30",
        "pageSize": {"width": {"magnitude": 9144000, "unit": "EMU"}},
        "slides": [{"objectId": "p1", "pageElements": [{"objectId": "img1",
            "image": {"contentUrl": "https://docs.google.com/slides-images-rt/rigel-vii",
                      "sourceUrl": "https://example.test/rigel.png"}}]}],
        "layouts": [], "masters": []
    })
}

pub const ENTERPRISE_PNG: &[u8] = b"\x89PNG\r\n\x1a\n enterprise-d";
pub const RIGEL_PNG: &[u8] = b"\x89PNG\r\n\x1a\n rigel vii";
pub const DOC_ODT_BEFORE: &[u8] = b"PK\x03\x04 odt of the log at version 12";
pub const DOC_ODT_AFTER: &[u8] = b"PK\x03\x04 odt of the log at version 13";
pub const SHEET_ODS: &[u8] = b"PK\x03\x04 ods of the roster";

pub fn doc_version(edition: Edition) -> u32 {
    match edition {
        Edition::Before => 12,
        Edition::After => 13,
    }
}

/// The whole tape for one edition, under `dir/<edition>`.
pub fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    let read = match edition {
        Edition::Before => 1,
        Edition::After => 2,
    };
    let doc_v = doc_version(edition);
    listing(
        &t,
        vec![
            entry(
                DOC,
                FileKind::Document,
                doc_v,
                "Captain's log, supplemental",
                read,
            ),
            entry(
                SHEET,
                FileKind::Spreadsheet,
                7,
                "Engineering duty roster",
                read,
            ),
        ],
        vec![entry(
            DECK,
            FileKind::Presentation,
            30,
            "Away team briefing",
            read,
        )],
    );

    // The Doc: everything a capture asks for, at this edition.
    let mut comments = vec![
        json!({"kind": "drive#comment", "id": "AAAA-tea", "content": "Tea, Earl Grey, hot?",
               "author": laforge(), "createdTime": "2026-10-02T10:00:00.000Z",
               "modifiedTime": "2026-10-02T10:05:00.000Z", "resolved": false, "deleted": false,
               "anchor": "kix.anchor1", "quotedFileContent": {"mimeType": "text/html", "value": "stardate"},
               "replies": [{"kind": "drive#reply", "id": "AAAA-tea-r1", "content": "Make it so.",
                            "author": picard(), "createdTime": "2026-10-02T10:05:00.000Z",
                            "modifiedTime": "2026-10-02T10:05:00.000Z", "deleted": false}]}),
        json!({"kind": "drive#comment", "id": "AAAA-gone", "author": laforge(),
               "createdTime": "2026-10-03T10:00:00.000Z", "modifiedTime": "2026-10-03T11:00:00.000Z",
               "deleted": true, "replies": []}),
    ];
    if edition == Edition::After {
        comments.push(json!({"kind": "drive#comment", "id": "AAAA-farpoint",
            "content": "Q will want a word.", "author": laforge(),
            "createdTime": "2026-10-04T10:00:00.000Z", "modifiedTime": "2026-10-04T10:00:00.000Z",
            "resolved": true, "deleted": false, "replies": []}));
    }
    put(
        &t,
        api::req_json(&api::comments_url(DOC, None)),
        json_response(&json!({"kind": "drive#commentList", "comments": comments})),
    );
    let mut revisions = vec![
        json!({"kind": "drive#revision", "id": "1", "mimeType": api::kind_info(FileKind::Document).mime_type,
               "modifiedTime": "2026-09-01T08:00:00.000Z", "lastModifyingUser": picard(),
               "exportLinks": {"application/vnd.oasis.opendocument.text":
                   format!("https://docs.google.com/feeds/download/documents/export/Export?id={DOC}&revision=1&exportFormat=odt")}}),
        json!({"kind": "drive#revision", "id": "29", "mimeType": api::kind_info(FileKind::Document).mime_type,
               "modifiedTime": "2026-10-02T09:00:00.000Z", "lastModifyingUser": picard()}),
    ];
    if edition == Edition::After {
        revisions.push(json!({"kind": "drive#revision", "id": "41",
            "mimeType": api::kind_info(FileKind::Document).mime_type,
            "modifiedTime": "2026-10-04T09:00:00.000Z", "lastModifyingUser": picard()}));
    }
    // The revision list comes over two pages, to walk the page token.
    let (first, rest) = revisions.split_at(1);
    put(
        &t,
        api::req_json(&api::revisions_url(DOC, None)),
        json_response(&json!({"kind": "drive#revisionList", "revisions": first,
                              "nextPageToken": "rev-page-2"})),
    );
    put(
        &t,
        api::req_json(&api::revisions_url(DOC, Some("rev-page-2"))),
        json_response(&json!({"kind": "drive#revisionList", "revisions": rest})),
    );
    put(
        &t,
        api::req_bytes(&api::native_url(FileKind::Document, DOC, true)),
        json_response(&doc_native(edition)),
    );
    put(
        &t,
        api::req_image("https://docs.google.com/docs-images-rt/enterprise-d"),
        bytes(200, ENTERPRISE_PNG),
    );
    let (odt, _) = api::export_type(FileKind::Document, ExportFormat::Odf);
    put(
        &t,
        api::req_bytes(&api::export_url(DOC, odt)),
        bytes(
            200,
            match edition {
                Edition::Before => DOC_ODT_BEFORE,
                Edition::After => DOC_ODT_AFTER,
            },
        ),
    );

    // The Sheet and the deck do not change, so only the first edition
    // answers for them: a second capture of either would miss.
    if edition == Edition::Before {
        put(
            &t,
            api::req_json(&api::comments_url(SHEET, None)),
            json_response(&json!({"kind": "drive#commentList", "comments": []})),
        );
        put(
            &t,
            api::req_bytes(&api::native_url(FileKind::Spreadsheet, SHEET, true)),
            json_response(&sheet_native()),
        );
        let (ods, _) = api::export_type(FileKind::Spreadsheet, ExportFormat::Odf);
        put(
            &t,
            api::req_bytes(&api::export_url(SHEET, ods)),
            bytes(200, SHEET_ODS),
        );

        put(
            &t,
            api::req_json(&api::revisions_url(DECK, None)),
            json_response(&json!({"kind": "drive#revisionList", "revisions": [
                {"kind": "drive#revision", "id": "88", "modifiedTime": "2026-10-11T09:00:00.000Z",
                 "lastModifyingUser": picard()}]})),
        );
        put(
            &t,
            api::req_json(&api::comments_url(DECK, None)),
            json_response(&json!({"kind": "drive#commentList", "comments": []})),
        );
        put(
            &t,
            api::req_bytes(&api::native_url(FileKind::Presentation, DECK, true)),
            json_response(&deck_native()),
        );
        put(
            &t,
            api::req_image("https://docs.google.com/slides-images-rt/rigel-vii"),
            bytes(200, RIGEL_PNG),
        );
        let (odp, _) = api::export_type(FileKind::Presentation, ExportFormat::Odf);
        put(
            &t,
            api::req_bytes(&api::export_url(DECK, odp)),
            bytes(
                403,
                br#"{"error": {"code": 403, "message": "This file is too large to be exported.",
                    "errors": [{"domain": "global", "reason": "exportSizeLimitExceeded",
                                "message": "This file is too large to be exported."}]}}"#,
            ),
        );
    }
    t
}

/// The listing as two pages.
pub fn listing(t: &Path, first: Vec<Value>, second: Vec<Value>) {
    put(
        t,
        api::req_json(&api::list_url(KINDS, false, None)),
        json_response(&json!({"kind": "drive#fileList", "incompleteSearch": false,
                              "nextPageToken": "files-page-2", "files": first})),
    );
    put(
        t,
        api::req_json(&api::list_url(KINDS, false, Some("files-page-2"))),
        json_response(&json!({"kind": "drive#fileList", "incompleteSearch": false,
                              "files": second})),
    );
}

pub const VIEW_ONLY_ODT: &[u8] = b"PK\x03\x04 odt of the counseling notes";

/// A Doc shared with the account to view only: Drive lists it no
/// revisions, and the Docs API refuses its suggestions with a 403, so
/// its native JSON answers only without `suggestionsViewMode`, in the
/// mode Google picks for a viewer. Adds it to `t`'s first listing page.
/// Without `may_download` its owner has turned off download, and the
/// tape answers no export for it.
pub fn view_only_doc(t: &Path, may_download: bool) {
    let mut viewer = entry(VIEW_ONLY_DOC, FileKind::Document, 5, "Counseling notes", 1);
    viewer["capabilities"] = json!({"canEdit": false, "canComment": false,
                                    "canReadRevisions": false, "canDownload": may_download});
    viewer["ownedByMe"] = json!(false);
    listing(
        t,
        vec![
            entry(
                DOC,
                FileKind::Document,
                doc_version(Edition::Before),
                "Captain's log, supplemental",
                1,
            ),
            entry(
                SHEET,
                FileKind::Spreadsheet,
                7,
                "Engineering duty roster",
                1,
            ),
            viewer,
        ],
        vec![entry(
            DECK,
            FileKind::Presentation,
            30,
            "Away team briefing",
            1,
        )],
    );
    put(
        t,
        api::req_json(&api::comments_url(VIEW_ONLY_DOC, None)),
        json_response(&json!({"kind": "drive#commentList", "comments": []})),
    );
    put(
        t,
        api::req_bytes(&format!(
            "{}/documents/{VIEW_ONLY_DOC}?includeTabsContent=true&suggestionsViewMode=SUGGESTIONS_INLINE",
            api::DOCS
        )),
        bytes(
            403,
            br#"{"error": {"code": 403, "message": "You do not have permission to access the document suggestions.",
                "status": "PERMISSION_DENIED"}}"#,
        ),
    );
    put(
        t,
        api::req_bytes(&format!(
            "{}/documents/{VIEW_ONLY_DOC}?includeTabsContent=true",
            api::DOCS
        )),
        json_response(&view_only_native()),
    );
    if may_download {
        let (odt, _) = api::export_type(FileKind::Document, ExportFormat::Odf);
        put(
            t,
            api::req_bytes(&api::export_url(VIEW_ONLY_DOC, odt)),
            bytes(200, VIEW_ONLY_ODT),
        );
    }
}

pub fn view_only_native() -> Value {
    json!({
        "documentId": VIEW_ONLY_DOC,
        "title": "Counseling notes",
        "revisionId": "ALm37BW-troi5",
        "suggestionsViewMode": "PREVIEW_WITHOUT_SUGGESTIONS",
        "tabs": [{"tabProperties": {"tabId": "t.0", "title": "Tab 1", "index": 0},
                  "documentTab": {"body": {"content": [{"startIndex": 1, "endIndex": 22, "paragraph": {
                      "elements": [{"startIndex": 1, "endIndex": 22, "textRun": {
                          "content": "The crew is anxious.\n"}}]}}]}}}]
    })
}
