//! The Rust step in `datalib/backend/step_protocol/README.md`, run under
//! the real runner: two shell producers feed it, and three syncs show it
//! reading what moved, then only what moved, then warning about a failed
//! input. The Python step in `docs/dev/step_protocol.md` is checked the
//! same way by `//tools:step_protocol_example_test`.

use std::path::{Path, PathBuf};
use std::process::Command;

use datalib_sql::test_sql;

fn bin(var: &str) -> PathBuf {
    std::path::absolute(std::env::var(var).unwrap_or_else(|_| panic!("{var} is set")))
        .unwrap_or_else(|e| panic!("{var}: {e}"))
}

/// Reports the version `control/version_<group>` holds, or fails when
/// `control/fail_<group>` exists.
const PRODUCER: &str = r#"name="${DATALIB_DAG_STEP%%/*}"
if [ -e "control/fail_$name" ]; then exit 1; fi
mkdir -p "$DATALIB_DAG_STEP"
printf '{"event":"outcome","outputs":[{"path":"%s","version":"%s"}]}\n' "$DATALIB_DAG_STEP" "$(cat "control/version_$name")"
"#;

fn config(example: &Path) -> String {
    format!(
        r#"[[steps]]
id = "a/make"
command = "sh make.sh"

[[steps]]
id = "b/make"
command = "sh make.sh"

[[steps]]
id = "report/build"
command = "{example}"
inputs = ["a/make", "b/make"]
reads = "pinned"
params = {{ greeting = "hello" }}
"#,
        example = example.display()
    )
}

struct Library {
    _dir: tempfile::TempDir,
    root: PathBuf,
    seen: i64,
}

impl Library {
    fn new() -> Library {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("control")).unwrap();
        std::fs::write(root.join("make.sh"), PRODUCER).unwrap();
        std::fs::write(root.join("config.toml"), config(&bin("MINIMAL_STEP"))).unwrap();
        Library {
            _dir: dir,
            root,
            seen: 0,
        }
    }

    fn set(&self, file: &str, text: &str) {
        std::fs::write(self.root.join("control").join(file), text).unwrap();
    }

    /// Runs both producers and what reads them; the example's new log
    /// lines, as `(level, msg)`.
    async fn sync(&mut self) -> Vec<(String, String)> {
        let out = Command::new(bin("DATALIB_PIPELINE_BIN"))
            .arg("sync")
            .arg(&self.root)
            .arg("a/make,b/make")
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("run datalib-pipeline");
        assert!(
            matches!(out.status.code(), Some(0 | 2)),
            "datalib-pipeline: {out:?}"
        );
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect(&format!(
                "sqlite://{}?mode=ro",
                datalib_runtime::layout::runs_db(&self.root).display()
            ))
            .await
            .unwrap();
        let rows: Vec<(i64, String, String)> = test_sql!(
            "SELECT seq, level, msg FROM log WHERE step = 'report/build' AND seq > ? ORDER BY seq"
        )
        .query_as()
        .bind(self.seen)
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;
        if let Some((seq, _, _)) = rows.last() {
            self.seen = *seq;
        }
        rows.into_iter()
            .map(|(_, level, msg)| (level, msg))
            .collect()
    }
}

fn has(lines: &[(String, String)], level: &str, msg: &str) -> bool {
    lines.iter().any(|(l, m)| l == level && m == msg)
}

#[tokio::test]
async fn the_rust_example_step_reads_what_moved_and_what_failed() {
    let mut lib = Library::new();
    lib.set("version_a", "a1");
    lib.set("version_b", "b1");
    let first = lib.sync().await;
    assert!(has(&first, "info", "a/make moved: a1"), "{first:?}");
    assert!(has(&first, "info", "b/make moved: b1"), "{first:?}");
    let out = lib.root.join("report/build");
    assert_eq!(
        std::fs::read_to_string(out.join("greeting")).unwrap(),
        "hello",
        "the params reached the step"
    );
    assert_eq!(std::fs::read_to_string(out.join("a_make")).unwrap(), "a1");

    lib.set("version_a", "a2");
    let second = lib.sync().await;
    assert!(has(&second, "info", "a/make moved: a2"), "{second:?}");
    assert!(
        !second.iter().any(|(_, m)| m.starts_with("b/make moved")),
        "b did not move: {second:?}"
    );

    lib.set("fail_a", "");
    lib.set("version_b", "b2");
    let third = lib.sync().await;
    assert!(has(&third, "info", "b/make moved: b2"), "{third:?}");
    assert!(
        has(
            &third,
            "warn",
            "a/make did not finish its last run; reading what it committed"
        ),
        "{third:?}"
    );
}
