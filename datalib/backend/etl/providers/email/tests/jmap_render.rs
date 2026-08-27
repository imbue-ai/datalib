//! End-to-end render test: build a tiny in-memory `ParsedEmail`
//! whose per-bucket `BlobBundle` carries real RFC 5322 `.eml` bytes
//! for each email, render it through `render_all`, and assert the
//! on-disk layout + grid_rows sidecar shape. Doesn't need a real
//! JMAP server or HTTP playback — exercises the renderer in
//! isolation.

#[allow(unused_imports)]
use std::path::PathBuf;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl_email::download::db::{EmailJoins, LoadedAttachment, LoadedEmail};
use datalib_etl_email::render::parse::{EmailThreadBucket, ParsedEmail, ScanResult};
use datalib_etl_email::render::render::{render_all, thread_uuid, OutlinkFormat};
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

fn insert_eml(bundle: &mut BlobBundle, ref_id: &str, body: &[u8]) {
    bundle.add(ref_id, body.to_vec(), Some("message/rfc822".into()), None);
}

fn make_loaded() -> ParsedEmail {
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
            in_reply_to: None,
            references: None,
            received_at: Some("2026-01-01T00:00:00Z".into()),
            sent_at: None,
            size: Some(EML_E1.len() as i64),
            subject: Some("Hello".into()),
            from_json: Some(r#"[{"name":"Alice","email":"a@x.test"}]"#.into()),
            to_json: None,
            cc_json: None,
            has_attachment: false,
        },
        LoadedEmail {
            id: "E2".into(),
            account_id: "A1".into(),
            thread_id: "T1".into(),
            blob_id: "B-eml-2".into(),
            message_id: None,
            in_reply_to: None,
            references: None,
            received_at: Some("2026-01-02T00:00:00Z".into()),
            sent_at: None,
            size: Some(EML_E2.len() as i64),
            subject: Some("Re: Hello".into()),
            from_json: Some(r#"[{"name":"Bob","email":"b@x.test"}]"#.into()),
            to_json: None,
            cc_json: None,
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

    let mut bundle = BlobBundle::new();
    insert_eml(&mut bundle, "B-eml-1", EML_E1.as_bytes());
    insert_eml(&mut bundle, "B-eml-2", EML_E2.as_bytes());
    bundle.add(
        "B-att-1",
        b"hello-pdf-12".to_vec(),
        Some("application/pdf".into()),
        Some("hello.pdf".into()),
    );

    ParsedEmail {
        accounts: vec![account],
        mailboxes: vec![mailbox],
        threads: vec![thread],
        docs: vec![EmailThreadBucket {
            account_id: "A1".into(),
            thread_id: "T1".into(),
            emails,
            joins,
            blobs: bundle,
        }],
        docs_skipped: 0,
        scan: ScanResult {
            changed_threads: None,
            new_head: None,
            scan_elapsed: None,
        },
    }
}

#[test]
fn render_smoke_produces_thread_dir_with_md_and_sidecar() {
    let parsed = make_loaded();
    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut completed: Vec<RenderedMarkdown> = Vec::new();
    let mut on_done = |md: RenderedMarkdown| -> anyhow::Result<()> {
        completed.push(md);
        Ok(())
    };
    render_all(
        &parsed,
        tmp.path(),
        "fastmail",
        Some(OutlinkFormat::Fastmail),
        &[],
        &progress,
        &mut on_done,
    )
    .expect("render_all");
    assert_eq!(completed.len(), 1, "one on_doc_complete call");

    // chat-common owns the page-dir layout
    // (rendered_md/jmap/<source>/chat-<id>__<slug>__<short>/all.md); find
    // the single rendered doc by walking rather than hard-coding the slug.
    let md_path = find_one(tmp.path(), ".md");
    let sidecar_path = find_one(tmp.path(), ".grid_rows.json");
    let page_dir = md_path.parent().unwrap();

    let blobs_dir = page_dir.join("blobs");
    assert!(blobs_dir.is_dir(), "blobs/ dir missing");
    let blobs: Vec<_> = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();
    assert_eq!(blobs.len(), 1, "expected exactly one materialized blob");

    let tuid = thread_uuid("A1", "T1");
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md.contains("display: \"Hello\""),
        "subject as display: {md}"
    );
    assert!(md.contains("external_id: T1"), "thread_id as external_id");
    assert!(md.contains("Alice"), "sender in a message header");
    assert!(md.contains("doc.pdf"), "attachment listed");
    assert!(md.contains("🏷 Inbox"), "mailbox label chip rendered");
    // Fastmail outlink: /mail/<mailbox>/<emailId>.<threadId>.
    assert!(
        md.contains("https://app.fastmail.com/mail/Inbox/E1.T1"),
        "fastmail outlink for root email: {md}"
    );

    // The thread (chat) row and the root email row both carry the outlink.
    let rows = {
        let s: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        s["rows"].as_array().unwrap().clone()
    };
    assert_eq!(
        rows[0]["source_url"], "https://app.fastmail.com/mail/Inbox/E1.T1",
        "thread row outlink"
    );
    assert_eq!(
        rows[1]["source_url"], "https://app.fastmail.com/mail/Inbox/E1.T1",
        "root email row outlink"
    );

    let sidecar: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(sidecar["header"]["markdown_uuid"], tuid);
    let rows = sidecar["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "1 thread + 2 emails");
    assert_eq!(rows[0]["kind"], "Email Thread");
    assert_eq!(rows[1]["kind"], "Email");
    assert_eq!(rows[2]["kind"], "Email");
    assert_eq!(rows[0]["provider"], "jmap");
    assert_eq!(rows[0]["source_label"], "Mail");
}

/// Find the single file under `root` whose name ends with `suffix`.
fn find_one(root: &std::path::Path, suffix: &str) -> PathBuf {
    fn walk(dir: &std::path::Path, suffix: &str, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, suffix, out);
            } else if p.to_string_lossy().ends_with(suffix) {
                out.push(p);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, suffix, &mut found);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one *{suffix} under {root:?}"
    );
    found.pop().unwrap()
}

