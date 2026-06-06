//! End-to-end render test: build a tiny in-memory `LoadedRaw`,
//! render it through `render_all`, and assert the on-disk layout +
//! grid_rows sidecar shape. Doesn't need a real JMAP server or
//! HTTP playback — exercises the renderer in isolation.

use std::collections::HashMap;
#[allow(unused_imports)]
use std::path::PathBuf;
use std::sync::Arc;

use frankweiler_etl::blob_store::InMemoryBlobStore;
use frankweiler_etl::doltlite_raw::BlobBytes;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_email::extract::db::{EmailJoins, LoadedAttachment, LoadedEmail, LoadedRaw};
use frankweiler_etl_email::translate::render::{render_all, thread_uuid};
use serde_json::json;

fn make_loaded() -> LoadedRaw {
    let account = json!({"id": "A1", "name": "thad@example.com", "isPersonal": true});
    let mailbox = json!({"id": "M-inbox", "name": "Inbox", "role": "inbox"});
    let thread = json!({"id": "T1", "emailIds": ["E1", "E2"]});
    let email_1_payload = json!({
        "id": "E1",
        "blobId": "B-eml-1",
        "threadId": "T1",
        "mailboxIds": {"M-inbox": true},
        "keywords": {"$seen": true},
        "from": [{"name": "Alice", "email": "a@x.test"}],
        "to": [{"name": "Bob", "email": "b@x.test"}],
        "subject": "Hello",
        "receivedAt": "2026-01-01T00:00:00Z",
        "preview": "first message",
        "bodyValues": {"1": {"value": "first message body\n"}},
        "textBody": [{"partId": "1", "type": "text/plain"}],
        "hasAttachment": false,
    });
    let email_2_payload = json!({
        "id": "E2",
        "blobId": "B-eml-2",
        "threadId": "T1",
        "mailboxIds": {"M-inbox": true},
        "keywords": {"$seen": true, "$flagged": true},
        "from": [{"name": "Bob", "email": "b@x.test"}],
        "subject": "Re: Hello",
        "receivedAt": "2026-01-02T00:00:00Z",
        "preview": "reply with attachment",
        "bodyValues": {"1": {"value": "reply body\n"}},
        "textBody": [{"partId": "1", "type": "text/plain"}],
        "hasAttachment": true,
        "attachments": [{"partId": "2", "blobId": "B-att-1", "name": "doc.pdf",
                         "type": "application/pdf", "size": 12}],
    });

    let emails = vec![
        LoadedEmail {
            id: "E1".into(),
            account_id: "A1".into(),
            thread_id: "T1".into(),
            blob_id: "B-eml-1".into(),
            message_id: None,
            received_at: Some("2026-01-01T00:00:00Z".into()),
            sent_at: None,
            size: Some(100),
            subject: Some("Hello".into()),
            has_attachment: false,
            payload: email_1_payload,
        },
        LoadedEmail {
            id: "E2".into(),
            account_id: "A1".into(),
            thread_id: "T1".into(),
            blob_id: "B-eml-2".into(),
            message_id: None,
            received_at: Some("2026-01-02T00:00:00Z".into()),
            sent_at: None,
            size: Some(200),
            subject: Some("Re: Hello".into()),
            has_attachment: true,
            payload: email_2_payload,
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

    let mut bytes: HashMap<String, BlobBytes> = HashMap::new();
    bytes.insert(
        "B-att-1".into(),
        BlobBytes {
            id: "B-att-1".into(),
            owning_id: "E2".into(),
            slot: "2".into(),
            content_type: Some("application/pdf".into()),
            bytes: b"hello-pdf-12".to_vec(),
            source_url: None,
        },
    );

    LoadedRaw {
        accounts: vec![account],
        mailboxes: vec![mailbox],
        threads: vec![thread],
        emails,
        joins,
        blobs: Arc::new(InMemoryBlobStore::from_id_map(bytes)),
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
    assert!(
        page_dir.join("blobs/doc.pdf").exists(),
        "attachment not materialized"
    );

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
