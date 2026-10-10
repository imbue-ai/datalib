//! The interruption test (`datalib_etl_web::interrupt`): a download cut off
//! at any request and run again ends with the store an uninterrupted run
//! leaves. The tape is a small workspace that makes the download do every
//! kind of work it has: listings of more than one page, a channel whose
//! history takes three pages, a thread whose replies take two, a file on
//! a message and one on a reply, a DM, an empty channel, and the account
//! state.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{record_auth, record_call, DEFAULT_SINCE_TS};
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::write_fixture;
use serde_json::{json, Value};

use crate::support::Tree;

const DM_TYPES: &str = "public_channel,private_channel,im,mpim";
const HISTORY_PAGE: usize = 3;

/// 2025-01-01T00:00:00Z plus `n` minutes, as a message `ts`.
pub fn ts(n: u64) -> String {
    format!("{}.{:06}", 1_735_689_600 + n * 60, n)
}

fn said(n: u64, user: &str, text: &str) -> Value {
    json!({"ts": ts(n), "user": user, "text": text})
}

fn file(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimetype": "text/plain",
        "size": name.len(),
        "url_private_download": file_url(id, name),
    })
}

fn file_url(id: &str, name: &str) -> String {
    format!("https://files.slack.com/files-pri/T1-{id}/download/{name}")
}

const THREAD: u64 = 40;
const REPLIES: [u64; 3] = [41, 45, 62];

fn thread_root() -> Value {
    json!({"ts": ts(THREAD), "user": "U1", "text": "Senior staff, report.",
           "thread_ts": ts(THREAD), "reply_count": REPLIES.len(),
           "latest_reply": ts(REPLIES[2])})
}

fn reply(n: u64, user: &str, text: &str) -> Value {
    json!({"ts": ts(n), "user": user, "text": text, "thread_ts": ts(THREAD)})
}

/// `#bridge`, newest first as `conversations.history` returns it.
fn bridge() -> Vec<Value> {
    let mut sensor_log = said(30, "U3", "Sensor log attached.");
    sensor_log["files"] = json!([file("F_SENSOR_LOG", "sensor_log.txt")]);
    vec![
        said(70, "U1", "Make it so."),
        said(60, "U2", "Course laid in for Starbase 74."),
        said(50, "U3", "Warp core is stable."),
        thread_root(),
        sensor_log,
        said(20, "U2", "Shields at maximum."),
        said(10, "U1", "Captain on the bridge."),
    ]
}

fn replies() -> Vec<Value> {
    let mut diagnostic = reply(REPLIES[2], "U3", "Level-three diagnostic attached.");
    diagnostic["files"] = json!([file("F_DIAGNOSTIC", "diagnostic.txt")]);
    vec![
        reply(REPLIES[0], "U2", "All decks report ready."),
        reply(REPLIES[1], "U3", "Engineering standing by."),
        diagnostic,
    ]
}

/// Every `conversations.history` request a walk of `channel` could make
/// between any two of `bounds`, answered as Slack would: the top-level
/// messages in range, newest first, [`HISTORY_PAGE`] to a page.
fn record_history(api: &Path, channel: &str, newest_first: &[Value], bounds: &[String]) {
    let ts_of = |m: &Value| m["ts"].as_str().unwrap().to_string();
    let latests = std::iter::once(None).chain(bounds.iter().map(Some));
    for latest in latests {
        for oldest in bounds {
            for inclusive in [true, false] {
                let hits: Vec<&Value> = newest_first
                    .iter()
                    .filter(|m| {
                        let t = ts_of(m);
                        let above = t > *oldest || (inclusive && t == *oldest);
                        let below = latest.is_none_or(|l| t < *l || (inclusive && t == *l));
                        above && below
                    })
                    .collect();
                let pages: Vec<&[&Value]> = if hits.is_empty() {
                    vec![&[]]
                } else {
                    hits.chunks(HISTORY_PAGE).collect()
                };
                for (i, page) in pages.iter().enumerate() {
                    let mut params = json!({
                        "channel": channel,
                        "include_all_metadata": "true",
                        "inclusive": if inclusive { "true" } else { "false" },
                        "limit": "200",
                        "oldest": oldest,
                    });
                    if let Some(latest) = latest {
                        params["latest"] = json!(latest);
                    }
                    if i > 0 {
                        params["cursor"] = json!(format!("page{i}"));
                    }
                    let more = i + 1 < pages.len();
                    let next = if more {
                        format!("page{}", i + 1)
                    } else {
                        String::new()
                    };
                    record_call(
                        api,
                        "conversations.history",
                        params,
                        json!({"ok": true, "messages": page, "has_more": more,
                               "response_metadata": {"next_cursor": next}}),
                    )
                    .unwrap();
                }
            }
        }
    }
}

