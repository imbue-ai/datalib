//! A protected library's temp files stay in its folder, where macOS guards
//! them: the desktop app points `TMPDIR` at `system/tmp`, a step inherits it,
//! and the server clears what a crash left, since macOS no longer does.

mod server;

use std::path::Path;
use std::time::{Duration, Instant};

const WRITES_ITS_TEMP_DIR: &str = r#"
[[groups]]
id = "g"

[[steps]]
group = "g"
function = "out"
command = '''sh -c 'mkdir -p "$DATALIB_DAG_DATA_ROOT/g/out" && printf "%s" "$TMPDIR" > "$DATALIB_DAG_DATA_ROOT/g/out/tmpdir.partial" && mv "$DATALIB_DAG_DATA_ROOT/g/out/tmpdir.partial" "$DATALIB_DAG_DATA_ROOT/g/out/tmpdir"' '''
"#;

fn seeded_root(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = dir.join("lib");
    let temp = root.join("system/tmp");
    std::fs::create_dir_all(temp.join("left-behind-dir")).unwrap();
    std::fs::write(temp.join("left-behind-file"), "from a crash").unwrap();
    std::fs::write(root.join("config.toml"), WRITES_ITS_TEMP_DIR).unwrap();
    (root, temp)
}

/// macOS clears its own temp dir, and nothing else would clear this one, so
/// a decrypted database a crash left would stay for good.
#[test]
fn a_protected_librarys_server_clears_what_a_crash_left_and_hands_steps_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let (root, temp) = seeded_root(dir.path());

    let server = server::start_with_env(
        &root,
        &[
            ("DATALIB_PROTECTED_LIBRARY", "1"),
            ("TMPDIR", temp.to_str().unwrap()),
        ],
    );

    assert!(temp.is_dir(), "the folder itself must stay");
    assert_eq!(
        std::fs::read_dir(&temp).unwrap().count(),
        0,
        "what a crash left is still there"
    );

    let answer = server.post_json("/api/requests", r#"{"roots":["g/out"]}"#);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let written = root.join("g/out/tmpdir");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !written.exists() {
        assert!(Instant::now() < deadline, "the step never ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        temp.to_str().unwrap()
    );
}

/// Any other library has no such folder of ours, so a `system/tmp` in one is
/// not ours to empty.
#[test]
fn another_librarys_server_leaves_system_tmp_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (root, temp) = seeded_root(dir.path());

    let _server = server::start(&root);

    assert!(temp.join("left-behind-file").exists());
    assert!(temp.join("left-behind-dir").exists());
}
