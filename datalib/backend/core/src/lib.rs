//! Datalib core: the data-root layout, the stores this server owns, and
//! the host-runtime helpers every binary shares.
//!
//! Deliberately not here: anything that knows what a grid row or a qmd
//! hit is. That lives in `datalib_unified_index`, which only
//! `datalib-step` and `datalib-applet` link.

pub mod app_store;
pub mod deeplink;
pub mod layout;
pub mod node_runtime;
pub mod repo;
pub mod store;
pub mod version;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
