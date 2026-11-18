//! What the shell's own config lets a page do. The libraries screen can
//! delete a library, so it has a content security policy of its own; a
//! library's page has the one its server sends.

const CONF: &str = include_str!("../tauri.conf.json");
const LIBRARIES_SCREEN: &str = include_str!("../launcher-dist/index.html");

/// `"csp": null` let the libraries screen load script and style from anywhere.
#[test]
fn the_libraries_screen_has_a_policy() {
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "base-uri 'none'",
        "frame-ancestors 'none'",
    ] {
        assert!(
            CONF.contains(directive),
            "{directive} is not in tauri.conf.json"
        );
    }
}

/// `withGlobalTauri` puts `window.__TAURI__` on every page the app shows,
/// a library's server's included, and nothing needs it.
#[test]
fn no_page_gets_the_global() {
    assert!(!CONF.contains("withGlobalTauri"));
    assert!(!LIBRARIES_SCREEN.contains("__TAURI__."));
}
