//! The Apple Mail mode end to end, over a made-up Mail folder: an
//! Envelope Index with the tables and columns the download reads, and
//! `.emlx` files laid out the way Mail lays them out (macOS 26, `V10`).

use std::path::{Path, PathBuf};

use datalib_etl::store_handle::RawStoreHandle as _;
use datalib_etl_email::ingest::apple_mail::{self, AppleMailAccountConfig, FetchSummary};
use datalib_etl_email::ingest::db::RawDb;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const ACCOUNT: &str = "1F0B8A52-0000-4000-8000-00000000E1D0";
const STORE: &str = "6A1D2C3B-0000-4000-8000-0000000000BB";
const ACCOUNT_ID: &str = "enterprise";

/// A Mail folder: `V10/MailData/Envelope Index` and one account's files.
struct MailFolder {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

const BRIEFING: &str = "From: Jean-Luc Picard <picard@enterprise.example>\n\
To: Senior Staff <staff@enterprise.example>\n\
Subject: Briefing at 0900\n\
Date: Mon, 05 Jan 2026 09:00:00 +0000\n\
Message-ID: <briefing@enterprise.example>\n\
\n\
Observation lounge, 0900.\n";

const REPLY: &str = "From: William Riker <riker@enterprise.example>\n\
To: Jean-Luc Picard <picard@enterprise.example>\n\
Subject: Re: Briefing at 0900\n\
Date: Mon, 05 Jan 2026 09:05:00 +0000\n\
Message-ID: <reply@enterprise.example>\n\
In-Reply-To: <briefing@enterprise.example>\n\
\n\
I'll be there.\n";

/// A `.partial.emlx`: Mail left the report out and keeps it beside the
/// file as part `2`; part `3`, an invitation, it has not downloaded.
const REPORT: &str = "From: Data <data@enterprise.example>\n\
To: Jean-Luc Picard <picard@enterprise.example>\n\
Subject: Away team report\n\
Date: Tue, 06 Jan 2026 14:00:00 +0000\n\
Message-ID: <report@enterprise.example>\n\
MIME-Version: 1.0\n\
Content-Type: multipart/mixed; boundary=b\n\
\n\
--b\n\
Content-Type: text/plain\n\
\n\
The report is attached.\n\
--b\n\
Content-Type: application/pdf; name=report.pdf\n\
Content-Disposition: attachment; filename=report.pdf\n\
Content-Transfer-Encoding: base64\n\
X-Apple-Content-Length: 64\n\
\n\
\n\
--b\n\
Content-Type: text/calendar; name=invite.ics\n\
Content-Transfer-Encoding: base64\n\
X-Apple-Content-Length: 30\n\
\n\
\n\
--b--\n";

fn report_pdf() -> Vec<u8> {
    (0..64u8).collect()
}

impl MailFolder {
    /// A folder with the index's tables, the account's mailboxes and no
    /// message.
    async fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Mail");
        std::fs::create_dir_all(root.join("V10/MailData")).unwrap();
        let folder = MailFolder { _dir: dir, root };
        std::fs::create_dir_all(folder.all_mail().join("Messages")).unwrap();
        folder
            .index(&[
                "CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL,
                    total_count INTEGER NOT NULL DEFAULT 0)",
                "CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                    message_id INTEGER, message_id_header TEXT)",
                "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                    message_id INTEGER NOT NULL DEFAULT 0, global_message_id INTEGER NOT NULL,
                    mailbox INTEGER NOT NULL, flags INTEGER NOT NULL DEFAULT 0,
                    read INTEGER NOT NULL DEFAULT 0, flagged INTEGER NOT NULL DEFAULT 0,
                    deleted INTEGER NOT NULL DEFAULT 0, conversation_id INTEGER NOT NULL DEFAULT 0,
                    date_received INTEGER)",
                "CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER,
                    PRIMARY KEY(message_id, mailbox_id)) WITHOUT ROWID",
                &format!(
                    "INSERT INTO mailboxes (ROWID, url) VALUES
                    (1, 'imap://{ACCOUNT}/INBOX'),
                    (2, 'imap://{ACCOUNT}/%5BGmail%5D/All%20Mail'),
                    (3, 'imap://{ACCOUNT}/Away%20Team'),
                    (4, 'local://2C7E0D11-0000-4000-8000-0000000000AA/Outbox')"
                ),
            ])
            .await;
        folder
    }

    /// The briefing and its reply, the away team's report, and a message
    /// Mail has not downloaded.
    async fn new() -> Self {
        let folder = Self::empty().await;
        let all_mail = folder.all_mail();
        let messages = all_mail.join("Messages");
        let deeper = all_mail.join("1/Messages");
        std::fs::create_dir_all(&deeper).unwrap();
        write_emlx(&messages.join("1.emlx"), BRIEFING);
        write_emlx(&deeper.join("1002.emlx"), REPLY);
        write_emlx(&messages.join("3.partial.emlx"), REPORT);
        folder.keep_part(3, "2", "report.pdf", &report_pdf());
        folder
            .index(&[
                "INSERT INTO message_global_data (ROWID, message_id_header) VALUES
                    (1, '<briefing@enterprise.example>'),
                    (2, '<reply@enterprise.example>'),
                    (3, '<report@enterprise.example>'),
                    (4, '<not-downloaded@enterprise.example>')",
                // Rowid 1002 is the reply: Mail spreads its files over
                // numbered folders as the rowids grow.
                "INSERT INTO messages (ROWID, global_message_id, mailbox, read, flags,
                    conversation_id, date_received) VALUES
                    (1, 1, 2, 1, 1, 10, 1767603600),
                    (1002, 2, 2, 0, 0, 10, 1767603900),
                    (3, 3, 2, 1, 5, 11, 1767708000),
                    (4, 4, 2, 0, 0, 12, 1767800000)",
                "INSERT INTO labels (message_id, mailbox_id) VALUES (1, 1), (1002, 1), (3, 3)",
            ])
            .await;
        folder
    }

    fn all_mail(&self) -> PathBuf {
        self.root
            .join("V10")
            .join(ACCOUNT)
            .join("[Gmail].mbox/All Mail.mbox")
            .join(STORE)
            .join("Data")
    }

    /// Keep `bytes` beside message `rowid` as its part `part`, the way
    /// Mail keeps what it left out of a `.partial.emlx`.
    fn keep_part(&self, rowid: i64, part: &str, name: &str, bytes: &[u8]) {
        let dir = self.all_mail().join(format!("Attachments/{rowid}/{part}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    /// Run `sql` on the Envelope Index, the way Mail changes it.
    async fn index(&self, sql: &[&str]) {
        let path = self.root.join("V10/MailData/Envelope Index");
        let opts = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        for s in sql {
            sqlx::query(sqlx::AssertSqlSafe(s.to_string()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{s}: {e}"));
        }
        pool.close().await;
    }
}

fn write_emlx(path: &Path, message: &str) {
    let mut out = format!("{:<10}\n", message.len()).into_bytes();
    out.extend_from_slice(message.as_bytes());
    out.extend_from_slice(
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict/>\n</plist>\n",
    );
    std::fs::write(path, out).unwrap();
}

/// The one account the single-account tests copy, under a name of its own.
fn enterprise() -> AppleMailAccountConfig {
    AppleMailAccountConfig {
        account: Some(ACCOUNT.to_string()),
        account_id: Some(ACCOUNT_ID.to_string()),
        ..AppleMailAccountConfig::default()
    }
}

/// One sync of `folder` into the store in `work`, committed the way the
/// processor commits it.
async fn sync(folder: &MailFolder, work: &Path) -> FetchSummary {
    sync_with(
        folder,
        work,
        enterprise(),
        datalib_etl::progress::Progress::noop(),
    )
    .await
}

async fn sync_with(
    folder: &MailFolder,
    work: &Path,
    account: AppleMailAccountConfig,
    progress: datalib_etl::progress::Progress,
) -> FetchSummary {
    let db = RawDb::open(&work.join("e.doltlite_db")).await.unwrap();
    let cache = FingerprintCache::open(&work.join("fingerprints.sqlite"))
        .await
        .unwrap();
    let summary = apple_mail::fetch(apple_mail::FetchOptions {
        db: db.clone(),
        path: folder.root.clone(),
        account,
        only_labels: Vec::new(),
        whole_threads: true,
        cache,
        progress,
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

/// What a download told its bar: its length, and each move.
#[derive(Default)]
struct Bar {
    length: std::sync::Mutex<Option<u64>>,
    moves: std::sync::Mutex<Vec<u64>>,
}

impl datalib_etl::progress::ProgressSink for Bar {
    fn set_length(&self, total: Option<u64>) {
        *self.length.lock().unwrap() = total;
    }
    fn inc(&self, delta: u64) {
        self.moves.lock().unwrap().push(delta);
    }
}

async fn commits(work: &Path) -> i64 {
    let n: Vec<(i64,)> = rows(work, "SELECT count(*) FROM dolt_log").await;
    n[0].0
}

/// Every problem row, as `<key> <severity>`.
async fn problems(work: &Path) -> Vec<String> {
    let rows: Vec<(String, String)> = rows(
        work,
        "SELECT scope_key, severity FROM problems ORDER BY scope_key",
    )
    .await;
    rows.into_iter().map(|(k, s)| format!("{k} {s}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mail_folder_mirrors_and_follows_mails_index() {
    let folder = MailFolder::new().await;
    let work = tempfile::tempdir().unwrap();

    let bar = std::sync::Arc::new(Bar::default());
    let first = sync_with(
        &folder,
        work.path(),
        enterprise(),
        datalib_etl::progress::Progress::new(bar.clone()),
    )
    .await;
    // The bar reaches its length, in moves of many emails: one event per
    // email is tens of thousands of them on a real account.
    let moves = bar.moves.lock().unwrap().clone();
    assert_eq!(Some(moves.iter().sum()), *bar.length.lock().unwrap());
    assert_eq!(moves.len(), 1, "{moves:?}");
    assert_eq!(first.emails_upserted, 3, "{first:?}");
    assert_eq!(first.emails_without_file, 1, "{first:?}");
    assert_eq!(first.emails_missing_attachments, 1, "{first:?}");

    let emails: Vec<(String, String, String)> = rows(
        work.path(),
        "SELECT id, thread_id, account_id FROM emails ORDER BY id",
    )
    .await;
    assert_eq!(
        emails,
        [
            (
                "enterprise:briefing@enterprise.example",
                "enterprise:briefing@enterprise.example",
                ACCOUNT_ID
            ),
            (
                "enterprise:reply@enterprise.example",
                "enterprise:briefing@enterprise.example",
                ACCOUNT_ID
            ),
            (
                "enterprise:report@enterprise.example",
                "enterprise:report@enterprise.example",
                ACCOUNT_ID
            ),
        ]
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    );
    let filed: Vec<(String, String)> = rows(
        work.path(),
        "SELECT j.email_id, m.name FROM email_mailboxes j JOIN mailboxes m ON m.id = j.mailbox_id
         ORDER BY j.email_id, m.name",
    )
    .await;
    let filed: Vec<(&str, &str)> = filed
        .iter()
        .map(|(e, m)| (e.as_str(), m.as_str()))
        .collect();
    assert_eq!(
        filed,
        [
            ("enterprise:briefing@enterprise.example", "Inbox"),
            ("enterprise:briefing@enterprise.example", "[Gmail]/All Mail"),
            ("enterprise:reply@enterprise.example", "Inbox"),
            ("enterprise:reply@enterprise.example", "[Gmail]/All Mail"),
            ("enterprise:report@enterprise.example", "Away Team"),
            ("enterprise:report@enterprise.example", "[Gmail]/All Mail"),
        ]
    );
    let read: Vec<(String, i64)> =
        rows(work.path(), "SELECT id, read FROM read_marks ORDER BY id").await;
    assert_eq!(
        read.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        [1, 0, 1],
        "{read:?}"
    );
    let answered: Vec<(String,)> = rows(
        work.path(),
        "SELECT email_id FROM email_keywords WHERE keyword = '$answered'",
    )
    .await;
    assert_eq!(
        answered,
        [("enterprise:report@enterprise.example".to_string(),)]
    );
    let expected_problems = [
        "phase:apple_mail_files warning",
        "record:emails:enterprise:report@enterprise.example warning",
    ];
    assert_eq!(problems(work.path()).await, expected_problems);

    // The report went in whole: its stored `.eml` holds the attachment
    // Mail kept beside the partial file.
    let blobs: Vec<(String,)> = rows(
        work.path(),
        "SELECT blob_id FROM emails WHERE id = 'enterprise:report@enterprise.example'",
    )
    .await;
    let db = RawDb::open(&work.path().join("e.doltlite_db"))
        .await
        .unwrap();
    let eml = db
        .cas()
        .get(&blobs[0].0)
        .await
        .unwrap()
        .expect("the report's .eml");
    db.close().await;
    let msg = mail_parser::MessageParser::default()
        .parse(&eml.bytes)
        .unwrap();
    let pdf = msg
        .parts()
        .find(|p| p.attachment_name() == Some("report.pdf"))
        .expect("the report is attached");
    assert_eq!(pdf.decoded().as_ref(), report_pdf().as_slice());

    // Nothing moved: nothing is written.
    let commits_before = commits(work.path()).await;
    let second = sync(&folder, work.path()).await;
    // Nothing moved in Mail, so nothing moved in the store.
    assert_eq!(commits(work.path()).await, commits_before);
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(second.emails_unchanged, 3, "{second:?}");
    // The report was not read again, and its missing part is still missing.
    assert_eq!(problems(work.path()).await, expected_problems);

    // Read in Mail, and one deleted there.
    folder
        .index(&[
            "UPDATE messages SET read = 1 WHERE ROWID = 1002",
            "DELETE FROM messages WHERE ROWID = 3",
        ])
        .await;
    let third = sync(&folder, work.path()).await;
    assert_eq!(third.emails_upserted, 1, "{third:?}");
    assert_eq!(third.emails_removed, 1, "{third:?}");
    assert_eq!(
        problems(work.path()).await,
        ["phase:apple_mail_files warning"]
    );
    let read: Vec<(String, i64)> =
        rows(work.path(), "SELECT id, read FROM read_marks ORDER BY id").await;
    assert_eq!(
        read,
        [
            ("enterprise:briefing@enterprise.example".to_string(), 1),
            ("enterprise:reply@enterprise.example".to_string(), 1),
        ]
    );
}

/// A 1×1 PNG.
const CONDUIT_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn core_jpeg() -> Vec<u8> {
    [&[0xff, 0xd8, 0xff, 0xe0][..], &[0x5a; 300], &[0xff, 0xd9]].concat()
}

/// A `.partial.emlx` whose HTML draws an image Mail kept beside the file
/// (part `1.2`, by `cid:`), with a photo attached (part `2`).
const SCHEMATIC: &str = "From: Geordi La Forge <laforge@enterprise.example>\n\
To: Jean-Luc Picard <picard@enterprise.example>\n\
Subject: Warp core schematic\n\
Date: Wed, 07 Jan 2026 10:00:00 +0000\n\
Message-ID: <schematic@enterprise.example>\n\
MIME-Version: 1.0\n\
Content-Type: multipart/mixed; boundary=outer\n\
\n\
--outer\n\
Content-Type: multipart/related; boundary=rel\n\
\n\
--rel\n\
Content-Type: text/html; charset=utf-8\n\
\n\
<p>The plasma conduits:</p><img src=\"cid:conduit@enterprise.example\" alt=\"conduits\">\n\
--rel\n\
Content-Type: image/png; name=conduit.png\n\
Content-ID: <conduit@enterprise.example>\n\
Content-Disposition: inline; filename=conduit.png\n\
Content-Transfer-Encoding: base64\n\
X-Apple-Content-Length: 94\n\
\n\
\n\
--rel--\n\
\n\
--outer\n\
Content-Type: image/jpeg; name=core.jpg\n\
Content-Disposition: attachment; filename=core.jpg\n\
Content-Transfer-Encoding: base64\n\
X-Apple-Content-Length: 410\n\
\n\
\n\
--outer--\n";

/// Images Mail kept beside a partial file reach the blob store inside the
/// message, and the page draws the inline one where its `cid:` was and
/// links the attached one, both at addresses the server answers.
#[tokio::test(flavor = "multi_thread")]
async fn images_mail_kept_aside_are_stored_and_drawn() {
    use datalib_etl_email::ingest::db::db_path_for;
    use datalib_etl_email_render::render::parse::open;
    use datalib_etl_render::inputs::RawRange;
    use datalib_etl_render::pages::PageSize;

    let folder = MailFolder::empty().await;
    write_emlx(
        &folder.all_mail().join("Messages/5.partial.emlx"),
        SCHEMATIC,
    );
    folder.keep_part(5, "1.2", "conduit.png", CONDUIT_PNG);
    folder.keep_part(5, "2", "core.jpg", &core_jpeg());
    folder
        .index(&[
            "INSERT INTO message_global_data (ROWID, message_id_header) VALUES
                (5, '<schematic@enterprise.example>')",
            "INSERT INTO messages (ROWID, global_message_id, mailbox, read, conversation_id,
                date_received) VALUES (5, 5, 2, 1, 20, 1767780000)",
        ])
        .await;
    let work = tempfile::tempdir().unwrap();
    let summary = sync(&folder, work.path()).await;
    assert_eq!(summary.emails_upserted, 1, "{summary:?}");
    assert_eq!(summary.emails_missing_attachments, 0, "{summary:?}");

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
            buckets: 10,
            bytes: 1 << 30,
        },
    );
    store.close();
    assert_eq!(docs.len(), 1);
    let md = std::fs::read_to_string(&docs[0].md_path).unwrap();

    let cas = datalib_etl::blob_cas::cas_path_for(&db_path);
    let linked = super::jmap_mbox::parts_linked(&md, &cas).await;
    let png = linked
        .iter()
        .find(|p| p.content_type.as_deref() == Some("image/png"))
        .unwrap_or_else(|| panic!("the inline image is not linked:\n{md}"));
    assert_eq!(png.bytes, CONDUIT_PNG);
    let jpeg = linked
        .iter()
        .find(|p| p.content_type.as_deref() == Some("image/jpeg"))
        .unwrap_or_else(|| panic!("the attached photo is not linked:\n{md}"));
    assert_eq!(jpeg.bytes, core_jpeg());

    // Drawn as an image where the HTML had it, not left pointing at `cid:`.
    assert!(
        md.lines()
            .any(|l| l.contains("![conduits](") && l.contains("/part/")),
        "the inline image is not drawn:\n{md}"
    );
    assert!(
        !md.contains("cid:conduit"),
        "a cid: reference is left:\n{md}"
    );
}

/// A Fastmail account beside the Gmail one: folders, not labels, so a
/// message filed in two folders is a file in each.
const FASTMAIL: &str = "5D3C9E70-0000-4000-8000-0000000000FA";

const RISA: &str = "From: Deanna Troi <troi@enterprise.example>\n\
To: William Riker <riker@enterprise.example>\n\
Subject: Shore leave on Risa\n\
Date: Thu, 08 Jan 2026 18:00:00 +0000\n\
Message-ID: <risa@enterprise.example>\n\
\n\
The jamaharon is booked.\n";

/// With no account named, every account in Mail is copied, each under its
/// own id: one message in both accounts is an email in each, a message in
/// two folders of one account is one email in both, and a message deleted
/// in one account leaves only that account.
#[tokio::test(flavor = "multi_thread")]
async fn every_account_is_copied_under_its_own_id() {
    let folder = MailFolder::new().await;
    let fastmail = |mailbox: &str| {
        folder
            .root
            .join("V10")
            .join(FASTMAIL)
            .join(format!("{mailbox}.mbox"))
            .join(STORE)
            .join("Data/Messages")
    };
    for mailbox in ["INBOX", "travel", "travel.mbox/portugal"] {
        std::fs::create_dir_all(fastmail(mailbox)).unwrap();
    }
    write_emlx(&fastmail("INBOX").join("20.emlx"), BRIEFING);
    write_emlx(&fastmail("travel").join("21.emlx"), RISA);
    write_emlx(&fastmail("travel.mbox/portugal").join("22.emlx"), RISA);
    folder
        .index(&[
            &format!(
                "INSERT INTO mailboxes (ROWID, url) VALUES
                (10, 'imap://{FASTMAIL}/INBOX'),
                (11, 'imap://{FASTMAIL}/travel'),
                (12, 'imap://{FASTMAIL}/travel/portugal')"
            ),
            "INSERT INTO message_global_data (ROWID, message_id_header) VALUES
                (20, '<briefing@enterprise.example>'),
                (21, '<risa@enterprise.example>')",
            "INSERT INTO messages (ROWID, global_message_id, mailbox, read, conversation_id,
                date_received) VALUES
                (20, 20, 10, 1, 30, 1767603600),
                (21, 21, 11, 0, 31, 1767895200),
                (22, 21, 12, 0, 31, 1767895200)",
        ])
        .await;
    let work = tempfile::tempdir().unwrap();
    let every = AppleMailAccountConfig::default();
    sync_with(
        &folder,
        work.path(),
        every.clone(),
        datalib_etl::progress::Progress::noop(),
    )
    .await;

    let gmail = "apple_mail_1f0b8a52";
    let fm = "apple_mail_5d3c9e70";
    let emails: Vec<(String, String)> =
        rows(work.path(), "SELECT id, account_id FROM emails ORDER BY id").await;
    let expected = |ids: &[(&str, &str)]| -> Vec<(String, String)> {
        ids.iter()
            .map(|(id, a)| (format!("{a}:{id}"), a.to_string()))
            .collect()
    };
    assert_eq!(
        emails,
        expected(&[
            ("briefing@enterprise.example", gmail),
            ("reply@enterprise.example", gmail),
            ("report@enterprise.example", gmail),
            ("briefing@enterprise.example", fm),
            ("risa@enterprise.example", fm),
        ])
    );
    let risa: Vec<(String,)> = rows(
        work.path(),
        "SELECT m.name FROM email_mailboxes j JOIN mailboxes m ON m.id = j.mailbox_id
         WHERE j.email_id = 'apple_mail_5d3c9e70:risa@enterprise.example' ORDER BY m.name",
    )
    .await;
    assert_eq!(
        risa,
        [("travel".to_string(),), ("travel/portugal".to_string(),)]
    );
    let accounts: Vec<(String,)> = rows(work.path(), "SELECT id FROM accounts ORDER BY id").await;
    assert_eq!(accounts, [(gmail.to_string(),), (fm.to_string(),)]);

    // Deleted in the Fastmail account only.
    folder
        .index(&["DELETE FROM messages WHERE ROWID = 20"])
        .await;
    let after = sync_with(
        &folder,
        work.path(),
        every,
        datalib_etl::progress::Progress::noop(),
    )
    .await;
    assert_eq!(after.emails_removed, 1, "{after:?}");
    let briefings: Vec<(String,)> = rows(
        work.path(),
        "SELECT id FROM emails WHERE message_id = 'briefing@enterprise.example'",
    )
    .await;
    assert_eq!(
        briefings,
        [(format!("{gmail}:briefing@enterprise.example"),)]
    );
}
