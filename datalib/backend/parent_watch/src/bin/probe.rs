//! Stand-in processes for the crate's test: a `child` that watches its
//! parent and otherwise sleeps, and a `parent` that starts one — with
//! the pipe (`watch`) or without it (`nowatch`) — prints its pid, and
//! sleeps until the test kills it. `exec` is the same parent for any
//! program, so another crate's test can SIGKILL the parent of its own
//! binary.
//!
//! `child` writes its pid to `$PARENT_WATCH_PID_FILE` when that is set,
//! for a test that cannot read the pid off stdout because something else
//! started the process — the DAG runner starting it as a step.

// A test fixture with no progress display: stdout is how it reports the
// child's pid, and the parent never reaps because being SIGKILLed is
// its whole job.
#![allow(clippy::disallowed_macros, clippy::zombie_processes)]

use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(),
        Some("parent") => parent(args.get(2).map(String::as_str) == Some("watch")),
        Some("exec") if args.len() > 2 => exec(&args[2], &args[3..]),
        _ => {
            eprintln!(
                "usage: probe child | probe parent (watch|nowatch) | probe exec PROGRAM ARG…"
            );
            std::process::exit(64);
        }
    }
}

fn child() {
    if let Ok(path) = std::env::var("PARENT_WATCH_PID_FILE") {
        std::fs::write(path, std::process::id().to_string()).expect("write the pid file");
    }
    if let Err(e) = datalib_parent_watch::exit_with_parent(|| std::process::exit(0)) {
        eprintln!("probe child: {e}");
        std::process::exit(2);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn parent(watch: bool) {
    let mut cmd = Command::new(std::env::current_exe().expect("own path"));
    cmd.arg("child");
    if watch {
        cmd.env(datalib_parent_watch::ENV_VAR, "1");
    } else {
        cmd.env_remove(datalib_parent_watch::ENV_VAR);
    }
    hold(cmd);
}

fn exec(program: &str, args: &[String]) {
    let mut cmd = Command::new(program);
    cmd.args(args).env(datalib_parent_watch::ENV_VAR, "1");
    hold(cmd);
}

/// Start the child on a parent pipe, print its pid, and sleep until
/// killed. `child` stays in scope, so its stdin stays open until we are
/// gone.
fn hold(mut cmd: Command) {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let child = cmd.spawn().expect("spawn the child");
    println!("{}", child.id());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
