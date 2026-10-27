//! A calendar series split with "this and following events", against
//! the real binary: the ingest and render steps over an `.ics` folder,
//! before and after the edit.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture(which: &str) -> PathBuf {
    PathBuf::from(
        std::env::var_os("CALENDAR_SPLIT_FIXTURE_DIR").expect("CALENDAR_SPLIT_FIXTURE_DIR"),
    )
    .join(which)
    .join("Bridge.ics")
}

fn step(root: &Path, function: &str) {
    let params = root.join("params.json");
    std::fs::write(
        &params,
        serde_json::json!({"ics": {"path": root.join("calendars")}}).to_string(),
    )
    .unwrap();
    let mut cmd = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"));
    if function == "ingest" {
        cmd.arg("--params-file").arg(&params);
    } else {
        cmd.env("DATALIB_DAG_INPUTS", "bridge/ingest");
    }
    let out = cmd
        .env("DATALIB_DAG_STEP", format!("bridge/{function}"))
        .env("DATALIB_DAG_GROUP", "bridge")
        .env("DATALIB_DAG_GROUP_TYPE", "calendar")
        .env("DATALIB_DAG_FUNCTION", function)
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_CACHE_DIR", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    assert!(
        out.status.success(),
        "{function}: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sync(root: &Path, which: &str) {
    let calendars = root.join("calendars");
    std::fs::create_dir_all(&calendars).unwrap();
    std::fs::copy(fixture(which), calendars.join("Bridge.ics")).unwrap();
    step(root, "ingest");
    step(root, "render_markdown");
}

/// The event documents' titles, sorted, and the `no_document` problems.
async fn rendered(root: &Path) -> (Vec<String>, i64) {
    let store = root.join("bridge/render_markdown/indexed_markdown.doltlite_db");
    let reader = datalib_etl::doltlite_raw::open_reader(&store, None)
        .await
        .unwrap()
        .expect("a render store with a main");
    let titles: Vec<String> =
        sqlx::query_scalar("SELECT title FROM markdowns WHERE kind <> 'storage' ORDER BY title")
            .fetch_all(reader.pool())
            .await
            .unwrap();
    let kept: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM problems WHERE reason = 'no_document'")
            .fetch_one(reader.pool())
            .await
            .unwrap();
    reader.close().await;
    (titles, kept)
}

/// Google cuts the old series short and moves its later occurrences to
/// a new `_R` UID. The old series' row stays, so the driver had no
/// evidence the moved occurrences' documents were gone: it kept them
/// beside the new ones, each drawn twice, with a `no_document` warning.
#[tokio::test]
async fn a_this_and_following_edit_leaves_each_occurrence_drawn_once() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();

    sync(root, "before");
    let (before, _) = rendered(root).await;
    assert_eq!(
        before,
        [
            "Duty roster review",
            "Duty roster review — Klingon exchange officer",
            "Duty roster review — after the evacuation drill",
            "Duty roster review — before the Pakled visit",
        ]
    );

    sync(root, "after");
    let (after, kept) = rendered(root).await;
    assert_eq!(
        after,
        [
            "Duty roster review",
            "Duty roster review",
            "Duty roster review — Klingon exchange officer",
            "Duty roster review — after the evacuation drill",
            "Duty roster review — before the Pakled visit",
        ]
    );
    assert_eq!(kept, 0, "no document is kept on the driver's doubt");
}

/// The same split, listed as occurrences by the real binary with the
/// run's now pinned: the old series ends where the new one begins, each
/// date is listed once, a moved occurrence where it went, and the weekly
/// 10:00 keeps its hour when the clocks go back on 1 Nov.
#[tokio::test]
async fn a_split_series_lists_each_occurrence_once() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    sync(root, "after");

    let params = root.join("occurrences.json");
    std::fs::write(&params, r#"{"days_back": 14, "days_ahead": 30}"#).unwrap();
    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("--params-file")
        .arg(&params)
        .env("DATALIB_DAG_INPUTS", "bridge/ingest")
        .env("DATALIB_DAG_STEP", "bridge/render_occurrences")
        .env("DATALIB_DAG_GROUP", "bridge")
        .env("DATALIB_DAG_GROUP_TYPE", "calendar")
        .env("DATALIB_DAG_FUNCTION", "render_occurrences")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_NOW", "2026-10-15T12:00:00-07:00")
        .env("DATALIB_CACHE_DIR", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let store = root.join("bridge/render_occurrences/occurrences.doltlite_db");
    let reader = datalib_etl::doltlite_raw::open_reader(&store, None)
        .await
        .unwrap()
        .expect("an occurrences store with a main");
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT start_time, title FROM occurrences ORDER BY start_utc")
            .fetch_all(reader.pool())
            .await
            .unwrap();
    let window: (String, String) =
        sqlx::query_as("SELECT from_utc, until_utc FROM occurrences_window")
            .fetch_one(reader.pool())
            .await
            .unwrap();
    reader.close().await;
    assert_eq!(
        window,
        ("2026-10-01T00:00:00Z".into(), "2026-11-15T00:00:00Z".into())
    );
    let rows: Vec<(&str, &str)> = rows.iter().map(|(s, t)| (s.as_str(), t.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("2026-10-07T10:00:00-07:00", "Duty roster review"),
            ("2026-10-14T10:00:00-07:00", "Duty roster review"),
            (
                "2026-10-21T15:00:00-07:00",
                "Duty roster review — Klingon exchange officer"
            ),
            ("2026-10-28T10:00:00-07:00", "Duty roster review"),
            (
                "2026-11-04T16:00:00-08:00",
                "Duty roster review — after the evacuation drill"
            ),
            ("2026-11-11T10:00:00-08:00", "Duty roster review"),
        ]
    );
}
