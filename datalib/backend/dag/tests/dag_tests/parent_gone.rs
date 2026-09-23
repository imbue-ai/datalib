//! The runner exits with the process that started it, and its steps
//! with the runner — including when the runner is the one that is
//! SIGKILLed and so runs no code of its own. The app server's worker
//! spawns `datalib-dag` on a parent pipe; when the server is SIGKILLed —
//! the desktop shell's way of stopping it — the run must not carry on
//! with nobody to record how it ended. The steps write their pid first,
//! so the test can check what was taken along.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn env_path(var: &str) -> String {
    let p = Path::new(&std::env::var(var).unwrap_or_else(|_| panic!("{var} is set")))
        .canonicalize()
        .unwrap_or_else(|e| panic!("{var}: {e}"));
    p.to_string_lossy().into_owned()
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

fn wait_for_file(path: &Path, within: Duration) -> String {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path) {
            if s.trim().parse::<u32>().is_ok() {
                return s.trim().to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", path.display());
}

#[test]
fn the_runner_and_its_steps_exit_when_the_parent_is_sigkilled() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().canonicalize().unwrap();
    std::fs::write(
        root.join("config.toml"),
        "[[steps]]\nid = \"slow/step\"\n\
         command = \"sh -c 'mkdir -p slow/step; echo $$ > slow/step/pid; exec sleep 300'\"\n",
    )
    .unwrap();

    let mut parent = Command::new(env_path("PARENT_WATCH_PROBE"))
        .args(["exec", &env_path("DATALIB_DAG_BIN")])
        .arg(root.join("config.toml"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the probe parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read the runner's pid");
    let runner: u32 = line.trim().parse().expect("runner pid");
    let step: u32 = wait_for_file(&root.join("slow/step/pid"), Duration::from_secs(60))
        .parse()
        .unwrap();
    assert!(alive(runner) && alive(step), "runner {runner}, step {step}");

    parent.kill().expect("SIGKILL the parent");
    parent.wait().expect("reap the parent");
    let runner_gone = wait_until_gone(runner, Duration::from_secs(10));
    let step_gone = wait_until_gone(step, Duration::from_secs(10));
    for pid in [runner, step] {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    assert!(
        runner_gone,
        "the runner {runner} outlived its SIGKILLed parent"
    );
    assert!(step_gone, "the step {step} outlived the runner");
}

/// A step stops itself when the *runner* is SIGKILLed, which runs no
/// runner code at all: `kill_children` never gets a chance, and nothing
/// else ever signals a step. A step that says it watches the runner is
/// given a pipe from it, and stops when it reads EOF on that.
///
/// `watches_runner` is a declaration about the program, so the step here
/// is the parent-watch probe — what a program that honours it looks
/// like. Without the pipe the probe watches nothing, sleeps for an hour,
/// and this times out.
#[test]
fn a_watching_step_exits_when_the_runner_itself_is_sigkilled() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().canonicalize().unwrap();
    let pid_file = root.join("step.pid");
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[[steps]]\nid = \"watched/step\"\n\
             command = \"'{}' child\"\nwatches_runner = true\n",
            env_path("PARENT_WATCH_PROBE")
        ),
    )
    .unwrap();

    let mut parent = Command::new(env_path("PARENT_WATCH_PROBE"))
        .args(["exec", &env_path("DATALIB_DAG_BIN")])
        .arg(root.join("config.toml"))
        // Inherited down through the runner to the step. The runner's own
        // stdout is the event stream, so there is nowhere else to read
        // the step's pid from.
        .env("PARENT_WATCH_PID_FILE", &pid_file)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the probe parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read the runner's pid");
    let runner: u32 = line.trim().parse().expect("runner pid");
    let step: u32 = wait_for_file(&pid_file, Duration::from_secs(60))
        .parse()
        .unwrap();
    assert!(alive(runner) && alive(step), "runner {runner}, step {step}");

    // SIGKILL, so the runner runs nothing at all on its way out —
    // `kill_children` included. That is the whole point of the case.
    let killed = Command::new("kill")
        .args(["-9", &runner.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success(), "kill -9 {runner}");
    let runner_gone = wait_until_gone(runner, Duration::from_secs(10));
    let step_gone = wait_until_gone(step, Duration::from_secs(30));
    let _ = parent.kill();
    let _ = parent.wait();
    for pid in [runner, step] {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    assert!(runner_gone, "the runner {runner} survived a SIGKILL");
    assert!(
        step_gone,
        "the step {step} outlived the runner that was SIGKILLed"
    );
}
