// `println!("cargo:...")` is the cargo build-script protocol — required
// here, exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Defaults `$DATALIB_UI_DIST` for the `rust-embed` proc macro
//! when building with cargo.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DATALIB_UI_DIST");

    let dist = match std::env::var_os("DATALIB_UI_DIST") {
        Some(v) => PathBuf::from(v),
        None => {
            let manifest_dir =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
            manifest_dir.join("../../ui/dist")
        }
    };
    if !dist.is_dir() {
        println!(
            "cargo:warning=DATALIB_UI_DIST does not exist: {} (run `pnpm build` in datalib/ui/)",
            dist.display()
        );
    }
    println!("cargo:rustc-env=DATALIB_UI_DIST={}", dist.display());
}
