//! The steering verbs, each run as its own process the way an agent at a
//! shell runs them: `status`, `stop`, `pause`, `resume` write a row and
//! return, and the loop — here another `datalib-dag` — acts on it.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn dag_bin() -> String {
    let p = Path::new(&std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .canonicalize()
        .expect("DATALIB_DAG_BIN");
    p.to_string_lossy().into_owned()
}

/// One source that marks it started and holds until `go` exists.
fn root_with_a_source() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(
        td.path().join("config.toml"),
        "[[steps]]\nid = \"a/src\"\ncommand = \"sh -c 'mkdir -p a/src; touch a/src/started; \
         while [ ! -f go ]; do sleep 0.05; done'\"\n",
    )
    .unwrap();
    td
}

fn dag(root: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(dag_bin());
    match args.split_first() {
        Some((verb, rest)) if ["status", "stop", "pause", "resume"].contains(verb) => {
            cmd.arg(verb).arg(root.join("config.toml")).args(rest)
        }
        _ => cmd.arg(root.join("config.toml")).args(args),
    };
    cmd.output().expect("run datalib-dag")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_paused_source_does_not_run_until_it_is_resumed() {
    let td = root_with_a_source();
    let root = td.path();
    std::fs::write(root.join("go"), "").unwrap();

    let out = dag(root, &["pause", "a/src", "--by", "claude"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&dag(root, &["status"])).contains("paused a/src  by claude"),
        "status names the pause and who made it"
    );

    let out = dag(root, &["--sync", "a/src"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!root.join("a/src/started").exists(), "a paused step ran");

    let out = dag(root, &["resume", "a/src"]);
    assert!(stdout(&out).contains("which claude had paused"), "{out:?}");
    let out = dag(root, &["--sync", "a/src"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(root.join("a/src/started").exists(), "resumed, it runs");
}

#[test]
fn a_sync_is_stopped_from_another_shell_by_its_request_id() {
    let td = root_with_a_source();
    let root = td.path();
    let sync: Child = Command::new(dag_bin())
        .arg(root.join("config.toml"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the sync");
    wait_for("the source to start", || {
        root.join("a/src/started").exists()
    });

    let status = stdout(&dag(root, &["status"]));
    let id = status
        .lines()
        .find_map(|l| l.strip_prefix("request "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no open request in: {status}"))
        .to_string();

    let out = dag(root, &["stop", &id, "--by", "claude"]);
    assert!(stdout(&out).contains("asked request"), "{out:?}");
    let done = sync.wait_with_output().expect("wait for the sync");
    assert_eq!(done.status.code(), Some(130), "{done:?}");
    assert!(
        stdout(&dag(root, &["stop", &id])).contains("already over: stopped"),
        "a second stop says what became of it"
    );
}

#[test]
fn a_verb_on_a_step_the_config_lacks_says_which_steps_it_has() {
    let td = root_with_a_source();
    let out = dag(td.path(), &["pause", "b/src"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("its steps: a/src"),
        "{out:?}"
    );
}
