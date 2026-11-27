//! The runner needs no `datalib-step`: a config made only of shell
//! scripts runs, resets a step whose program takes no verbs, and starts
//! again, with no `datalib-step` beside the runner or on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PATH: &str = "/usr/bin:/bin";

const CONFIG: &str = r#"
[env]
GREETING = "from-config"
SHADOWED = "from-config"

[[programs]]
command = "./prep.sh"
verbs = ["prepare"]

[[groups]]
id = "src"
type = "anything-at-all"

[[steps]]
id = "src/fetch"
command = "sh fetch.sh"
params = { since = "2026-01-01" }
env = { SHADOWED = "from-step" }

[[steps]]
id = "src/render"
command = "sh render.sh"
inputs = ["src/fetch"]

[[steps]]
id = "src/prepared"
command = "./prep.sh"
"#;

const FETCH: &str = r#"echo "fetch $#:$*" >> calls.log
mkdir -p src/fetch
printf '%s\n%s\n%s\n' "$DATALIB_DAG_PARAMS" "$GREETING" "$SHADOWED" > src/fetch/env
"#;

const RENDER: &str = r#"echo "render $#:$DATALIB_DAG_CHANGED_INPUTS" >> calls.log
mkdir -p src/render
cp src/fetch/env src/render/env
"#;

const PREP: &str = "#!/bin/sh\necho \"prepared $*\" >> calls.log\nmkdir -p src/prepared\n";

/// The runner, copied alone into a directory of its own: the directory a
/// step's `PATH` starts with when the config names no `binary_dir`.
fn lone_runner(dir: &Path) -> PathBuf {
    let src =
        std::path::absolute(std::env::var("DATALIB_PIPELINE_BIN").expect("DATALIB_PIPELINE_BIN"))
            .expect("absolute DATALIB_PIPELINE_BIN");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let dag = bin.join("datalib-pipeline");
    std::fs::copy(&src, &dag).unwrap();
    executable(&dag);
    dag
}

fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `datalib-pipeline <verb> <root> <rest>`.
fn dag(bin: &Path, root: &Path, args: &[&str]) -> Output {
    let (verb, rest) = args.split_first().expect("a verb");
    let out = Command::new(bin)
        .arg(verb)
        .arg(root)
        .args(rest)
        .env("PATH", PATH)
        .env_remove("RUST_LOG")
        .output()
        .expect("run datalib-pipeline");
    assert!(
        out.status.success(),
        "datalib-pipeline {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn calls(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count(calls: &[String], line: &str) -> usize {
    calls.iter().filter(|c| *c == line).count()
}

/// The plan's proof that the runner is general: if any part of a run, a
/// reset or a start still reached for `datalib-step`, a step here would
/// fail to spawn and the run would not succeed.
#[test]
fn shell_steps_run_reset_and_start_again_with_no_datalib_step_anywhere() {
    let td = tempfile::tempdir().unwrap();
    let bin = lone_runner(td.path());
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    for dir in std::iter::once(bin.parent().unwrap()).chain(PATH.split(':').map(Path::new)) {
        assert!(
            !dir.join("datalib-step").exists(),
            "{} holds a datalib-step",
            dir.display()
        );
    }
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    std::fs::write(root.join("fetch.sh"), FETCH).unwrap();
    std::fs::write(root.join("render.sh"), RENDER).unwrap();
    std::fs::write(root.join("prep.sh"), PREP).unwrap();
    executable(&root.join("prep.sh"));

    dag(&bin, &root, &["check"]);

    // A run: every step runs, the declared program's step is prepared
    // before it runs, and the scripts get what the protocol promises.
    dag(&bin, &root, &["sync"]);
    let first = calls(&root);
    let at = |line: &str| first.iter().position(|l| l == line);
    assert!(
        at("prepared --prepare") < at("prepared "),
        "prepare comes before its step runs: {first:?}"
    );
    assert_eq!(count(&first, "fetch 0:"), 1, "{first:?}");
    assert_eq!(count(&first, "render 0:src/fetch"), 1, "{first:?}");
    assert_eq!(count(&first, "prepared "), 1, "{first:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("src/render/env")).unwrap(),
        "{\"since\":\"2026-01-01\"}\nfrom-config\nfrom-step\n",
        "params as JSON, [env] below the step's own env"
    );

    // A reset of a step whose program takes no verbs: never invoked,
    // its tree deleted. As the app's reset does, it then runs what reads
    // the download, which takes the emptiness downstream, and does not
    // refill the download itself.
    std::fs::write(root.join("src/fetch/planted"), "").unwrap();
    dag(&bin, &root, &["reset", "src/fetch"]);
    assert!(!root.join("src/fetch").exists(), "the tree is deleted");
    let after_reset = calls(&root);
    assert_eq!(
        count(&after_reset, "fetch 0:"),
        1,
        "a reset does not refill the download: {after_reset:?}"
    );
    assert_eq!(
        count(&after_reset, "render 0:src/fetch"),
        2,
        "what reads it runs: {after_reset:?}"
    );

    // A start: the reset step runs again, and so does what reads it.
    dag(&bin, &root, &["sync"]);
    let all = calls(&root);
    assert_eq!(count(&all, "fetch 0:"), 2, "{all:?}");
    assert_eq!(count(&all, "render 0:src/fetch"), 3, "{all:?}");
    assert!(root.join("src/fetch/env").exists());
    assert!(!root.join("src/fetch/planted").exists());
    assert_eq!(
        count(&all, "prepared --prepare"),
        3,
        "prepare at every start, the reset's included: {all:?}"
    );
    assert!(
        all.iter()
            .filter(|c| c.starts_with("fetch") || c.starts_with("render"))
            .all(|c| c.starts_with("fetch 0:") || c.starts_with("render 0:")),
        "a program with no [[programs]] entry is sent no verb: {all:?}"
    );
}
