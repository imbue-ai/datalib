//! Where a window of the app may go: the rule `main.rs` applies to every
//! navigation and every new window a page asks for. Anything not named
//! here as the app's, or as a link the OS opens, goes nowhere — a page
//! can name any scheme, and a scheme this list does not know is one the
//! app has no business following.
//!
//! Nothing here may reference `tauri`: `//datalib/tauri:navigation_test`
//! compiles this file on its own.

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A page of the app: the open library's server, or the libraries
    /// screen the shell serves itself.
    App,
    /// A link the OS opens, in the person's browser or mail or phone app.
    Os,
    /// Refused.
    Nowhere,
}

/// Where a window that is navigating to `next` ends up. A document's
/// frame starts as `about:srcdoc`, and a blank frame as `about:blank`;
/// both are the page's own.
pub fn navigation(next: &Url, app: Option<&Url>, launcher: &Url) -> Destination {
    if matches!(next.as_str(), "about:srcdoc" | "about:blank") {
        return Destination::App;
    }
    new_window(next, app, launcher)
}

/// Where a link asking for a new window goes: a second window of the app
/// for one of the app's pages.
pub fn new_window(next: &Url, app: Option<&Url>, launcher: &Url) -> Destination {
    if [Some(launcher), app]
        .into_iter()
        .flatten()
        .any(|home| same_place(next, home))
    {
        Destination::App
    } else if opens_outside(next) {
        Destination::Os
    } else {
        Destination::Nowhere
    }
}

/// The schemes the OS is asked to open: the same set the page's own
/// opener may hand out (`library-capabilities/open-external-urls.json`).
fn opens_outside(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "mailto" | "tel")
}

/// Same scheme, host and port. Not `Url::origin`: the libraries screen's
/// `tauri://localhost` has an opaque origin, which equals nothing, not
/// even itself.
fn same_place(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    const APP: &str = "http://127.0.0.1:61234/";

    fn nav(next: &str) -> Destination {
        navigation(
            &url(next),
            Some(&url(APP)),
            &url("tauri://localhost/index.html"),
        )
    }

    fn window(next: &str) -> Destination {
        new_window(
            &url(next),
            Some(&url(APP)),
            &url("tauri://localhost/index.html"),
        )
    }

    /// The app moves within its own server and to the libraries screen,
    /// and a document's frame starts as `about:srcdoc`.
    #[test]
    fn the_app_and_its_frames_stay_in_the_window() {
        for next in [
            "http://127.0.0.1:61234/",
            "http://127.0.0.1:61234/chat/00000000-0000-8000-a000-000000000001",
            "tauri://localhost/index.html",
            "about:srcdoc",
            "about:blank",
        ] {
            assert_eq!(nav(next), Destination::App, "{next}");
        }
    }

    #[test]
    fn the_web_mail_and_phone_go_to_the_os() {
        for next in [
            "https://enterprise.example/",
            "http://127.0.0.1:61235/",
            "http://localhost:61234/",
            "mailto:picard@enterprise.example",
            "tel:+15550123456",
        ] {
            assert_eq!(nav(next), Destination::Os, "{next}");
            assert_eq!(window(next), Destination::Os, "{next}");
        }
    }

    /// Every other scheme a page could name, a file dropped on the
    /// window among them, goes nowhere: it was followed in place or
    /// opened as a window of the app before.
    #[test]
    fn every_other_scheme_goes_nowhere() {
        for next in [
            "file:///etc/passwd",
            "datalib:person/%25%25%25",
            "slack://user?team=T01&id=U02",
            "sms:+15550123456",
            "javascript:alert(1)",
            "data:text/html,<p>hi</p>",
            "blob:http://127.0.0.1:61234/5f0c",
            "ftp://enterprise.example/",
            "x-apple-systempreferences:",
            "about:config",
            "tauri://elsewhere/index.html",
        ] {
            assert_eq!(nav(next), Destination::Nowhere, "{next}");
            assert_eq!(window(next), Destination::Nowhere, "{next}");
        }
    }

    /// A new window is a card window only for a page of the app; a
    /// blank one has nothing to show.
    #[test]
    fn a_new_window_opens_only_the_app() {
        assert_eq!(window("http://127.0.0.1:61234/x"), Destination::App);
        assert_eq!(window("tauri://localhost/index.html"), Destination::App);
        assert_eq!(window("about:blank"), Destination::Nowhere);
        assert_eq!(window("about:srcdoc"), Destination::Nowhere);
    }

    /// On the libraries screen no library is open, so no server's page
    /// is the app's.
    #[test]
    fn with_no_library_open_only_the_launcher_is_the_app() {
        let launcher = url("http://tauri.localhost/index.html");
        assert_eq!(
            navigation(&url("http://tauri.localhost/index.html"), None, &launcher),
            Destination::App
        );
        assert_eq!(
            navigation(&url(APP), None, &launcher),
            Destination::Os,
            "a server no library opened is a website like any other"
        );
    }
}
