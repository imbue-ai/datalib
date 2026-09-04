//! Datalib core: the data-root layout, the stores this server owns, and
//! the host-runtime helpers every binary shares.
//!
//! Deliberately not here: anything that knows what a grid row or a qmd
//! hit is. That lives in `datalib_unified_index`, which only
//! `datalib-step` and `datalib-applet` link.

pub mod app_store;
pub mod deeplink;
pub mod repo;
pub mod store;
pub mod version;

/// The data-root layout and the bundled-Node resolver moved down into
/// `datalib_runtime`, a crate with no dependencies, so that
/// `qmd_indexer_bin` can link them without dragging this crate (and its
/// 160 rdeps) into the digest of the fixture's embedding action. See
/// `datalib_runtime`'s crate docs. Re-exported here so every existing
/// `datalib_core::layout::…` / `datalib_core::node_runtime::…` call site
/// still resolves.
pub use datalib_runtime::{layout, node_runtime};

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
