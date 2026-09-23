//! Two `datalib-dag` invocations on one data root: the second hands its
//! request to the loop the first is running, instead of failing on the
//! lock, and each exits with its own request's outcome. This is how an
//! agent's sync runs while the app's does (`docs/dev/plans/supervisor.md`
//! §2.8). Each step holds until the test writes `go`, so "beside" is
//! observed, not timed.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dag_bin() -> String {
    let p = Path::new(&std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .canonicalize()
        .expect("DATALIB_DAG_BIN");
    p.to_string_lossy().into_owned()
}

fn root_with_two_sources() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let hold = |id: &str| {
        format!(
            "[[steps]]\nid = \"{id}\"\ncommand = \"sh -c 'mkdir -p {id}; touch {id}/started; \
             while [ ! -f go ]; do sleep 0.05; done'\"\n"
        )
    };
    std::fs::write(
        td.path().join("config.toml"),
        format!("{}{}", hold("a/src"), hold("b/src")),
    )
    .unwrap();
    td
}

fn sync(root: &Path, source: &str) -> Child {
    Command::new(dag_bin())
        .arg(root.join("config.toml"))
        .args(["--sync", source])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn datalib-dag")
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finish(child: Child) -> (i32, String) {
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.code().unwrap_or(-1), stderr)
}

#[test]
fn a_second_sync_joins_the_running_loop_and_runs_beside_the_first() {
    let td = root_with_two_sources();
    let root = td.path();
    let first = sync(root, "a/src");
    wait_for("a to start", || root.join("a/src/started").exists());

    let second = sync(root, "b/src");
    wait_for("b to start while a still runs", || {
        root.join("b/src/started").exists()
    });
    std::fs::write(root.join("go"), "").unwrap();

    let (code, stderr) = finish(second);
    assert_eq!(code, 0, "the second invocation: {stderr}");
    assert!(stderr.contains("following request"), "{stderr}");
    let (code, stderr) = finish(first);
    assert_eq!(code, 0, "the first invocation: {stderr}");
}

#[test]
fn stopping_the_second_sync_stops_its_request_and_leaves_the_first_alone() {
    let td = root_with_two_sources();
    let root = td.path();
    let first = sync(root, "a/src");
    wait_for("a to start", || root.join("a/src/started").exists());
    let second = sync(root, "b/src");
    wait_for("b to start", || root.join("b/src/started").exists());

    // Safety: kill(2) with SIGINT on a child we spawned.
    unsafe { libc::kill(second.id() as libc::pid_t, libc::SIGINT) };
    let (code, stderr) = finish(second);
    assert_eq!(code, 130, "a stopped request exits 130: {stderr}");

    std::fs::write(root.join("go"), "").unwrap();
    let (code, stderr) = finish(first);
    assert_eq!(
        code, 0,
        "the first sync is not the one that was stopped: {stderr}"
    );
}
