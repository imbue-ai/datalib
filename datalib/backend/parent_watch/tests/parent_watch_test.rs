//! SIGKILL a parent and check its child noticed. The `nowatch` case is
//! the control: without the pipe the child lives on, which is what makes
//! the `watch` case's exit evidence rather than coincidence.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn probe() -> Command {
    Command::new(std::env::var("PARENT_WATCH_PROBE").expect("PARENT_WATCH_PROBE is set"))
}

fn spawn_parent(mode: &str) -> (Child, u32) {
    let mut parent = probe()
        .args(["parent", mode])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn probe parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read child pid");
    let pid = line.trim().parse().expect("child pid");
    (parent, pid)
}

/// A pid that is running, and not a zombie waiting for a reaper the
/// sandbox may not have.
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

fn sigkill(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

fn wait_until_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

#[test]
fn child_exits_when_its_parent_is_sigkilled() {
    let (mut parent, child) = spawn_parent("watch");
    assert!(
        alive(child),
        "child should be running before the parent dies"
    );
    parent.kill().expect("SIGKILL the parent");
    parent.wait().expect("reap the parent");
    assert!(
        wait_until_gone(child, Duration::from_secs(5)),
        "child {child} outlived its SIGKILLed parent"
    );
}

#[test]
fn without_the_pipe_the_child_outlives_its_parent() {
    let (mut parent, child) = spawn_parent("nowatch");
    parent.kill().expect("SIGKILL the parent");
    parent.wait().expect("reap the parent");
    std::thread::sleep(Duration::from_secs(1));
    let survived = alive(child);
    sigkill(child);
    assert!(
        survived,
        "the control child should have outlived its parent"
    );
}

#[test]
fn asked_to_watch_with_no_pipe_it_refuses_to_start() {
    let out = probe()
        .arg("child")
        .env(datalib_parent_watch::ENV_VAR, "0")
        .stdin(Stdio::null())
        .output()
        .expect("run probe child");
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a pipe"), "stderr: {stderr}");
}
