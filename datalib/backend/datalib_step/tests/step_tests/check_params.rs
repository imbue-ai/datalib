//! `datalib-dag --check` asks `datalib-step check-params` about the
//! built-in steps' params, which the runner cannot read itself, and
//! reports a misspelled one against its own line before any sync runs.

use std::path::Path;
use std::process::Command;

fn bin(var: &str) -> std::path::PathBuf {
    Path::new(&std::env::var(var).unwrap_or_else(|_| panic!("{var} is set")))
        .canonicalize()
        .unwrap_or_else(|e| panic!("{var}: {e}"))
}

fn check(dir: &Path, channels_key: &str) -> std::process::Output {
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\ngroup = \"slack\"\n\
             function = \"ingest\"\n\n[steps.params.api]\n{channels_key} = [\"bridge\"]\n"
        ),
    )
    .unwrap();
    Command::new(bin("DATALIB_DAG_BIN"))
        .arg("--check")
        .arg(dir.join("config.toml"))
        .arg("--binary-dir")
        .arg(dir.join("bin"))
        .output()
        .expect("run datalib-dag --check")
}

#[test]
fn check_reports_a_misspelled_param_at_its_line() {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(bin("DATALIB_STEP_BIN"), td.path().join("bin/datalib-step"))
        .unwrap();

    let out = check(td.path(), "chanels");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("config.toml:10:1: rejected: step \"slack/ingest\": a sync would refuse"),
        "points at the misspelled key's line: {stderr}"
    );
    assert!(stderr.contains("chanels"), "{stderr}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 step(s) whose params a sync would refuse"),
        "{out:?}"
    );

    let out = check(td.path(), "channels");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}
