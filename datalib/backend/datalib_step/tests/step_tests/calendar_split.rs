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
