//! Tauri command surface for Frankweiler. v0 skeleton — no commands wired yet.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
