//! Frankweiler core: query engine. v0 skeleton.

pub mod config;
pub mod db;
pub mod deeplink;
pub mod dolt_repo;
pub mod dolt_server;
pub mod qmd;
pub mod query;
pub mod repo;
pub mod search;
pub mod sqlite_repo;
pub mod version;
pub mod worker;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
