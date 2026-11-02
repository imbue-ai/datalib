//! A bundled tool, run by the pinned Node, keeps its debugger shut when
//! another program sends it `SIGUSR1` (#1515).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;

use datalib_runtime::node_runtime::{bundled_command, RUNTIME_DIR_ENV};

/// Answers each line on stdin with the debugger's address, if one is open.
const PROBE: &str = r#"
import inspector from "node:inspector";
import readline from "node:readline";
console.log("ready");
readline.createInterface({ input: process.stdin }).on("line", () =>
  console.log(inspector.url() ?? "no debugger"));
"#;

fn pinned_node() -> PathBuf {
    let rel = std::env::var("NODE_RLOC").expect("NODE_RLOC unset: is it on the test rule?");
    runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("NODE_RLOC={rel} not in runfiles"))
}

/// Guards #1515: started without `--disable-sigusr1`, Node opens its
/// debugger to any program that signals it, and a running latchkey's
/// sign-ins can be read out through it.
#[test]
fn a_signal_does_not_open_the_debugger() {
    let tree = tempfile::tempdir().unwrap();
    let node = tree.path().join("node/bin/node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(pinned_node(), &node).unwrap();
    let entry = tree.path().join("probe/1.0.0/probe.mjs");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, PROBE).unwrap();
    // The only test in this binary, so no other thread reads the variable.
    std::env::set_var(RUNTIME_DIR_ENV, tree.path());

    let mut cmd = bundled_command("probe", "1.0.0", "probe.mjs").expect("staged tree");
    // A debugger that does open takes a free port, not 9229, which
    // another process on the machine may hold.
    cmd.env("NODE_OPTIONS", "--inspect-port=0");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");

    let pid = libc::pid_t::try_from(child.id()).unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGUSR1) }, 0);

    // A debugger the signal opens is listening within a turn or two of
    // the event loop; twenty round trips give it every chance to show.
    let answers: Vec<String> = (0..20)
        .map(|_| {
            writeln!(stdin, "?").unwrap();
            line.clear();
            stdout.read_line(&mut line).unwrap();
            line.trim().to_string()
        })
        .collect();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        answers.iter().all(|a| a == "no debugger"),
        "SIGUSR1 opened the debugger: {answers:?}"
    );
}
