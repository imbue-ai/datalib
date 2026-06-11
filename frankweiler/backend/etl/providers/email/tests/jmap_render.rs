//! End-to-end render test: build a tiny in-memory `LoadedRaw` whose
//! `blobs` reader carries real RFC 5322 `.eml` bytes for each email,
//! render it through `render_all`, and assert the on-disk layout +
//! grid_rows sidecar shape. Doesn't need a real JMAP server or HTTP
//! playback — exercises the renderer in isolation.

use std::collections::HashMap;
#[allow(unused_imports)]
use std::path::PathBuf;

use frankweiler_etl::blob_cas::{blake3_hex, BlobView, InMemoryBlobReader};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_email::extract::db::{EmailJoins, LoadedAttachment, LoadedEmail, LoadedRaw};
use frankweiler_etl_email::translate::render::{render_all, thread_uuid};
use serde_json::json;

const EML_E1: &str = "From: Alice <a@x.test>\r\n\
                      To: Bob <b@x.test>\r\n\
                      Subject: Hello\r\n\
                      Date: Thu, 1 Jan 2026 00:00:00 +0000\r\n\
                      Content-Type: text/plain; charset=utf-8\r\n\
                      \r\n\
                      first message body\r\n";

const EML_E2: &str = "From: Bob <b@x.test>\r\n\
                      Subject: Re: Hello\r\n\
                      Date: Fri, 2 Jan 2026 00:00:00 +0000\r\n\
                      Content-Type: text/plain; charset=utf-8\r\n\
                      \r\n\
                      reply body\r\n";

fn insert_eml(reader: &mut InMemoryBlobReader, ref_id: &str, owning_id: &str, body: &[u8]) {
    reader.insert(BlobView {
        ref_id: ref_id.into(),
        owning_id: owning_id.into(),
        slot: "source".into(),
        blake3: blake3_hex(body),
        content_type: Some("message/rfc822".into()),
        upstream_name: None,
        source_url: None,
        bytes: body.to_vec(),
    });
}

fn make_loaded() -> LoadedRaw {
    let account = json!({"id": "A1", "name": "thad@example.com", "isPersonal": true});
    let mailbox = json!({"id": "M-inbox", "name": "Inbox", "role": "inbox"});
    let thread = json!({"id": "T1", "emailIds": ["E1", "E2"]});

    let emails = vec![
        LoadedEmail {
            id: "E1".into(),
            account_id: "A1".into(),
            thread_id: "T1".into(),
            blob_id: "B-eml-1".into(),
            message_id: None,
            received_at: Some("2026-01-01T00:00:00Z".into()),
            sent_at: None,
            size: Some(EML_E1.len() as i64),
            subject: Some("Hello".into()),
            from_json: Some(r#"[{"name":"Alice","email":"a@x.test"}]"#.into()),
            has_attachment: false,
        },
        LoadedEmail {
            id: "E2".into(),
            account_id: "A1".into(),
            thread_id: "T1".into(),
            blob_id: "B-eml-2".into(),
            message_id: None,
            received_at: Some("2026-01-02T00:00:00Z".into()),
            sent_at: None,
            size: Some(EML_E2.len() as i64),
            subject: Some("Re: Hello".into()),
            from_json: Some(r#"[{"name":"Bob","email":"b@x.test"}]"#.into()),
            has_attachment: true,
        },
    ];

    let mut joins = EmailJoins::default();
    joins.mailboxes.insert("E1".into(), vec!["M-inbox".into()]);
    joins.mailboxes.insert("E2".into(), vec!["M-inbox".into()]);
    joins.keywords.insert("E1".into(), vec!["$seen".into()]);
    joins
        .keywords
        .insert("E2".into(), vec!["$seen".into(), "$flagged".into()]);
    joins.attachments.insert(
        "E2".into(),
        vec![LoadedAttachment {
            part_id: "2".into(),
            blob_id: "B-att-1".into(),
            name: Some("doc.pdf".into()),
            content_type: Some("application/pdf".into()),
            size: Some(12),
            disposition: Some("attachment".into()),
            cid: None,
        }],
    );

    let mut reader = InMemoryBlobReader::new();
    insert_eml(&mut reader, "B-eml-1", "E1", EML_E1.as_bytes());
    insert_eml(&mut reader, "B-eml-2", "E2", EML_E2.as_bytes());
    let att_bytes = b"hello-pdf-12";
    reader.insert(BlobView {
        ref_id: "B-att-1".into(),
        owning_id: "E2".into(),
        slot: "2".into(),
        blake3: blake3_hex(att_bytes),
        content_type: Some("application/pdf".into()),
        upstream_name: Some("hello.pdf".into()),
        source_url: None,
        bytes: att_bytes.to_vec(),
    });

    LoadedRaw {
        accounts: vec![account],
        mailboxes: vec![mailbox],
        threads: vec![thread],
        emails,
        joins,
        blobs: reader.into_handle(),
    }
}

#[test]
fn render_smoke_produces_thread_dir_with_md_and_sidecar() {
    let parsed = make_loaded();
    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let prior: HashMap<String, String> = HashMap::new();
    let mut completed: Vec<RenderedMarkdown> = Vec::new();
    let mut on_done = |md: RenderedMarkdown| -> anyhow::Result<()> {
        completed.push(md);
        Ok(())
    };
    let written = render_all(
        &parsed,
        tmp.path(),
        "fastmail",
        &progress,
        &prior,
        &mut on_done,
    )
    .expect("render_all");
    assert_eq!(written.len(), 1, "one thread → one rendered md");
    assert_eq!(completed.len(), 1, "one on_doc_complete call");

    let tuid = thread_uuid("A1", "T1");
    let page_dir = tmp
        .path()
        .join("rendered_md/jmap")
        .join("thad_example_com")
        .join(&tuid);
    assert!(page_dir.join("index.md").exists(), "index.md missing");
    assert!(
        page_dir.join("index.grid_rows.json").exists(),
        "sidecar missing"
    );
    let blobs_dir = page_dir.join("blobs");
    assert!(blobs_dir.is_dir(), "blobs/ dir missing");
    let mut entries: Vec<_> = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    entries.retain(|e| e.path().is_file());
    assert_eq!(entries.len(), 1, "expected exactly one materialized blob");

    let md = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    assert!(md.contains("subject: \"Hello\""));
    assert!(md.contains("thread_id: \"T1\""));
    assert!(md.contains("Alice"));
    assert!(md.contains("doc.pdf"));

    let sidecar: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(page_dir.join("index.grid_rows.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(sidecar["header"]["markdown_uuid"], tuid);
    let rows = sidecar["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "1 thread + 2 emails");
    assert_eq!(rows[0]["kind"], "Email Thread");
    assert_eq!(rows[1]["kind"], "Email");
    assert_eq!(rows[2]["kind"], "Email");
    assert_eq!(rows[0]["provider"], "jmap");
    assert_eq!(rows[0]["source_label"], "Mail");
}