/// The run's one now, and where its 30-day refresh window starts: half
/// way down `#bridge`, so a second run re-reads the newer part of every
/// channel and nothing re-reads the older part.
const NOW: &str = "2025-01-31T00:35:00Z";
const REFRESH_DAYS: i64 = 30;
const REFRESH_FROM_TS: &str = "1735691700.000000";

/// Every `ts` a walk of these messages could name as a bound, with the
/// start of the default `since` and of the refresh window.
fn bounds(messages: &[&[Value]]) -> Vec<String> {
    let mut out = vec![DEFAULT_SINCE_TS.to_string(), REFRESH_FROM_TS.to_string()];
    for m in messages.iter().flat_map(|m| m.iter()) {
        out.push(m["ts"].as_str().unwrap().to_string());
    }
    out
}

fn record_workspace(api: &Path) {
    record_auth(api).unwrap();
    record_call(
        api,
        "users.list",
        json!({"limit": "200"}),
        json!({"ok": true, "members": [
            {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
            {"id": "U2", "name": "riker", "real_name": "William Riker"},
        ], "response_metadata": {"next_cursor": "more"}}),
    )
    .unwrap();
    record_call(
        api,
        "users.list",
        json!({"limit": "200", "cursor": "more"}),
        json!({"ok": true, "members": [{"id": "U3", "name": "data", "real_name": "Data"}]}),
    )
    .unwrap();

    let list = json!({"exclude_archived": "true", "limit": "200", "types": DM_TYPES});
    record_call(
        api,
        "conversations.list",
        list.clone(),
        json!({"ok": true, "has_more": true, "response_metadata": {"next_cursor": "more"},
               "channels": [
            {"id": "C_BRIDGE", "name": "bridge", "is_member": true, "is_archived": false,
             "properties": {"tabs": [{"type": "bookmarks"}]}},
            {"id": "C_ENG", "name": "engineering", "is_member": true, "is_archived": false},
        ]}),
    )
    .unwrap();
    let mut second = list;
    second["cursor"] = json!("more");
    record_call(
        api,
        "conversations.list",
        second,
        json!({"ok": true, "has_more": false, "channels": [
            {"id": "C_HOLODECK", "name": "holodeck", "is_member": true, "is_archived": false},
            {"id": "D_RIKER", "is_im": true, "user": "U2", "is_archived": false},
        ]}),
    )
    .unwrap();

    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "mpims": [],
               "channels": [{"id": "C_BRIDGE", "last_read": ts(60), "latest": ts(70),
                             "mention_count": 0, "has_unreads": true}],
               "ims": [{"id": "D_RIKER", "last_read": ts(81), "latest": ts(81),
                        "mention_count": 0, "has_unreads": false}]}),
    )
    .unwrap();
    for filter in ["saved", "completed", "archived"] {
        let items = if filter == "saved" {
            json!([{"item_type": "message", "item_id": "C_BRIDGE", "ts": ts(50),
                    "state": "in_progress"}])
        } else {
            json!([])
        };
        record_call(
            api,
            "saved.list",
            json!({"filter": filter, "limit": "50"}),
            json!({"ok": true, "saved_items": items}),
        )
        .unwrap();
    }
    record_call(
        api,
        "bookmarks.list",
        json!({"channel_id": "C_BRIDGE"}),
        json!({"ok": true, "bookmarks": [
            {"id": "Bk1", "channel_id": "C_BRIDGE", "title": "Duty roster", "type": "link",
             "link": "https://example.test/duty-roster"},
        ]}),
    )
    .unwrap();

    let bridge = bridge();
    let replies = replies();
    record_history(api, "C_BRIDGE", &bridge, &bounds(&[&bridge, &replies]));
    let engineering = vec![
        said(91, "U3", "Dilithium matrix realigned."),
        said(90, "U3", "Beginning realignment."),
    ];
    record_history(api, "C_ENG", &engineering, &bounds(&[&engineering]));
    record_history(api, "C_HOLODECK", &[], &bounds(&[]));
    let riker = vec![said(81, "U1", "Agreed."), said(80, "U2", "Poker tonight?")];
    record_history(api, "D_RIKER", &riker, &bounds(&[&riker]));

    let thread = json!({"channel": "C_BRIDGE", "ts": ts(THREAD), "limit": "200"});
    record_call(
        api,
        "conversations.replies",
        thread.clone(),
        json!({"ok": true, "has_more": true, "response_metadata": {"next_cursor": "more"},
               "messages": [thread_root(), replies[0], replies[1]]}),
    )
    .unwrap();
    let mut second = thread;
    second["cursor"] = json!("more");
    record_call(
        api,
        "conversations.replies",
        second,
        json!({"ok": true, "has_more": false, "messages": [replies[2]]}),
    )
    .unwrap();
}

