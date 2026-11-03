//! The report `datalib-dag` prints on stdout at the end of a sync: one
//! line per step, saying how the run went for it — not how its last pass
//! went. A consumer of a streaming producer runs once per seal, so the two
//! differ.

use std::path::Path;
use std::process::Command;

fn dag_bin() -> std::path::PathBuf {
    std::path::absolute(std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .expect("absolute DATALIB_DAG_BIN")
}

/// A producer that streams: it seals `v1`, waits for its consumer's first
/// pass to start, and ends on `v2`, so the consumer runs twice. The
/// consumer's two passes are `first` and `second`, shell run in the data
/// root.
fn root_with_two_passes(first: &str, second: &str) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(
        root.join("a.sh"),
        r#"echo '{"event":"capabilities","step":"me","streams_output":true}'
echo '{"event":"checkpoint","step":"me","version":"v1"}'
while [ ! -f c-pass-1 ]; do sleep 0.05; done
echo '{"event":"outcome","outputs":[{"path":"a/src","version":"v2"}]}'
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("c.sh"),
        format!(
            "if [ -f c-pass-1 ]; then\n  touch c-pass-2\n  {second}\nelse\n  touch c-pass-1\n  \
             {first}\nfi\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("config.toml"),
        "[[steps]]\nid = \"a/src\"\ncommand = \"sh a.sh\"\n\n\
         [[steps]]\nid = \"c/idx\"\ncommand = \"sh c.sh\"\ninputs = [\"a/src\"]\n",
    )
    .unwrap();
    td
}

const REPORTS_C1: &str =
    r#"echo '{"event":"outcome","outputs":[{"path":"c/idx","version":"c1"}]}'"#;

fn sync(root: &Path) -> (Option<i32>, String) {
    let out = Command::new(dag_bin())
        .arg(root.join("config.toml"))
        .output()
        .expect("run datalib-dag");
    assert!(root.join("c-pass-2").exists(), "c ran twice: {out:?}");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn line_of<'a>(stdout: &'a str, step: &str) -> &'a str {
    stdout
        .lines()
        .find(|l| l.starts_with(step))
        .unwrap_or_else(|| panic!("no line for {step} in:\n{stdout}"))
}

/// The consumer's first pass wrote `c1` and its second wrote `c1` again.
/// The run changed its output; the line said `changed: 0`, which is what
/// the last pass alone did.
#[test]
fn a_step_whose_first_pass_changed_its_output_reads_changed() {
    let td = root_with_two_passes(REPORTS_C1, REPORTS_C1);
    let (code, stdout) = sync(td.path());
    assert_eq!(code, Some(0), "{stdout}");
    let c = line_of(&stdout, "c/idx");
    assert!(c.contains("Succeeded { changed: 1 }"), "{c}");
}

/// A first pass that failed, then a second that succeeded: the line was
/// a plain success, with nothing to say a pass had failed.
#[test]
fn a_failed_pass_that_ran_again_is_on_the_steps_line() {
    let td = root_with_two_passes("echo boom >&2; exit 1", REPORTS_C1);
    let (code, stdout) = sync(td.path());
    assert_eq!(code, Some(0), "{stdout}");
    let c = line_of(&stdout, "c/idx");
    assert!(c.contains("Succeeded"), "{c}");
    assert!(
        c.contains("1 pass failed and was run again; its error is in the step's log"),
        "{c}"
    );
}
