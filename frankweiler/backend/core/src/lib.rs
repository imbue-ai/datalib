//! Frankweiler core: query engine. v0 skeleton.

pub mod config;
pub mod db;
pub mod deeplink;
pub mod dolt_repo;
pub mod dolt_server;
pub mod query;
pub mod repo;
pub mod sqlite_repo;
pub mod search;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