fn serve_file(playback: &Path, id: &str, name: &str) {
    write_fixture(
        playback,
        &HttpRequest::get(HttpService::Slack, file_url(id, name)),
        &HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: format!("contents of {name}").into_bytes(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

/// The tape, synthesized. Kept alive by the returned tree.
pub fn workspace() -> Tree {
    let t = Tree::new();
    record_workspace(&t.api);
    t.serve();
    serve_file(&t.playback, "F_SENSOR_LOG", "sensor_log.txt");
    serve_file(&t.playback, "F_DIAGNOSTIC", "diagnostic.txt");
    t
}

/// What the download mirrors, and what it has covered. Not the
/// `_bookkeeping` sidecars (attempt counts and stamps, which a run that
/// was cut off has more of), `problems`, `sync_runs`, or the sweep
/// markers in `sync_scope_state`, which only schedule a listing. The
/// one sidecar column that is a mirror of upstream, the version each
/// thread is held at, is dumped on its own by [`Rig::contents`].
const MIRRORED: &[&str] = &[
    "coverage",
    "workspaces",
    "users",
    "channels",
    "messages",
    "threads",
    "slack_attachments",
    "channel_read_states",
    "bookmarks",
    "saved_items",
];

struct Slack;

#[async_trait]
impl Rig for Slack {
    type Store = RawDb;

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        fetch(FetchOptions {
            refresh_window_days: REFRESH_DAYS,
            now: NOW.parse().unwrap(),
            members_only: false,
            media: true,
            dms: true,
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..FetchOptions::new(db.clone())
        })
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
            let mut out = dump_tables(db.pool(), MIRRORED).await?;
            let held: Vec<String> = sqlx::query_scalar(
                "SELECT id || ' ' || held_version FROM threads_bookkeeping \
                 WHERE held_version IS NOT NULL ORDER BY id",
            )
            .fetch_all(db.pool())
            .await?;
            out.push_str("== held threads\n");
            for h in held {
                out.push_str(&h);
                out.push('\n');
            }
            Ok::<_, anyhow::Error>(out)
        }
        .await;
        db.close().await;
        out
    }
}

async fn every_cut(how: How) {
    let tape = workspace();
    let scratch = tempfile::tempdir().unwrap();
    let cuts = every_cut_resumes(&Slack, how, scratch.path(), |n| (1..=n).collect());
    playback::scope(&tape.playback, cuts).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_killed_at_any_request_is_finished_by_the_next() {
    every_cut(How::Kill).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_stopped_at_any_request_is_finished_by_the_next() {
    every_cut(How::Stop).await;
}
