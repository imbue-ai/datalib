//! Datalib core: query engine. v0 skeleton.

// `config` left this crate long ago: naming every source `type:` puts it
// above the providers rather than in this base crate. What remains of it
// is the retired stanza schema in `datalib_migrate_config`.
pub mod db;
pub mod deeplink;
pub mod dolt_repo;
pub mod layout;
pub mod node_runtime;
pub mod qmd;
pub mod query;
pub mod repo;
pub mod search;
pub mod version;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