// ─────────────────────────────────────────────────────────────────────
// Calendar invites: the same payload arriving twice
// ─────────────────────────────────────────────────────────────────────

/// The iCalendar payload, byte-identical in both places it arrives.
const ICS: &str = "BEGIN:VCALENDAR\r\n\
                   METHOD:REQUEST\r\n\
                   BEGIN:VEVENT\r\n\
                   SUMMARY:Standup\r\n\
                   END:VEVENT\r\n\
                   END:VCALENDAR";

/// A Google-Calendar-shaped invite: `multipart/mixed` wrapping a
/// `multipart/alternative` whose third alternative is an inline
/// `text/calendar; method=REQUEST` part, plus an `invite.ics`
/// attachment part carrying the very same bytes.
fn invite_eml() -> String {
    two_copy_eml(
        "text/calendar; method=REQUEST",
        "application/ics",
        "invite.ics",
        ICS,
    )
}

/// One payload carried twice: an inline `alt_type` part inside a
/// `multipart/alternative`, and an `att_name` attachment part with the
/// very same bytes.
fn two_copy_eml(alt_type: &str, att_type: &str, att_name: &str, payload: &str) -> String {
    format!(
        "From: Organizer <o@x.test>\r\n\
         To: Bob <b@x.test>\r\n\
         Subject: Invitation: Standup\r\n\
         Date: Sat, 3 Jan 2026 00:00:00 +0000\r\n\
         Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\
         \r\n\
         --OUT\r\n\
         Content-Type: multipart/alternative; boundary=\"IN\"\r\n\
         \r\n\
         --IN\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         You have been invited to Standup.\r\n\
         --IN\r\n\
         Content-Type: {alt_type}; charset=utf-8\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         \r\n\
         {payload}\r\n\
         --IN--\r\n\
         --OUT\r\n\
         Content-Type: {att_type}; name=\"{att_name}\"\r\n\
         Content-Disposition: attachment; filename=\"{att_name}\"\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         \r\n\
         {payload}\r\n\
         --OUT--\r\n"
    )
}

fn make_invite() -> ParsedEmail {
    make_two_copy(invite_eml(), "application/ics", "invite.ics", ICS)
}

