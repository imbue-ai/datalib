//! Frankweiler core: query engine. v0 skeleton.

pub mod config;
pub mod deeplink;
pub mod query;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
