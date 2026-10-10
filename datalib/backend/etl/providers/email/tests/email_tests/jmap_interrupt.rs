//! A Fastmail (JMAP) download cut off at any request, then run again,
//! ends with the store an uninterrupted run leaves
//! (`datalib_etl_web::interrupt`). Twice: a first sync, whose
//! `Email/query` takes three pages and whose `Email/get` takes two
//! batches; and a later sync from the store the first left, against an
//! upstream where a mailbox was renamed, a message arrived, one moved
//! mailbox, one was flagged and one was destroyed, the changes arriving
//! over two `Email/changes` pages.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_email::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;

use crate::jmap_tape::{Account, Email, Tape, HOST};

/// What the download mirrors, what upstream listed, and the state
/// tokens. Not the `_bookkeeping` sidecars (attempt counts and stamps,
/// which a run that was cut off has more of), `problems` or `sync_runs`;
/// what is held for each listed message, which lives on the listing's
/// sidecar, is read by [`contents`] on its own.
pub const MIRRORED: &[&str] = &[
    "accounts",
    "mailboxes",
    "threads",
    "emails",
    "email_mailboxes",
    "email_keywords",
    "email_blobs",
    "listed_messages",
    "listed_whole",
    "sync_scope_state",
];

/// [`MIRRORED`], then the version held for each listed message.
pub async fn contents(db: &RawDb) -> Result<String> {
    let mut out = dump_tables(db.pool(), MIRRORED).await?;
    out.push_str("== held\n");
    let held: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, held_version FROM listed_messages_bookkeeping
         WHERE held_version IS NOT NULL ORDER BY id",
    )
    .fetch_all(db.pool())
    .await?;
    for (id, version) in held {
        out.push_str(&format!("{id}={version}\n"));
    }
    Ok(out)
}

/// Copies the store an earlier run left under `earlier` into `dir`.
pub fn copy_store(earlier: &Path, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(earlier)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
    }
    Ok(())
}

struct Jmap {
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Jmap {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        match &self.earlier {
            Some(earlier) => copy_store(earlier, dir),
            None => Ok(()),
        }
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        let mut opts = FetchOptions::new(db.clone());
        opts.hostname = HOST.to_string();
        // One at a time, so the order of the `.eml` requests is the same
        // in every run and a cut lands on the same one.
        opts.blob_download_concurrency = Some(1);
        opts.blob_flush_count = Some(1);
        opts.control = DownloadControl {
            stop,
            ..Default::default()
        };
        playback::scope(&self.playback, fetch(opts))
            .await
            .map(|_| ())
    }

    async fn seal(&self, db: RawDb) -> Result<()> {
        db.commit_all("test").await?;
        db.close().await;
        Ok(())
    }

    async fn contents(&self, dir: &Path) -> Result<String> {
        let db = self.open(dir).await?;
        let out = contents(&db).await;
        db.close().await;
        out
    }
}

fn id(n: usize) -> String {
    format!("M{n:02}")
}

/// Fifty-five emails in three threads, all in the Inbox and the first
/// five in the Archive too, and a Holodeck mailbox holding nothing.
fn before() -> Account {
    let mut a = Account::new(
        &[("MB1", "Inbox"), ("MB2", "Archive"), ("MB3", "Holodeck")],
        &[],
    );
    for n in 1..=55 {
        let mut mailboxes = vec!["MB1".to_string()];
        if n <= 5 {
            mailboxes.push("MB2".to_string());
        }
        a.emails.insert(
            id(n),
            Email {
                mailboxes,
                thread: format!("T{}", 1 + (n - 1) / 20),
                flagged: false,
            },
        );
    }
    a
}

/// [`before`], later: the Holodeck renamed, `M56` arrived in the third
/// thread, `M03` moved to the Holodeck, `M30` flagged, `M10` destroyed.
fn after() -> Account {
    let mut a = before();
    a.mailbox_state = "mbox-2".into();
    a.email_state = "email-2".into();
    a.mailboxes.insert("MB3".into(), "Holodeck Three".into());
    a.emails.insert(
        id(56),
        Email {
            mailboxes: vec!["MB1".into()],
            thread: "T3".into(),
            flagged: false,
        },
    );
    a.emails.get_mut("M03").unwrap().mailboxes = vec!["MB3".into()];
    a.emails.get_mut("M30").unwrap().flagged = true;
    a.emails.remove("M10");
    a
}

/// Every `Email/get` a run over `a` can ask for when it fetches what it
/// owes in id order, fifty at a time, from any batch boundary.
fn record_batches(tape: &Tape, a: &Account) {
    let ids = a.ids();
    for start in (0..ids.len()).step_by(50) {
        let batch: Vec<&str> = ids[start..].iter().take(50).map(String::as_str).collect();
        tape.gets(a, &batch);
    }
}

fn record_before(out: &Path) -> Account {
    let a = before();
    let mut tape = Tape::new(out);
    tape.query_page = 20;
    tape.serve(&a);
    record_batches(&tape, &a);
    a
}

fn record_after(out: &Path) {
    let a = after();
    let mut tape = Tape::new(out);
    tape.query_page = 20;
    tape.serve(&a);
    record_batches(&tape, &a);
    tape.mailbox_changes(&a, "mbox-1", &["MB3"], &[]);
    tape.email_changes("email-1", &["M56"], &["M03"], &[], "email-1a", true);
    tape.email_changes("email-1a", &[], &["M30"], &["M10"], "email-2", false);
    tape.gets(&a, &["M03", "M30", "M56"]);
}

/// The first twenty requests, the last ten, and every fourth between:
/// the session, the listings and the first bodies, the last bodies, and
/// a walk through the body phase.
fn some(n: u64) -> Vec<u64> {
    (1..=n)
        .filter(|k| *k <= 20 || *k > n.saturating_sub(10) || k % 4 == 0)
        .collect()
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let playback = d.path().join("playback");
    record_before(&playback);
    let rig = Jmap {
        playback,
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, some)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// From an empty store nothing is ever changed, moved or destroyed, so
/// a download that remembers which emails a delta named only while it
/// runs passes the test above and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Jmap {
        playback: d.path().join("playback-before"),
        earlier: None,
    };
    record_before(&first.playback);
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Jmap {
        playback: d.path().join("playback-after"),
        earlier: Some(earlier),
    };
    record_after(&rig.playback);
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
