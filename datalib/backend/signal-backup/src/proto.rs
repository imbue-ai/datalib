//! Re-export the prost-generated modules under stable paths.

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
