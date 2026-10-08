//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (docs/dev/plans/sync_state.md §8).
//! The tape is a small TNG workspace that makes the download do every
//! kind of work it has: a search of two pages, a body with an
//! attachment, a truncated body with a follow-up, a body that answers
//! 404, comments on the page and on a block, a user Notion has and one
//! it has not. Run from an empty store and from the store an earlier
//! run left, against a workspace that has moved since; in search mode
//! and from a root.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::retry::{self, RetryGuard};
use datalib_etl_web::synthesize::write_fixture;
use serde_json::json;

use crate::support::*;

/// Every table the download fills. The `_bookkeeping` sidecars and
/// `problems` are left out: a run that was cut off has more attempts
/// than one that was not. What the sidecars hold is compared on its
/// own ([`HELD`]): the next run decides what to fetch from it.
const TABLES: &[&str] = &[
    "pages",
    "page_markdown",
    "page_comments",
    "comments",
    "comment_anchors",
    "users",
    "notion_attachments",
    "coverage",
];

const HELD: &[&str] = &[
    "pages",
    "page_markdown",
    "page_comments",
    "comment_anchors",
    "users",
    "notion_attachments",
];

async fn dump_held(pool: &sqlx::SqlitePool) -> Result<String> {
    let mut out = String::new();
    for table in HELD {
        out.push_str(&format!("== {table} held\n"));
        // Audited: `table` is a literal of HELD.
        let rows: Vec<(String, bool, Option<String>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT id, fetched_at_utc IS NOT NULL, held_version \
                 FROM {table}_bookkeeping ORDER BY id"
            )))
            .fetch_all(pool)
            .await?;
        for (id, fetched, held) in rows {
            out.push_str(&format!("{id} fetched={fetched} @ {held:?}\n"));
        }
    }
    Ok(out)
}

const T1: &str = "2026-09-01T00:00:00.000Z";
const T2: &str = "2026-09-02T00:00:00.000Z";
const T3: &str = "2026-09-03T00:00:00.000Z";
const T4: &str = "2026-09-04T00:00:00.000Z";
const T5: &str = "2026-09-05T00:00:00.000Z";
const PICARD: &str = "1701d000-0000-4000-8000-00000000aa01";
const RIKER: &str = "1701d000-0000-4000-8000-00000000aa02";
const DATA: &str = "1701d000-0000-4000-8000-00000000aa03";
const BLOCK: &str = "1701d000-0000-4000-8000-00000000bb01";
const HOLE: &str = "1701d000-0000-4000-8000-00000000cc01";
const SLOT: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/warp.png";

fn link(id: &str) -> String {
    format!(
        "<page url=\"https://app.notion.com/p/Page-{}\">a page</page>",
        id.replace('-', "")
    )
}

fn authored(id: &str, edited: &str, by: &str) -> serde_json::Value {
    let mut p = page(id, edited);
    p["created_by"] = json!({"object": "user", "id": by});
    p
}

