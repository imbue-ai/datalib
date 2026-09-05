// `println!("cargo:...")` is the cargo build-script protocol — required
// here, exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Stamps the binary with the git commit SHA via `cargo:rustc-env`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // HEAD moves on commit; rerun on every build so the stamp stays fresh.
    // (Cheap: the build script itself is a couple of ms.)
    println!("cargo:rerun-if-env-changed=DATALIB_GIT_HASH");
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                println!("cargo:rustc-env=DATALIB_GIT_HASH={sha}");
            }
        }
    }
}
