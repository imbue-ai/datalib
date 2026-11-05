//! What a library's page may ask the shell to do, granted for that
//! library's server alone. Its port is picked when it starts, so these
//! cannot be files Tauri reads at build time: a pattern there could only
//! say "any port of this machine". They are granted as each library opens,
//! and Tauri has no way to take one back, so a server the app has left keeps
//! its grant until the app quits; `navigation.rs` is what keeps a window from
//! showing anything on that port once it is no longer the open library.
//!
//! Nothing here may reference `tauri`: `//datalib/tauri:library_capabilities_test`
//! compiles this file on its own.

use url::Url;

const ORIGIN_SLOT: &str = "{{origin}}";

const TEMPLATES: &[&str] = &[
    include_str!("../library-capabilities/confirm-actions.json"),
    include_str!("../library-capabilities/open-external-urls.json"),
    include_str!("../library-capabilities/open-raw-stores.json"),
    include_str!("../library-capabilities/pick-local-paths.json"),
    include_str!("../library-capabilities/reveal-local-files.json"),
    include_str!("../library-capabilities/switch-libraries.json"),
    include_str!("../library-capabilities/window-drag.json"),
];

/// Every capability, as the JSON Tauri takes, for the server at `url`.
pub fn for_server(url: &Url) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    TEMPLATES
        .iter()
        .map(|template| template.replace(ORIGIN_SLOT, &origin))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const TREE: &str = "datalib/tauri";

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn files_in(dir: &str) -> Vec<(String, String)> {
        let dir = format!("{TREE}/{dir}");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .map(|entry| {
                let path = entry.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap();
                (path.display().to_string(), text)
            })
            .collect();
        assert!(!files.is_empty(), "nothing in {dir}");
        files
    }

    /// `http://127.0.0.1:*/**` gave IPC to a page on any port of this
    /// machine; the patterns name the one server now.
    #[test]
    fn every_grant_names_the_one_server_and_nothing_else() {
        let url: Url = "http://127.0.0.1:61234/?token=t0ken".parse().unwrap();
        let granted = for_server(&url);
        assert!(!granted.is_empty());
        for capability in granted {
            assert!(!capability.contains(ORIGIN_SLOT), "{capability}");
            assert!(!capability.contains("t0ken"), "{capability}");
            let parsed = json(&capability);
            assert_eq!(
                parsed["remote"]["urls"],
                serde_json::json!(["http://127.0.0.1:61234/**"]),
                "{}",
                parsed["identifier"]
            );
        }
    }

    /// A template left out of the list grants nothing, and the button it
    /// is for does nothing with no message.
    #[test]
    fn every_template_on_disk_is_granted() {
        let on_disk = files_in("library-capabilities");
        assert_eq!(on_disk.len(), TEMPLATES.len());
        for (path, text) in on_disk {
            assert!(TEMPLATES.contains(&text.as_str()), "{path} is not listed");
        }
    }

    /// Tauri reads this folder at build time, so a `remote` here has no
    /// way to know the server's port, and would have to be a wildcard.
    #[test]
    fn no_capability_read_at_build_time_reaches_a_remote_page() {
        for (path, text) in files_in("capabilities") {
            assert!(json(&text).get("remote").is_none(), "{path}");
        }
    }

    /// The libraries screen can delete a library. It has a policy of its
    /// own; a library's page has the one its server sends. `__TAURI__` is
    /// the convenience global `withGlobalTauri` adds to every page, the
    /// server's included, and nothing uses it.
    #[test]
    fn the_libraries_screen_has_a_policy_and_no_page_has_the_global() {
        let conf = json(&std::fs::read_to_string(format!("{TREE}/tauri.conf.json")).unwrap());
        let csp = conf["app"]["security"]["csp"].as_str().unwrap_or_default();
        for directive in ["default-src 'none'", "script-src 'self'", "base-uri 'none'"] {
            assert!(csp.contains(directive), "{directive} not in {csp:?}");
        }
        assert_ne!(conf["app"]["withGlobalTauri"], true);
        let page = std::fs::read_to_string(format!("{TREE}/launcher-dist/index.html")).unwrap();
        assert!(!page.contains("__TAURI__."));
    }
}
