//! Frankweiler core: query engine. v0 skeleton.

// `config` relocated to the `frankweiler_ingest_config` crate (Program A): it
// names every source `type:`, so it sits above the providers rather than in
// this base crate.
pub mod db;
pub mod deeplink;
pub mod dolt_repo;
pub mod layout;
pub mod node_runtime;
pub mod qmd;
pub mod query;
pub mod repo;
pub mod search;
pub mod sync_phase;
pub mod version;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
