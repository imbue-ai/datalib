//! The Apple Mail mode over the TNG Mail folder
//! (`//datalib/backend/etl/providers/email:tng_apple_mail`): a Gmail
//! account and a Fastmail one, written by a stock SQLite and laid out the
//! way Mail lays them out. `tests/fixtures/make_apple_mail_folder.py` says
//! what is in it; this checks the download and the render of each part.

use std::path::{Path, PathBuf};

use datalib_etl::store_handle::RawStoreHandle as _;
use datalib_etl_email::ingest::apple_mail::{self, AppleMailAccountConfig, FetchSummary};
use datalib_etl_email::ingest::db::{db_path_for, RawDb};
use datalib_etl_email_render::render::parse::open;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_render::pages::PageSize;

const GMAIL: &str = "apple_mail_1f0b8a52";
const FASTMAIL: &str = "apple_mail_5d3c9e70";

/// The fixture unpacked where the test may read it as Mail's folder.
fn mail_folder(dir: &Path) -> PathBuf {
    let tar = std::env::var("APPLE_MAIL_TNG_TAR").expect("APPLE_MAIL_TNG_TAR is set by the BUILD");
    let file = std::fs::File::open(&tar).unwrap_or_else(|e| panic!("open {tar}: {e}"));
    let root = dir.join("Mail");
    tar::Archive::new(file).unpack(&root).unwrap();
    root
}

async fn sync(mail: &Path, work: &Path) -> FetchSummary {
    let db = RawDb::open(&work.join("e.doltlite_db")).await.unwrap();
    let cache = FingerprintCache::open(&work.join("fingerprints.sqlite"))
        .await
        .unwrap();
    let summary = apple_mail::fetch(apple_mail::FetchOptions {
        db: db.clone(),
        path: mail.to_path_buf(),
        account: AppleMailAccountConfig::default(),
        only_labels: Vec::new(),
        whole_threads: true,
        cache,
        progress: datalib_etl::progress::Progress::noop(),
        control: datalib_etl::control::DownloadControl::default(),
    })
    .await
    .expect("apple mail fetch");
    datalib_etl::doltlite_raw::commit_run(db.pool(), "test: apple mail fetch")
        .await
        .unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;
    summary
}

