// `println!("cargo:...")` is the cargo build-script protocol — required
// here, exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Stamps `frankweiler-etl`'s binaries (notably `latchkey-curl-impersonate`)
//! with build metadata via `cargo:rustc-env` for `cargo build` users.
//! Bazel builds get the same values from `rustc_env.txt` +
//! `--workspace_status_command=tools/workspace_status.sh`, so this
//! build.rs is the cargo-side counterpart only. Mirror of
//! `frankweiler/backend/sync/build.rs`.
//!
//! Emitted env:
//!   FRANKWEILER_GIT_HASH       full HEAD SHA
//!   FRANKWEILER_VERSION        `git describe --tags --always --dirty`

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FRANKWEILER_GIT_HASH");
    println!("cargo:rerun-if-env-changed=FRANKWEILER_VERSION");
    println!(
        "cargo:rustc-env=FRANKWEILER_GIT_HASH={}",
        git("rev-parse", &["HEAD"])
    );
    println!(
        "cargo:rustc-env=FRANKWEILER_VERSION={}",
        git("describe", &["--tags", "--always", "--dirty"]),
    );
}

fn git(subcmd: &str, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg(subcmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
