//! Re-export the prost-generated modules under stable paths.
//!
//! Two paths exist:
//!
//!   * Bazel build (`--cfg=bazel_prost`): `rust_prost_library` emits a
//!     separate crate `signal_backup_proto` whose package modules we
//!     re-export verbatim. This is the production path.
//!   * Cargo build (no cfg): `build.rs` runs `prost_build` and writes
//!     `signal.backup.rs` + `signal.backup.local.rs` into `$OUT_DIR`;
//!     we `include!` them under a hand-rolled module tree that
//!     mirrors the Bazel crate's shape. This path exists so the
//!     workspace's pre-commit `cargo clippy` succeeds without protoc
//!     installed system-wide (protoc is vendored via `protobuf-src`).

#[cfg(bazel_prost)]
pub use signal_backup_proto::signal::backup;
#[cfg(bazel_prost)]
pub use signal_backup_proto::signal::backup::local;

// prost-generated enums frequently include one large boxed-bytes variant
// and many small ones. Allow it — the diagnostic isn't actionable on
// codegen output.
#[cfg(not(bazel_prost))]
#[allow(clippy::large_enum_variant, clippy::doc_overindented_list_items)]
mod cargo {
    pub mod signal {
        pub mod backup {
            include!(concat!(env!("OUT_DIR"), "/signal.backup.rs"));
            pub mod local {
                include!(concat!(env!("OUT_DIR"), "/signal.backup.local.rs"));
            }
        }
    }
}

#[cfg(not(bazel_prost))]
pub use cargo::signal::backup;
#[cfg(not(bazel_prost))]
pub use cargo::signal::backup::local;
