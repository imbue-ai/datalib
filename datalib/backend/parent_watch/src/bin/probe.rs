//! Stand-in processes for the crate's test: a `child` that watches its
//! parent and otherwise sleeps, and a `parent` that starts one — with
//! the pipe (`watch`) or without it (`nowatch`) — prints its pid, and
//! sleeps until the test kills it.

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
        _ => {
            eprintln!("usage: probe child | probe parent (watch|nowatch)");
            std::process::exit(64);
        }
    }
}

fn child() {
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
    cmd.arg("child")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if watch {
        cmd.env(datalib_parent_watch::ENV_VAR, "1");
    } else {
        cmd.env_remove(datalib_parent_watch::ENV_VAR);
    }
    let child = cmd.spawn().expect("spawn probe child");
    println!("{}", child.id());
    // `child` stays in scope, so its stdin stays open until we are gone.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
