// `println!("cargo:...")` is the cargo build-script protocol — required
// here, exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Defaults `$DATALIB_UI_DIST` for the `rust-embed` proc macro
//! when building with cargo.
//!
//! Resolution at compile time:
//!   1. `$DATALIB_UI_DIST` already set in the environment — used
//!      verbatim. The Bazel `rust_library.rustc_env` populates this
//!      with `$(execpath //datalib/ui:dist)`, the hermetic vite
//!      output.
//!   2. Cargo fallback: `<crate>/../../ui/dist`. The caller is
//!      expected to have run `pnpm build` (or
//!      `bazel build //datalib/ui:dist` + symlink) before
//!      `cargo build` — otherwise the proc macro produces an empty
//!      asset set and the resulting binary will 500 on every UI request.
//!
//! Cargo doesn't actually build the http crate end-to-end today
//! because the workspace's sqlx-sqlite/doltlite wiring expects
//! Bazel-built headers. But cargo's precommit check (`cargo check`)
//! still runs proc macros, so this fallback exists to keep that lint
//! green.

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
