//! Datalib core: the data-root layout, the stores this server owns, and
//! the host-runtime helpers every binary shares.

pub mod app_store;
mod app_store_migrate;
pub mod deeplink;
pub mod disk;
pub mod repo;
pub mod store;

/// The data-root layout and the bundled-Node resolver live in
/// `datalib_runtime`, a crate with no dependencies, so small crates can
/// link them without this one. Re-exported here so every
/// `datalib_core::layout::…` / `datalib_core::node_runtime::…` call site
/// resolves.
pub use datalib_runtime::{layout, node_runtime};

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
