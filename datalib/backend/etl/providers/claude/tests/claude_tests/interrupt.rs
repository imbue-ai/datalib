//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! The tape is a small TNG org: three conversations, one with two
//! files, one of which claude.ai no longer serves, and two projects
//! with knowledge docs. Run from an empty store and from the store an
//! earlier run left, against an org that has moved since: one
//! conversation edited and given a new file, one new, one deleted, and
//! one project's docs changed with its `updated_at`.
//!
//! The cut the rig cannot make, between a conversation's row and the
//! attachment edges that name its files, is made by taking the blob
//! store away ([`a_death_between_the_row_and_its_attachments`]).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_claude::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_claude::synthesize::ClaudeSynth;
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::{write_fixture, Synthesizer};
use serde_json::{json, Value};

const ORG: (&str, &str) = ("org-a", "Enterprise");
const NOW: &str = "2369-02-01T00:00:00Z";
const T1: &str = "2369-01-01T00:00:00Z";
const T2: &str = "2369-01-02T00:00:00Z";
const T3: &str = "2369-01-03T00:00:00Z";
const T4: &str = "2369-01-04T00:00:00Z";
const T5: &str = "2369-01-05T00:00:00Z";

/// Every table the download fills, with the blob store's beside them
/// and the sweep markers, which carry the run's pinned now. The
/// `_bookkeeping` sidecars and `problems` are left out: a run that was
/// cut off has more attempts than one that was not. What the sidecars
/// hold is compared on its own ([`HELD`]).
const TABLES: &[&str] = &[
    "users",
    "orgs",
    "projects",
    "project_docs",
    "project_docs_listings",
    "conversations",
    "claude_attachments",
    "sync_scope_state",
];
const HELD: &[&str] = &[
    "projects",
    "project_docs_listings",
    "conversations",
    "claude_attachments",
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

fn file(uuid: &str) -> Value {
    json!({
        "file_uuid": uuid,
        "file_name": format!("{uuid}.png"),
        "file_kind": "image",
        "preview_url": format!("/api/files/{uuid}/preview"),
    })
}

fn conv(id: &str, updated_at: &str, files: &[&str]) -> Value {
    let (org, org_name) = ORG;
    json!({
        "uuid": id,
        "name": id,
        "updated_at": updated_at,
        "organization_uuid": org,
        "account": {"uuid": "acct-1"},
        "chat_messages": [{
            "uuid": format!("{id}-m1"),
            "sender": "human",
            "text": "Readout attached.",
            "content": [{"type": "text", "text": "Readout attached."}],
            "files": files.iter().map(|f| file(f)).collect::<Vec<_>>(),
        }],
        "_source": {"via": "claude.ai/api", "org_uuid": org, "org_name": org_name},
    })
}

fn project(id: &str, updated_at: &str, docs: &[(&str, &str)]) -> Value {
    let (org, org_name) = ORG;
    json!({
        "uuid": id,
        "name": id,
        "creator": {"uuid": "acct-1"},
        "created_at": T1,
        "updated_at": updated_at,
        "_source": {"org_uuid": org, "org_name": org_name},
        "docs": docs.iter().map(|(uuid, content)| json!({
            "uuid": uuid, "file_name": format!("{uuid}.md"), "content": content, "created_at": T1,
        })).collect::<Vec<_>>(),
    })
}

fn write_json(path: &Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn contents(uuid: &str) -> HttpRequest {
    HttpRequest::get(
        HttpService::Claude,
        format!(
            "https://claude.ai/api/organizations/{}/files/{uuid}/contents",
            ORG.0
        ),
    )
}

fn serve_bytes(tape: &Path, uuid: &str) {
    write_fixture(
        tape,
        &contents(uuid),
        &HttpResponse {
            status: 200,
            headers: [("content-type".to_string(), "image/png".to_string())].into(),
            body: format!("the bytes of {uuid}").into_bytes(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

fn serve_gone(tape: &Path, uuid: &str) {
    write_fixture(
        tape,
        &contents(uuid),
        &HttpResponse {
            status: 404,
            headers: Default::default(),
            body: b"{\"error\":\"no\"}".to_vec(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

/// The org before and after upstream moved.
#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

fn tape(dir: &Path, edition: Edition) -> (PathBuf, PathBuf) {
    let name = match edition {
        Edition::Before => "before",
        Edition::After => "after",
    };
    let api = dir.join(format!("api-{name}"));
    let t = dir.join(format!("tape-{name}"));
    write_json(&api.join("users.json"), &json!([{"uuid": "acct-1"}]));
    let (convs, projects) = match edition {
        Edition::Before => (
            vec![
                conv("c-picard", T3, &["f-log", "f-lost"]),
                conv("c-riker", T2, &[]),
                conv("c-data", T1, &[]),
            ],
            vec![
                project(
                    "p-bridge",
                    T1,
                    &[("d-hail", "Subspace band 3."), ("d-esc", "Ops.")],
                ),
                project("p-holo", T1, &[("d-safe", "Leave them on.")]),
            ],
        ),
        Edition::After => (
            vec![
                conv("c-troi", T5, &[]),
                conv("c-picard", T4, &["f-log", "f-lost", "f-warp"]),
                conv("c-riker", T2, &[]),
            ],
            vec![
                project(
                    "p-bridge",
                    T2,
                    &[
                        ("d-hail", "Subspace band 4."),
                        ("d-esc", "Ops."),
                        ("d-new", "Engage."),
                    ],
                ),
                project("p-holo", T1, &[("d-safe", "Leave them on.")]),
            ],
        ),
    };
    write_json(&api.join("conversations.json"), &Value::Array(convs));
    for p in &projects {
        write_json(
            &api.join(format!("projects/{}.json", p["uuid"].as_str().unwrap())),
            p,
        );
    }
    ClaudeSynth::new(&api).synthesize(&t).unwrap();
    serve_bytes(&t, "f-log");
    serve_gone(&t, "f-lost");
    if matches!(edition, Edition::After) {
        serve_bytes(&t, "f-warp");
    }
    (api, t)
}

struct Claude {
    api: PathBuf,
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Claude {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        if let Some(earlier) = &self.earlier {
            for entry in fs::read_dir(earlier)? {
                let entry = entry?;
                fs::copy(entry.path(), dir.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        let download = fetch(FetchOptions {
            export_dir: Some(self.api.clone()),
            now: Some(NOW.to_string()),
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..FetchOptions::new(db.clone())
        });
        playback::scope(&self.playback, download).await.map(|_| ())
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
            let blobs = dump_tables(db.cas().pool(), &["cas_objects"]).await?;
            let held = dump_held(db.pool()).await?;
            Ok::<_, anyhow::Error>(tables + &blobs + &held)
        }
        .await;
        db.close().await;
        out
    }
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

fn rig(dir: &Path, edition: Edition, earlier: Option<PathBuf>) -> Claude {
    let (api, playback) = tape(dir, edition);
    Claude {
        api,
        playback,
        earlier,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = rig(d.path(), Edition::Before, None);
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
/// fails this one. The project whose docs changed is what caught the
/// metadata-before-docs ordering: its row was stored current before
/// its docs were listed, and a cut at the docs listing left the old
/// docs under a fresh sweep marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = rig(d.path(), Edition::Before, None);
    let earlier = d.path().join("earlier");
    fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = rig(d.path(), Edition::After, Some(earlier));
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// Bug C2 of the plan: the conversation's row and its freshness were
/// committed in one transaction and its attachment edges in a later one.
/// A run that died between them left a conversation that read as up to
/// date with no edge to say it had files, and no later run fetched
/// them. The rig cuts only at requests, so the gap is opened here by
/// taking the blob store away: the edge flush fails, whatever the run
/// had committed before it is sealed, and the next run has to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_between_the_row_and_its_attachments_leaves_them_owed() {
    let d = tempfile::tempdir().unwrap();
    let rig = rig(d.path(), Edition::Before, None);
    let whole = d.path().join("whole");
    fs::create_dir_all(&whole).unwrap();
    let db = rig.open(&whole).await.unwrap();
    rig.download(&db, StopFlag::new()).await.unwrap();
    rig.seal(db).await.unwrap();
    let want = rig.contents(&whole).await.unwrap();

    let cut = d.path().join("cut");
    fs::create_dir_all(&cut).unwrap();
    let db = rig.open(&cut).await.unwrap();
    db.cas().pool().close().await;
    let err = rig
        .download(&db, StopFlag::new())
        .await
        .expect_err("the blob store is gone, so the run cannot finish");
    assert!(format!("{err:#}").contains("closed"), "{err:#}");
    rig.seal(db).await.unwrap();

    let db = rig.open(&cut).await.unwrap();
    rig.download(&db, StopFlag::new()).await.unwrap();
    rig.seal(db).await.unwrap();
    let got = rig.contents(&cut).await.unwrap();
    assert_eq!(
        got, want,
        "the run after the death did not finish the store"
    );
}
