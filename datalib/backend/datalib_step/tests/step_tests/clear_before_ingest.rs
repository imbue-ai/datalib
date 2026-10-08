//! A config still carrying the retired `always_clear_before_ingest`,
//! against the real binary: what a reader sees when the input the store
//! would be rewritten from is not there.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn fixture() -> PathBuf {
    PathBuf::from(std::env::var_os("SMS_FIXTURE_DIR").expect("SMS_FIXTURE_DIR"))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

fn ingest(root: &Path, backup: &Path) -> Output {
    let params = root.join("params.json");
    std::fs::write(
        &params,
        serde_json::json!({
            "backup": {"path": backup},
            "common": {"always_clear_before_ingest": true},
        })
        .to_string(),
    )
    .unwrap();
    Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("--params-file")
        .arg(&params)
        .env("DATALIB_DAG_STEP", "phone/ingest")
        .env("DATALIB_DAG_GROUP", "phone")
        .env("DATALIB_DAG_GROUP_TYPE", "sms_backup_restore")
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_CACHE_DIR", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step")
}

async fn messages_on_main(root: &Path) -> i64 {
    let store = datalib_etl::raw_layout::entities_db(&root.join("phone/ingest"));
    let reader = datalib_etl::doltlite_raw::open_reader(&store, None)
        .await
        .unwrap()
        .expect("a store with a main");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sms_messages")
        .fetch_one(reader.pool())
        .await
        .unwrap();
    reader.close().await;
    n
}

/// The key used to wipe the store and seal that to `main` before the
/// input was read, so a backup folder that had gone (an unmounted drive,
/// a moved folder) left every reader an empty mirror, and render deleted
/// every document. It now has no effect, and the config still loads.
#[tokio::test]
async fn an_input_that_is_not_there_wipes_nothing_a_reader_sees() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let backup = root.join("backup");
    copy_dir(&fixture(), &backup);

    let first = ingest(root, &backup);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let held = messages_on_main(root).await;
    assert!(held > 0, "the fixture stored no messages");

    std::fs::remove_dir_all(&backup).unwrap();
    let second = ingest(root, &backup);
    assert!(
        !second.status.success(),
        "a missing backup should fail the step"
    );
    assert_eq!(
        messages_on_main(root).await,
        held,
        "a reader still sees what the last good run stored"
    );
}
