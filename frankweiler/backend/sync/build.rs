//! Stamps `frankweiler-sync` with the git commit SHA via `cargo:rustc-env`
//! for `cargo build` users. Bazel builds get the same value from
//! `rustc_env.txt` + `--workspace_status_command=tools/workspace_status.sh`,
//! so this build.rs is the cargo-side counterpart only.
//!
//! Surfaced at runtime in `main.rs` via `option_env!("FRANKWEILER_GIT_HASH")`
//! and rendered by clap's `--version`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FRANKWEILER_GIT_HASH");
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    // Always emit so main.rs can use `env!` (compile-time concat) rather
    // than `option_env!`, which doesn't compose with `concat!`.
    println!("cargo:rustc-env=FRANKWEILER_GIT_HASH={sha}");
}
