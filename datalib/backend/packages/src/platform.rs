//! The platforms a package may ship a build for: the ones datalib itself
//! is released for (`.github/workflows/release.yml`), plus `any` for a
//! build that runs everywhere, such as a script.

pub const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// The build a manifest may offer for every platform.
pub const ANY: &str = "any";

/// The target triple this binary was built for. A platform datalib is
/// not released for still gets its triple, so a package's `any` build
/// works there.
pub fn this_target() -> &'static str {
    match (
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(target_env = "musl"),
    ) {
        ("aarch64", "macos", _) => "aarch64-apple-darwin",
        ("x86_64", "macos", _) => "x86_64-apple-darwin",
        ("x86_64", "linux", false) => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux", false) => "aarch64-unknown-linux-gnu",
        ("x86_64", "linux", true) => "x86_64-unknown-linux-musl",
        ("aarch64", "linux", true) => "aarch64-unknown-linux-musl",
        _ => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_target_is_a_triple() {
        assert_ne!(this_target(), "unsupported");
    }
}