async fn rows<T>(work: &Path, sql: &'static str) -> Vec<T>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
{
    let db = RawDb::open(&work.join("e.doltlite_db")).await.unwrap();
    let out = sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    out
}

fn id(account: &str, message_id: &str) -> String {
    format!("{account}:{message_id}@enterprise.example")
}

/// The decoded bytes Mail kept beside a partial file, from the unpacked
/// folder: what the stored message must hold for that part.
fn kept(mail: &Path, rowid: u32, part: &str, name: &str) -> Vec<u8> {
    let path = mail
        .join("V10/1F0B8A52-0000-4000-8000-00000000E1D0/[Gmail].mbox/All Mail.mbox")
        .join("6A1D2C3B-0000-4000-8000-0000000000BB/Data/Attachments")
        .join(rowid.to_string())
        .join(part)
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tng_mail_folder_is_copied_and_rendered() {
    let dir = tempfile::tempdir().unwrap();
    let mail = mail_folder(dir.path());
    let work = tempfile::tempdir().unwrap();

    let first = sync(&mail, work.path()).await;
    assert_eq!(first.emails_upserted, 10, "{first:?}");
    assert_eq!(first.emails_without_file, 1, "{first:?}");
    assert_eq!(first.emails_missing_attachments, 1, "{first:?}");
    assert_eq!(first.read_errors, 0, "{first:?}");

    // Every account with mail, each under its own id; On My Mac's empty
    // Outbox makes no account. The deleted message is not here, and the
    // one only in the index's WAL is.
    let fleet = "fleet-advisory@starfleet.example";
    let mut expected: Vec<(String, String)> = [
        (GMAIL, "briefing"),
        (GMAIL, "reply"),
        (GMAIL, "report"),
        (GMAIL, "schematic"),
        (GMAIL, "orders"),
        (FASTMAIL, "risa"),
        (FASTMAIL, "risa-reply"),
        (FASTMAIL, "late-arrival"),
    ]
    .iter()
    .map(|(a, m)| (id(a, m), a.to_string()))
    .chain([GMAIL, FASTMAIL].map(|a| (format!("{a}:{fleet}"), a.to_string())))
    .collect();
    expected.sort();
    let emails: Vec<(String, String)> =
        rows(work.path(), "SELECT id, account_id FROM emails ORDER BY id").await;
    assert_eq!(emails, expected);
    let accounts: Vec<(String,)> = rows(work.path(), "SELECT id FROM accounts ORDER BY id").await;
    assert_eq!(accounts, [(GMAIL.to_string(),), (FASTMAIL.to_string(),)]);

    // Gmail labels are rows of `labels`; Fastmail folders are files, and a
    // message in two of them is one email in both.
    let filed: Vec<(String, String)> = rows(
        work.path(),
        "SELECT j.email_id, m.name FROM email_mailboxes j JOIN mailboxes m ON m.id = j.mailbox_id
         ORDER BY j.email_id, m.name",
    )
    .await;
    let filed_under = |email: &str| -> Vec<&str> {
        filed
            .iter()
            .filter(|(e, _)| e == email)
            .map(|(_, m)| m.as_str())
            .collect()
    };
    assert_eq!(
        filed_under(&id(GMAIL, "orders")),
        ["Inbox", "[Gmail]/All Mail", "[Gmail]/Sent Mail"]
    );
    assert_eq!(
        filed_under(&id(GMAIL, "report")),
        ["Away Team/Reports", "[Gmail]/All Mail"]
    );
    assert_eq!(
        filed_under(&id(FASTMAIL, "risa")),
        ["travel", "travel/portugal"]
    );

    // Read, answered and flagged from the index; a thread by its first
    // message.
    let read: Vec<(String, i64)> =
        rows(work.path(), "SELECT id, read FROM read_marks ORDER BY id").await;
    let is_read = |email: String| read.iter().find(|(e, _)| *e == email).map(|(_, r)| *r);
    assert_eq!(is_read(id(GMAIL, "briefing")), Some(1));
    assert_eq!(is_read(id(GMAIL, "reply")), Some(0));
    let keywords: Vec<(String, String)> = rows(
        work.path(),
        "SELECT email_id, keyword FROM email_keywords ORDER BY email_id, keyword",
    )
    .await;
    assert_eq!(
        keywords,
        [
            (id(GMAIL, "report"), "$answered".to_string()),
            (id(GMAIL, "schematic"), "$flagged".to_string()),
        ]
    );
    let threads: Vec<(String, String)> =
        rows(work.path(), "SELECT id, thread_id FROM emails ORDER BY id").await;
    let thread_of = |email: String| threads.iter().find(|(e, _)| *e == email).unwrap().1.clone();
    assert_eq!(thread_of(id(GMAIL, "reply")), id(GMAIL, "briefing"));
    assert_eq!(thread_of(id(FASTMAIL, "risa-reply")), id(FASTMAIL, "risa"));

    let problems: Vec<(String, String)> = rows(
        work.path(),
        "SELECT scope_key, severity FROM problems ORDER BY scope_key",
    )
    .await;
    assert_eq!(
        problems,
        [
            ("phase:apple_mail_files".to_string(), "warning".to_string()),
            (
                format!("record:emails:{}", id(GMAIL, "report")),
                "warning".to_string()
            ),
        ]
    );

    // The pages: one per thread, with the parts Mail kept beside a partial
    // file linked at addresses the server answers, and the inline image
    // drawn where its `cid:` was.
    let db_path = work.path().join("e.doltlite_db");
    let store = open(
        &db_path_for(&db_path),
        "star-trek-mbox",
        RawRange::cold(),
        false,
    )
    .expect("open")
    .expect("a committed store");
    let pages = tempfile::tempdir().unwrap();
    let (docs, _) = super::jmap_mbox::render_paged(
        &store,
        pages.path(),
        &[],
        PageSize {
            buckets: 3,
            bytes: 1 << 30,
        },
    );
    store.close();
    assert_eq!(docs.len(), 8, "one page per thread");
    let page_with = |needle: &str| -> String {
        docs.iter()
            .map(|d| std::fs::read_to_string(&d.md_path).unwrap())
            .find(|md| md.contains(needle))
            .unwrap_or_else(|| panic!("no page mentions {needle}"))
    };
    let cas = datalib_etl::blob_cas::cas_path_for(&db_path);

    let schematic = page_with("Warp core schematic");
    let linked = super::jmap_mbox::parts_linked(&schematic, &cas).await;
    let bytes_of = |content_type: &str| -> Vec<u8> {
        linked
            .iter()
            .find(|p| p.content_type.as_deref() == Some(content_type))
            .unwrap_or_else(|| panic!("no {content_type} part linked:\n{schematic}"))
            .bytes
            .clone()
    };
    assert_eq!(bytes_of("image/png"), kept(&mail, 4, "1.2", "conduit.png"));
    assert_eq!(bytes_of("image/jpeg"), kept(&mail, 4, "2", "core.jpg"));
    assert!(
        schematic
            .lines()
            .any(|l| l.contains("![conduits](") && l.contains("/part/")),
        "the inline image is not drawn:\n{schematic}"
    );
    assert!(!schematic.contains("cid:"), "a cid: is left:\n{schematic}");

    let report = page_with("Away team report");
    let pdf = super::jmap_mbox::parts_linked(&report, &cas)
        .await
        .into_iter()
        .find(|p| p.content_type.as_deref() == Some("application/pdf"))
        .unwrap_or_else(|| panic!("the report is not linked:\n{report}"));
    assert_eq!(pdf.bytes, kept(&mail, 3, "2", "report.pdf"));

    // Nothing moved in Mail: nothing is read or written.
    let second = sync(&mail, work.path()).await;
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(second.emails_unchanged, 10, "{second:?}");
}
