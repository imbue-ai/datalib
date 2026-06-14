//! Cargo-only proto codegen.
//!
//! Under Bazel the proto sources are compiled by `rust_prost_library`
//! (see `BUILD.bazel`), which injects the generated code as an
//! external crate `signal_backup_proto`; the rust_library target sets
//! `--cfg=bazel_prost` so `src/proto.rs` re-exports from that crate.
//!
//! Under cargo there's no such crate — the prost path inside the
//! workspace is Bazel-only — but the workspace's `cargo clippy`
//! pre-commit hook still needs to typecheck this crate. So we run
//! `prost_build` here at build time, write the generated modules into
//! `OUT_DIR`, and `src/proto.rs` `include!`s them under
//! `#[cfg(not(bazel_prost))]`.
//!
//! `protoc` is provided by `protobuf-src` (vendored prebuilt) so the
//! cargo path works on a clean host without a system protoc install.

// `println!` is the only way to communicate with cargo from a build
// script (it parses `cargo:` directives off stdout). The workspace's
// "no println" lint doesn't apply here.
#![allow(clippy::disallowed_macros)]

fn main() -> std::io::Result<()> {
    // Declare the `bazel_prost` cfg so consumers don't trip the
    // `unexpected_cfgs` lint when this build script runs.
    println!("cargo:rustc-check-cfg=cfg(bazel_prost)");
    // Bazel doesn't run this build script (`gen_build_script = "off"`
    // in MODULE.bazel for this crate) — but belt-and-suspenders.
    if std::env::var_os("BAZEL_OUT_BASE").is_some() {
        return Ok(());
    }
    // SAFETY: build scripts run single-threaded before the crate
    // begins compiling, so setting env vars here is sound. The
    // `unsafe` is required by the Rust 2024 edition's stricter
    // `std::env::set_var` signature.
    unsafe {
        std::env::set_var("PROTOC", protobuf_src::protoc());
    }
    // Generate prost types with serde derive so the extract path
    // can write `serde_json::to_string(&frame)?` directly. See
    // `docs/data_architecture_ingestion.md` §"Wire-fidelity": the
    // raw store records semantic content as JSON; the transcoding
    // from prost wire bytes is lossless and not a normalization.
    //
    // TODO(follow-up): bytes fields currently serialize as JSON
    // arrays of u8 numbers (the serde default for `Vec<u8>`).
    // Polish to base64/hex via `field_attribute` once we enumerate
    // the relevant field paths.
    prost_build::Config::new()
        .type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)]")
        .compile_protos(
            &["proto/Backup.proto", "proto/LocalArchive.proto"],
            &["proto"],
        )
}
