//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! The tape is a small TNG account: two PRs, one with a comment, a
//! review and a review comment. Run from an empty store and from the
//! store an earlier run left, against an account that has moved since:
//! a PR edited and commented on again, and a new one.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_forge_ingest_common::Bounds;
use datalib_etl_github::ingest::{
    db_path_for, fetch, search_url, FetchOptions, RawDb, BASE, DEFAULT_SCOPES, PER_PAGE,
};
use datalib_etl_web::http::{HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::retry::{self, RetryGuard};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

use crate::support::*;

/// Every table the download fills. The `_bookkeeping` sidecars and
/// `problems` are left out: a run that was cut off has more attempts
/// than one that was not. What the PR sidecar holds is compared on its
/// own: the next run decides what to fetch from it.
const TABLES: &[&str] = &[
    "self_identity",
    "pull_requests",
    "issue_comments",
    "pr_reviews",
    "pr_review_comments",
    "listed_change_requests",
    "coverage",
];

async fn dump_held(pool: &sqlx::SqlitePool) -> Result<String> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM pull_requests_bookkeeping ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = String::from("== pull_requests held\n");
    for (id, fetched, held) in rows {
        out.push_str(&format!("{id} fetched={fetched} @ {held:?}\n"));
    }
    Ok(out)
}

const T1: &str = "2369-04-10T00:00:00Z";
const T2: &str = "2369-04-12T00:00:00Z";
const T4: &str = "2369-04-15T10:00:00Z";
const T5: &str = "2369-04-15T11:00:00Z";

fn serve(pb: &Path, url: &str, body: Value) {
    write_fixture(
        pb,
        &HttpRequest::get(HttpService::Github, url),
        &json_response(&body),
    )
    .unwrap();
}

fn pr(num: u64, updated_at: &str, title: &str) -> Value {
    json!({
        "number": num,
        "title": title,
        "state": "open",
        "updated_at": updated_at,
        "html_url": format!("https://github.com/{REPO}/pull/{num}"),
        "head": {"sha": "abc", "ref": "br"},
        "base": {"sha": "def", "ref": "main"},
    })
}

fn item(num: u64, updated_at: &str) -> Value {
    json!({
        "repository_url": format!("{BASE}/repos/{REPO}"),
        "number": num,
        "updated_at": updated_at,
    })
}

fn comment(id: i64, body: &str) -> Value {
    json!({"id": id, "body": body, "user": {"login": "wtriker"}})
}

fn search_page(items: &[Value]) -> Value {
    json!({"total_count": items.len(), "incomplete_results": false, "items": items})
}

/// One PR of the tape: its record and its three child lists.
struct Pr {
    num: u64,
    at: &'static str,
    detail: Value,
    comments: Vec<Value>,
    reviews: Vec<Value>,
    review_comments: Vec<Value>,
}

/// The account before and after upstream moved: PR 1 was edited and
/// gained a comment, and PR 3 appeared.
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
    serve(
        &t,
        &format!("{BASE}/user"),
        json!({"id": 17010001, "login": "jlpicard"}),
    );
    let (pr1_at, pr1_title) = match edition {
        Edition::Before => (T2, "Safety protocols"),
        Edition::After => (T4, "Safety protocols, revised"),
    };
    let mut prs = vec![
        Pr {
            num: 1,
            at: pr1_at,
            detail: pr(1, pr1_at, pr1_title),
            comments: vec![comment(101, "Looks good.")],
            reviews: Vec::new(),
            review_comments: Vec::new(),
        },
        Pr {
            num: 2,
            at: T1,
            detail: pr(2, T1, "Warp core alignment"),
            comments: Vec::new(),
            reviews: vec![json!({"id": 201, "state": "APPROVED", "user": {"login": "gdlaforge"}})],
            review_comments: vec![
                json!({"id": 301, "body": "nit", "user": {"login": "gdlaforge"}, "path": "x.rs"}),
            ],
        },
    ];
    if matches!(edition, Edition::After) {
        prs[0].comments.push(comment(102, "Make it so."));
        prs.push(Pr {
            num: 3,
            at: T5,
            detail: pr(3, T5, "Poker night"),
            comments: Vec::new(),
            reviews: Vec::new(),
            review_comments: Vec::new(),
        });
    }
    for p in &prs {
        let num = p.num;
        serve(
            &t,
            &format!("{BASE}/repos/{REPO}/pulls/{num}"),
            p.detail.clone(),
        );
        serve(
            &t,
            &format!("{BASE}/repos/{REPO}/issues/{num}/comments?per_page={PER_PAGE}"),
            json!(p.comments),
        );
        serve(
            &t,
            &format!("{BASE}/repos/{REPO}/pulls/{num}/reviews?per_page={PER_PAGE}"),
            json!(p.reviews),
        );
        serve(
            &t,
            &format!("{BASE}/repos/{REPO}/pulls/{num}/comments?per_page={PER_PAGE}"),
            json!(p.review_comments),
        );
    }
    let all: Vec<Value> = prs.iter().map(|p| item(p.num, p.at)).collect();
    // What moved since the earlier run's search: nothing before, PR 1
    // and PR 3 after.
    let moved: Vec<Value> = prs
        .iter()
        .filter(|p| p.at > SINCE)
        .map(|p| item(p.num, p.at))
        .collect();
    for scope in DEFAULT_SCOPES {
        serve(
            &t,
            &search_url(scope, &Bounds::default()),
            search_page(&all),
        );
        serve(&t, &search_url(scope, &resumed()), search_page(&moved));
    }
    t
}

struct Github {
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Github {
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
                refresh_window_days: 0,
                control: DownloadControl {
                    stop,
                    ..Default::default()
                },
                ..FetchOptions::new(db.clone(), crate::tng_now())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Github {
        playback: tape(d.path(), Edition::Before),
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Github {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Github {
        playback: tape(d.path(), Edition::After),
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
