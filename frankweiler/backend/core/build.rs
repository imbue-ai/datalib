// `println!("cargo:...")` is the cargo build-script protocol — required
// here, exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Stamps the binary with the git commit SHA via `cargo:rustc-env`.
//!
//! Picked up at compile time by `crate::version::git_hash()` through
//! `option_env!("FRANKWEILER_GIT_HASH")`. If `git rev-parse` fails (no
//! `.git`, no `git` binary) the env var stays unset and `git_hash()`
//! reports the literal `"unknown"`.
//!
//! Bazel builds get the same value via `--workspace_status_command`
//! (`tools/workspace_status.sh` → `STABLE_GIT_HASH`). Wiring the stamp
//! file into `rustc_env_files` for the Bazel `rust_library` is a
//! follow-up — until then, Bazel-built binaries also report "unknown".

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // HEAD moves on commit; rerun on every build so the stamp stays fresh.
    // (Cheap: the build script itself is a couple of ms.)
    println!("cargo:rerun-if-env-changed=FRANKWEILER_GIT_HASH");
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                println!("cargo:rustc-env=FRANKWEILER_GIT_HASH={sha}");
            }
        }
    }
}
