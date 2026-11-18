//! A latchkey that datalib runs does not report itself to latchkey's usage
//! counter, which it otherwise pings once a day from any run.

use std::path::{Path, PathBuf};
use std::process::Command;

use datalib_runtime::node_runtime::{
    latchkey_command, LATCHKEY_DISABLE_COUNTING, LATCHKEY_VERSION, RUNTIME_DIR_ENV,
};

/// The file latchkey writes in its directory just before it sends the ping.
const COUNT_FILE: &str = "last-daily-count";

fn runfile(var: &str) -> PathBuf {
    let rel = std::env::var(var).unwrap_or_else(|_| panic!("{var} unset: is it on the test rule?"));
    runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"))
}

/// Runs a harmless command in a directory of its own. A stand-in curl means
/// that a ping, if latchkey sends one, goes nowhere.
fn run_in(mut cmd: Command, work: &Path, name: &str) -> PathBuf {
    let directory = work.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let out = cmd
        .args(["services", "list"])
        .env("HOME", work)
        .env("LATCHKEY_DIRECTORY", &directory)
        .env("LATCHKEY_CURL", "/usr/bin/true")
        .env(
            "LATCHKEY_ENCRYPTION_KEY",
            "a2tra2tra2tra2tra2tra2tra2tra2tra2tra2tra2s=",
        )
        .output()
        .expect("latchkey runs");
    assert!(
        out.status.success(),
        "latchkey failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    directory
}

/// Guards the README's "nothing leaves your machine" against the pinned
/// latchkey. The second half is the proof the first half means something: the
/// real package does count without the variable, so a bump that renames the
/// variable or the file fails here instead of quietly turning the ping back on.
#[test]
fn the_command_turns_off_latchkeys_daily_count() {
    let tree = tempfile::tempdir().unwrap();
    let node = tree.path().join("node/bin/node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(runfile("NODE_RLOC"), &node).unwrap();
    // The package is a link inside the pnpm store, and it resolves its
    // dependencies from its siblings there, so the store is what gets linked.
    let package = runfile("LATCHKEY_PACKAGE_RLOC");
    let store = package.parent().unwrap();
    let staged = tree.path().join("latchkey").join(LATCHKEY_VERSION);
    std::fs::create_dir_all(&staged).unwrap();
    std::os::unix::fs::symlink(store, staged.join("node_modules")).unwrap();
    // The only test in this binary, so no other thread reads the variable.
    std::env::set_var(RUNTIME_DIR_ENV, tree.path());
    let work = tempfile::tempdir().unwrap();

    let quiet = run_in(
        latchkey_command().expect("staged tree"),
        work.path(),
        "quiet",
    );
    assert!(
        !quiet.join(COUNT_FILE).exists(),
        "latchkey_command() left latchkey counting"
    );

    let mut counting = latchkey_command().expect("staged tree");
    counting.env_remove(LATCHKEY_DISABLE_COUNTING);
    let counted = run_in(counting, work.path(), "counted");
    assert!(
        counted.join(COUNT_FILE).exists(),
        "latchkey no longer writes {COUNT_FILE}, so this test no longer shows anything"
    );
}
