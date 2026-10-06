//! The repo's fixture pipeline (`tests/fixtures/run_sync_pipeline.py`)
//! syncs the checked-in capture, then the second capture `slack_api_v2`,
//! then each of them once more, with the config below. A tape answers
//! only the exact requests it holds, so a download that asks for anything
//! else leaves `listing:` rows there. This replays that sequence here,
//! where a change to what the download asks for is seen at once.

use std::path::{Path, PathBuf};

use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};

use crate::support::{fetch_into, serve, stored_ts};

fn first_capture() -> PathBuf {
    match std::env::var("SLACK_FIXTURE_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api"),
    }
}

async fn sync(out: &Path) {
    fetch_into(out, |o| FetchOptions {
        members_only: true,
        media: false,
        dms: true,
        refresh_window_days: 0,
        ..o
    })
    .await
    .unwrap();
}

async fn column(out: &Path, sql: &'static str) -> Vec<String> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    rows
}

async fn problems(out: &Path) -> Vec<String> {
    column(out, "SELECT scope_key FROM problems ORDER BY scope_key").await
}

const WORFS_FOURTH_REPLY: &str = "12604001100.001100";
const RED_ALERT: &str = "12604001200.001200";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_captures_answer_every_request_the_pipelines_syncs_make() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("out_raw");
    let first = first_capture();
    let second = first.with_file_name("slack_api_v2");
    let (playback, playback_v2) = (d.path().join("playback"), d.path().join("playback_v2"));

    serve(&first, &playback);
    sync(&out).await;
    assert_eq!(problems(&out).await, [] as [&str; 0], "the first sync");
    let cold = stored_ts(&out);
    sync(&out).await;
    assert_eq!(problems(&out).await, [] as [&str; 0], "the first again");
    assert_eq!(stored_ts(&out), cold);

    serve(&second, &playback_v2);
    sync(&out).await;
    assert_eq!(problems(&out).await, [] as [&str; 0], "the second capture");
    let grown = stored_ts(&out);
    assert_eq!(grown.len(), cold.len() + 2);
    for added in [WORFS_FOURTH_REPLY, RED_ALERT] {
        assert!(grown.iter().any(|ts| ts == added), "{added} in {grown:?}");
    }
    sync(&out).await;
    assert_eq!(problems(&out).await, [] as [&str; 0], "the second again");

    serve(&first, &playback);
    sync(&out).await;
    assert_eq!(problems(&out).await, [] as [&str; 0], "back to the first");
    assert_eq!(stored_ts(&out), grown);

    // What this config leaves in the tables that say what is held: the
    // newest top-level message of each conversation, never a reply's
    // `ts`; each thread at its newest reply; the one file as an edge
    // with no bytes, since `media` is off.
    assert_eq!(
        column(
            &out,
            "SELECT scope || ' ' || hi FROM coverage ORDER BY scope"
        )
        .await,
        [
            "history:C_BRIDGE 012604001200.001200",
            "history:C_ENG 012604000500.000500",
            "history:C_TENFWD 012604000600.000600",
            "history:D_DATA 012604000902.000902",
            "history:D_RIKER 012604000901.000901",
            "history:G_AWAYTEAM 012604000904.000904",
        ]
    );
    assert_eq!(
        column(
            &out,
            "SELECT id || ' ' || latest_reply FROM replies_pages ORDER BY id"
        )
        .await,
        [
            "T_NCC1701D#C_BRIDGE#12604000100.000100 12604001100.001100",
            "T_NCC1701D#C_TENFWD#12604000600.000600 12604000700.000700",
        ]
    );
    assert_eq!(
        column(
            &out,
            "SELECT file_id || ' ' || COALESCE(blake3, 'no bytes') FROM slack_attachments"
        )
        .await,
        ["F_DELTA_SHIELD no bytes"]
    );
}