/// The workspace before and after upstream moved: the bridge page was
/// edited and gained a comment, and a new page appeared.
#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    let bridge_edited = match edition {
        Edition::Before => T3,
        Edition::After => T4,
    };
    let signed = format!("{SLOT}?X-Amz-Signature=abc");
    // The bridge: an attachment, links to the other two pages, a comment
    // on the page and one on a block, two users.
    serve(
        &t,
        &format!("{BASE}/pages/{BRIDGE}"),
        authored(BRIDGE, bridge_edited, PICARD),
    );
    let mut bridge_body = format!(
        "Captain's log.\n![warp]({signed})\n{}\n{}\n",
        link(SICKBAY),
        link(HOLODECK)
    );
    if matches!(edition, Edition::After) {
        bridge_body.push_str("Make it so.\n");
    }
    serve_body(&t, BRIDGE, &bridge_body, false);
    let mut comments = vec![
        comment("c1", BRIDGE, None),
        comment("c2", BRIDGE, Some(BLOCK)),
    ];
    if matches!(edition, Edition::After) {
        let mut c3 = comment("c3", BRIDGE, None);
        c3["created_by"] = json!({"object": "user", "id": DATA});
        comments.push(c3);
    }
    serve_comment_list(&t, BRIDGE, json!(comments));
    write_fixture(
        &t,
        &HttpRequest::get(HttpService::Notion, &signed).plain(),
        &HttpResponse {
            status: 200,
            headers: [("content-type".to_string(), "image/png".to_string())].into(),
            body: b"\x89PNG warp core".to_vec(),
            duration_ms: 0,
        },
    )
    .unwrap();
    serve(
        &t,
        &format!("{BASE}/blocks/{BLOCK}"),
        json!({"object": "block", "id": BLOCK, "type": "paragraph",
               "paragraph": {"rich_text": [{"plain_text": "Warp core alignment"}]}}),
    );
    serve(
        &t,
        &format!("{BASE}/users/{PICARD}"),
        json!({"object": "user", "id": PICARD, "name": "Jean-Luc Picard"}),
    );
    serve_status(&t, &format!("{BASE}/users/{RIKER}"), 404);
    serve(
        &t,
        &format!("{BASE}/users/{DATA}"),
        json!({"object": "user", "id": DATA, "name": "Data"}),
    );

    // Sickbay: a truncated body whose subtree is fetched as a follow-up,
    // and comments Notion answers 404 for.
    serve(
        &t,
        &format!("{BASE}/pages/{SICKBAY}"),
        authored(SICKBAY, T2, RIKER),
    );
    let marker = format!(
        "<unknown url=\"https://www.notion.so/x#{}\"/>",
        HOLE.replace('-', "")
    );
    serve_body(&t, SICKBAY, &format!("Sickbay log\n{marker}\n"), true);
    serve_body(&t, HOLE, "Patient: Riker\n", false);
    serve_status(&t, &comments_url(SICKBAY), 404);

    // The holodeck: a body Notion answers 404 for, and no comments.
    serve_object(&t, HOLODECK, T1);
    serve_status(&t, &format!("{BASE}/pages/{HOLODECK}/markdown"), 404);
    serve_comments(&t, HOLODECK);

    match edition {
        Edition::Before => {
            serve_search(
                &t,
                None,
                json!([page(BRIDGE, T3), page(SICKBAY, T2)]),
                Some("p2"),
            );
        }
        Edition::After => {
            serve_page(&t, TEN_FORWARD, T5, "Poker night.\n");
            serve_search(
                &t,
                None,
                json!([page(TEN_FORWARD, T5), page(BRIDGE, T4), page(SICKBAY, T2)]),
                Some("p2"),
            );
        }
    }
    serve_search(&t, Some("p2"), json!([page(HOLODECK, T1)]), None);
    t
}

struct Notion {
    playback: PathBuf,
    roots: Vec<String>,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Notion {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        if let Some(earlier) = &self.earlier {
            for entry in std::fs::read_dir(earlier)? {
                let entry = entry?;
                std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        let fast = std::time::Duration::from_millis(1);
        let guard = RetryGuard::new(
            std::time::Duration::from_secs(3600),
            100,
            fast,
            fast,
            stop.clone(),
        );
        retry::scope(
            guard,
            fetch(FetchOptions {
                subtree_pages: self.roots.clone(),
                control: DownloadControl {
                    stop,
                    ..Default::default()
                },
                ..FetchOptions::new(db.clone())
            }),
        )
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
        let out = async {
            let tables = dump_tables(db.pool(), TABLES).await?;
            let held = dump_held(db.pool()).await?;
            Ok::<_, anyhow::Error>(tables + &held)
        }
        .await;
        db.close().await;
        out
    }
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

async fn from_an_empty_store(roots: Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    let rig = Notion {
        playback: tape(d.path(), Edition::Before),
        roots,
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// A store an earlier run filled, and an upstream that has moved since.
/// From an empty store nothing is ever *changed*, and a download that
/// remembers "changed" only while it runs passes the test above and
/// fails this one.
async fn from_an_earlier_store(roots: Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    let first = Notion {
        playback: tape(d.path(), Edition::Before),
        roots: roots.clone(),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Notion {
        playback: tape(d.path(), Edition::After),
        roots,
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_search_cut_off_at_any_request_resumes_to_the_same_store() {
    from_an_empty_store(Vec::new()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_search_cut_off_at_any_request_resumes_to_the_same_store() {
    from_an_earlier_store(Vec::new()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_roots_walk_cut_off_at_any_request_resumes_to_the_same_store() {
    from_an_empty_store(vec![BRIDGE.to_string()]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_roots_walk_cut_off_at_any_request_resumes_to_the_same_store() {
    from_an_earlier_store(vec![BRIDGE.to_string()]).await;
}