fn make_two_copy(eml: String, att_type: &str, att_name: &str, payload: &str) -> ParsedEmail {
    let account = json!({"id": "A1", "name": "thad@example.com", "isPersonal": true});
    let mailbox = json!({"id": "M-inbox", "name": "Inbox", "role": "inbox"});
    let thread = json!({"id": "T9", "emailIds": ["E9"]});

    let emails = vec![LoadedEmail {
        id: "E9".into(),
        account_id: "A1".into(),
        thread_id: "T9".into(),
        blob_id: "B-eml-9".into(),
        message_id: None,
        in_reply_to: None,
        references: None,
        received_at: Some("2026-01-03T00:00:00Z".into()),
        sent_at: None,
        size: Some(eml.len() as i64),
        subject: Some("Invitation: Standup".into()),
        from_json: Some(r#"[{"name":"Organizer","email":"o@x.test"}]"#.into()),
        to_json: None,
        cc_json: None,
        has_attachment: true,
    }];

    let mut joins = EmailJoins::default();
    joins.mailboxes.insert("E9".into(), vec!["M-inbox".into()]);
    joins.attachments.insert(
        "E9".into(),
        vec![LoadedAttachment {
            part_id: "3".into(),
            blob_id: "B-ics".into(),
            name: Some(att_name.into()),
            content_type: Some(att_type.into()),
            size: Some(payload.len() as i64),
            disposition: Some("attachment".into()),
            cid: None,
        }],
    );

    let mut bundle = BlobBundle::new();
    insert_eml(&mut bundle, "B-eml-9", eml.as_bytes());
    // The attachment part's bytes as the downloader stored them —
    // identical to what the inline MIME part decodes to.
    bundle.add(
        "B-ics",
        payload.as_bytes().to_vec(),
        Some(att_type.into()),
        Some(att_name.into()),
    );

    ParsedEmail {
        accounts: vec![account],
        mailboxes: vec![mailbox],
        threads: vec![thread],
        docs: vec![EmailThreadBucket {
            account_id: "A1".into(),
            thread_id: "T9".into(),
            emails,
            joins,
            blobs: bundle,
        }],
        docs_skipped: 0,
        scan: ScanResult {
            changed_threads: None,
            new_head: None,
            scan_elapsed: None,
        },
    }
}

/// A calendar invite carries one iCalendar payload that arrives twice —
/// as an inline `text/calendar; method=REQUEST` MIME part (no upstream
/// filename) and as an `invite.ics` attachment part. Both used to be
/// materialized: `blobs/<stem>` beside `blobs/<stem>.ics`,
/// byte-identical, with the extensionless copy spliced into the body as
/// a broken `![](…)` image and only the `.ics` copy linked from the
/// "### Attachments" list.
#[test]
fn calendar_invite_materializes_one_blob_linked_once() {
    let parsed = make_invite();
    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut on_done = |_: RenderedMarkdown| -> anyhow::Result<()> { Ok(()) };
    render_all(
        &parsed,
        tmp.path(),
        "gmail",
        Some(OutlinkFormat::Gmail),
        &[],
        &progress,
        &mut on_done,
    )
    .expect("render_all");

    let md_path = find_one(tmp.path(), ".md");
    let blobs_dir = md_path.parent().unwrap().join("blobs");
    let mut names: Vec<String> = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        1,
        "one payload → one file; got byte-identical duplicates: {names:?}"
    );
    assert!(
        names[0].ends_with(".ics"),
        "keeps the openable name, not the bare hex stem: {}",
        names[0]
    );

    let md = std::fs::read_to_string(&md_path).unwrap();
    let links: Vec<&str> = md.match_indices("blobs/").map(|(i, _)| &md[i..]).collect();
    assert_eq!(
        links.len(),
        1,
        "linked exactly once, from the attachment list"
    );
    assert!(
        md.contains(&format!("[invite.ics](blobs/{})", names[0])),
        "the one link is the attachment-list entry: {md}"
    );
    assert!(
        !md.contains("![](blobs/"),
        "a calendar part is not an image: {md}"
    );
}

/// The narrow half of the same rule: an inline part that is also a
/// listed attachment is skipped only when it is *not* an image. An
/// image keeps its inline preview — seeing the picture beats a second
/// link to it — so this must NOT get swept up in the invite fix.
#[test]
fn inline_image_that_is_also_an_attachment_keeps_its_preview() {
    const PNG: &str = "not-really-png-but-bytes-are-bytes";
    let parsed = make_two_copy(
        two_copy_eml("image/png", "image/png", "shot.png", PNG),
        "image/png",
        "shot.png",
        PNG,
    );
    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut on_done = |_: RenderedMarkdown| -> anyhow::Result<()> { Ok(()) };
    render_all(
        &parsed,
        tmp.path(),
        "fastmail",
        Some(OutlinkFormat::Fastmail),
        &[],
        &progress,
        &mut on_done,
    )
    .expect("render_all");

    let md_path = find_one(tmp.path(), ".md");
    let blobs_dir = md_path.parent().unwrap().join("blobs");
    let names: Vec<String> = std::fs::read_dir(&blobs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "still one payload, one file: {names:?}");

    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md.contains(&format!("![](blobs/{})", names[0])),
        "inline image preview survives: {md}"
    );
    assert!(
        md.contains(&format!("[shot.png](blobs/{})", names[0])),
        "and it is still listed as an attachment: {md}"
    );
}
