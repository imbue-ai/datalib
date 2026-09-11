// Standalone curl-dispatch binary. Like the impersonator it writes
// directly to stderr and is exempt from the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `latchkey-curl-dispatch` — a drop-in `curl` that routes each
//! invocation to one of two real implementations, and optionally to a
//! different destination, based on private signatures in the arguments.
//! It exists so a single `LATCHKEY_CURL` binary can serve impersonating
//! and non-impersonating callers alike without breaking the latter: only
//! callers that opt in get the Chrome-impersonating curl; everyone else
//! keeps getting the system curl they expect.
//!
//! The impersonating curl is upstream `curl-impersonate` (shipped next to
//! this binary as `latchkey-curl-impersonate`), a real curl whose
//! `--impersonate` flag selects a browser profile. This binary is what
//! turns that flag on, so an impersonating invocation is rewritten, not
//! forwarded verbatim; see [`impersonate_args`].

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Name of the private routing marker header. Namespaced so it can't
/// collide with a header a caller legitimately wants to set or strip.
const MARKER_HEADER_NAME: &str = "X-Imbue-Impersonate";

/// Name of the private marker header asking for the request to leave from
/// the user's own computer rather than from this machine. minds publishes
/// this name to remote workspaces as `MINDS_DESKTOP_PROXY_HEADER`, which
/// `http.rs` attaches as-is; this is the one place the name is spelled
/// out. Matched by name like [`MARKER_HEADER_NAME`], for the same reason.
const DESKTOP_PROXY_MARKER_HEADER_NAME: &str = "X-Imbue-Desktop-Proxy";

/// Env var holding the base URL of the latchkey gateway on the user's
/// computer, as reachable from this machine (in minds, a reverse tunnel
/// into the VPS loopback). It is the variable minds already gives the
/// VPS gateway for its own desktop-forwarding extension; the gateway
/// runs us as a child, so we inherit it rather than needing one of our
/// own. Required whenever a request carries the desktop-proxy marker; a
/// marked request with no gateway to send it to is an error, not a
/// silent direct request, since the caller asked for a different source
/// address on purpose.
const DESKTOP_PROXY_GATEWAY_URL_ENV: &str = "LATCHKEY_EXTENSION_DESKTOP_GATEWAY_URL";

/// Env var holding the password to send as [`GATEWAY_PASSWORD_HEADER_NAME`].
/// This is the password the gateway running us listens with: in minds,
/// the desktop and every VPS gateway share one password (derived on the
/// desktop and handed to each machine at provisioning), which is also
/// what lets the VPS's forwarding extension pass a caller's password
/// through to the desktop unchanged. Optional: an empty or unset value
/// sends no password header, for a gateway that requires none.
const DESKTOP_PROXY_GATEWAY_PASSWORD_ENV: &str = "LATCHKEY_GATEWAY_LISTEN_PASSWORD";

/// The latchkey gateway's outbound-proxy endpoint: `<gateway>/gateway/<target-url>`.
const GATEWAY_PATH_PREFIX: &str = "/gateway/";

/// The header a latchkey gateway reads its shared password from.
const GATEWAY_PASSWORD_HEADER_NAME: &str = "X-Latchkey-Gateway-Password";

/// The header that asks a latchkey gateway to forward a `/gateway/<url>`
/// request exactly as received — no credential injection, no permission
/// check — because the credentials are already in it, injected by the
/// gateway that handed the request to us.
const GATEWAY_NO_CREDENTIALS_HEADER: &str = "X-Latchkey-Gateway-No-Credentials: 1";

/// Filenames to look for next to `current_exe()` — mirrors
/// `SIBLING_NAMES` in `latchkey.rs`. Installers ship the impersonator and
/// this dispatcher side by side in the same dir, so a sibling lookup
/// resolves it without any configuration.
const IMPERSONATE_SIBLING_NAMES: &[&str] =
    &["latchkey-curl-impersonate", "latchkey_curl_impersonate"];

/// Env var naming the `--impersonate` target. Passed through as-is:
/// curl-impersonate rejects a name it does not know (exit 43, naming a
/// valid one), which is the loud failure we want rather than a fallback
/// to some other fingerprint.
const PROFILE_ENV: &str = "DATALIB_IMPERSONATE_PROFILE";

/// The profile used when [`PROFILE_ENV`] is unset. Its JA4 and HTTP/2
/// fingerprints matched a real Chromium 152 when it was chosen
/// (2026-09-11, `tls.browserleaks.com`); bump it as Chrome moves.
const DEFAULT_PROFILE: &str = "chrome150";

/// Headers the caller may not set on an impersonated request: the marker
/// is ours and must not reach the wire, and a `User-Agent` would override
/// the one the profile sends (curl lets `-H` replace any header). The
/// latchkey gateway forwards its own client's `User-Agent: curl/...`, so
/// the second is the normal case, not a corner.
const IMPERSONATE_STRIPPED_HEADERS: &[&str] = &[MARKER_HEADER_NAME, "User-Agent"];

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-curl-dispatch: {}", msg.as_ref());
    std::process::exit(2);
}

fn is_header_named(header_argument: &str, name: &str) -> bool {
    match header_argument.find([':', ';']) {
        Some(index) => header_argument[..index].trim().eq_ignore_ascii_case(name),
        // No separator: not a header argument curl would accept, so not
        // a marker either.
        None => false,
    }
}

fn is_header_flag(token: &str) -> bool {
    token == "-H" || token == "--header"
}

fn has_header(argv: &[String], name: &str) -> bool {
    let mut it = argv.iter();
    while let Some(tok) = it.next() {
        // Consume the value along with the flag, so a header value that
        // happens to look like a flag is never read as one. `&&`
        // short-circuits, so `it.next()` still runs only when the flag
        // matched — the advance is identical to the nested-`if` form
        // clippy asked us to collapse.
        if is_header_flag(tok) && it.next().is_some_and(|v| is_header_named(v, name)) {
            return true;
        }
    }
    false
}

/// Where an invocation goes; see the module docs for the order.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Rewritten onto the desktop latchkey gateway and run by the system
    /// curl.
    DesktopProxy,
    /// Handed verbatim to the Chrome-impersonating curl.
    Impersonate,
    /// Handed verbatim to the system curl.
    SystemCurl,
}

fn choose_route(argv: &[String]) -> Route {
    // The desktop-proxy marker is checked first: an invocation carrying
    // both markers must keep its impersonation marker for the desktop
    // gateway's curl, which the impersonator here would strip.
    if has_header(argv, DESKTOP_PROXY_MARKER_HEADER_NAME) {
        Route::DesktopProxy
    } else if has_header(argv, MARKER_HEADER_NAME) {
        Route::Impersonate
    } else {
        Route::SystemCurl
    }
}

/// The desktop latchkey gateway a marked request is sent to, read from
/// the environment inherited from the gateway that runs us.
struct DesktopGateway {
    /// Base URL without a trailing slash, so the endpoint path can be
    /// appended directly.
    base_url: String,
    password: Option<String>,
}

impl DesktopGateway {
    fn from_env() -> Result<Self, String> {
        let base_url = match std::env::var(DESKTOP_PROXY_GATEWAY_URL_ENV) {
            Ok(value) if !value.is_empty() => value,
            _ => {
                return Err(format!(
                    "desktop proxy requested ({DESKTOP_PROXY_MARKER_HEADER_NAME} header) but \
                     {DESKTOP_PROXY_GATEWAY_URL_ENV} is not set"
                ))
            }
        };
        let password = std::env::var(DESKTOP_PROXY_GATEWAY_PASSWORD_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            password,
        })
    }
}

/// Rewrite a marked invocation so it goes to the desktop gateway's
/// outbound proxy instead of straight to the third party.
fn rewrite_for_desktop_proxy(
    argv: &[String],
    gateway: &DesktopGateway,
) -> Result<Vec<String>, String> {
    let Some(target_url) = argv.last() else {
        return Err("desktop proxy requested but the invocation has no arguments".to_string());
    };
    if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
        return Err(format!(
            "desktop proxy requested but the last argument is not an absolute http(s) URL: {target_url:?}"
        ));
    }

    let mut rewritten = Vec::with_capacity(argv.len() + 4);
    rewritten.push("-H".to_string());
    rewritten.push(GATEWAY_NO_CREDENTIALS_HEADER.to_string());
    if let Some(password) = &gateway.password {
        rewritten.push("-H".to_string());
        rewritten.push(format!("{GATEWAY_PASSWORD_HEADER_NAME}: {password}"));
    }

    let body = &argv[..argv.len() - 1];
    let mut it = body.iter().peekable();
    while let Some(tok) = it.next() {
        if is_header_flag(tok) {
            // A header's value belongs to it: copy or drop the pair as a
            // unit, so a value that looks like a flag is never re-read.
            match it.next() {
                Some(value) if is_header_named(value, DESKTOP_PROXY_MARKER_HEADER_NAME) => continue,
                Some(value) => {
                    rewritten.push(tok.clone());
                    rewritten.push(value.clone());
                }
                None => rewritten.push(tok.clone()),
            }
        } else {
            rewritten.push(tok.clone());
        }
    }

    rewritten.push(format!(
        "{}{GATEWAY_PATH_PREFIX}{target_url}",
        gateway.base_url
    ));
    Ok(rewritten)
}

fn sibling_of_exe(names: &[&str]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?;
    names
        .iter()
        .map(|n| dir.join(n))
        .find(|candidate| candidate.is_file())
}

fn resolve_impersonator() -> PathBuf {
    sibling_of_exe(IMPERSONATE_SIBLING_NAMES).unwrap_or_else(|| {
        die(
            "impersonation requested but no impersonator curl found next to \
             this binary (expected a latchkey-curl-impersonate sibling)",
        )
    })
}

fn is_stripped_for_impersonation(header_argument: &str) -> bool {
    IMPERSONATE_STRIPPED_HEADERS
        .iter()
        .any(|name| is_header_named(header_argument, name))
}

/// The argv handed to curl-impersonate for a marked invocation.
///
/// Three flags go in front. `--impersonate <profile>` is the whole point.
/// `--compressed` is required, not cosmetic: the profile advertises
/// `Accept-Encoding: gzip, deflate, br, zstd` as part of looking like
/// Chrome, and without this flag curl hands the caller the still-encoded
/// body. `--noproxy '*'` keeps `HTTP(S)_PROXY` and the macOS proxy pane
/// out of a request that carries the user's credentials — deliberately
/// stricter than plain curl, which honors them.
fn impersonate_args(argv: &[String], profile: &str) -> Vec<String> {
    let mut rewritten = Vec::with_capacity(argv.len() + 5);
    rewritten.extend(
        ["--compressed", "--noproxy", "*", "--impersonate", profile]
            .into_iter()
            .map(str::to_string),
    );
    let mut it = argv.iter();
    while let Some(tok) = it.next() {
        if is_header_flag(tok) {
            // A header's value belongs to it: keep or drop the pair as a
            // unit, so a value that looks like a flag is never re-read.
            match it.next() {
                Some(value) if is_stripped_for_impersonation(value) => continue,
                Some(value) => {
                    rewritten.push(tok.clone());
                    rewritten.push(value.clone());
                }
                None => rewritten.push(tok.clone()),
            }
        } else {
            rewritten.push(tok.clone());
        }
    }
    rewritten
}

fn impersonation_profile() -> String {
    match std::env::var(PROFILE_ENV) {
        Ok(value) if !value.is_empty() => value,
        _ => DEFAULT_PROFILE.to_string(),
    }
}

fn curl_on_path(self_exe: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("curl");
        if !candidate.is_file() {
            continue;
        }
        let canonical = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if self_exe == Some(canonical.as_path()) {
            continue;
        }
        return Some(candidate);
    }
    None
}

fn resolve_real_curl(self_exe: Option<&Path>) -> PathBuf {
    curl_on_path(self_exe).unwrap_or_else(|| die("no system curl found on $PATH"))
}

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let self_exe = std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    let target = match choose_route(&argv) {
        Route::DesktopProxy => {
            let gateway = DesktopGateway::from_env().unwrap_or_else(|message| die(message));
            argv =
                rewrite_for_desktop_proxy(&argv, &gateway).unwrap_or_else(|message| die(message));
            resolve_real_curl(self_exe.as_deref())
        }
        Route::Impersonate => {
            argv = impersonate_args(&argv, &impersonation_profile());
            resolve_impersonator()
        }
        Route::SystemCurl => resolve_real_curl(self_exe.as_deref()),
    };

    // `exec` replaces this process on success and only returns on error.
    let err = Command::new(&target).args(&argv).exec();
    die(format!("failed to exec {}: {err}", target.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    /// Every value the marker can arrive with is recognized, under either
    /// spelling of the header flag.
    #[test]
    fn detects_marker_whatever_its_value() {
        for marker in [
            "X-Imbue-Impersonate:",
            "X-Imbue-Impersonate: 1",
            "X-Imbue-Impersonate;",
            "x-imbue-impersonate: 1",
            "X-IMBUE-IMPERSONATE:",
        ] {
            for flag in ["-H", "--header"] {
                let tokens = argv(&[
                    "-sS",
                    "-H",
                    "Accept: */*",
                    flag,
                    marker,
                    "https://example.com/",
                ]);
                assert!(
                    has_header(&tokens, MARKER_HEADER_NAME),
                    "not recognized: {flag} {marker:?}"
                );
            }
        }
    }

    /// The shape the latchkey gateway rebuilds an inbound request into
    /// (`gatewayEndpoint.ts`'s `buildCurlArguments`, plus the `-sS -D`
    /// it prepends) routes to the impersonator.
    #[test]
    fn detects_marker_in_the_gateway_reconstructed_invocation() {
        let tokens = argv(&[
            "-sS",
            "-D",
            "/tmp/headers",
            "-X",
            "POST",
            "-H",
            "User-Agent: curl/8.7.1",
            "-H",
            "Accept: */*",
            "-H",
            "X-Imbue-Impersonate: 1",
            "--data-binary",
            "@-",
            "https://claude.ai/api/organizations",
        ]);
        assert!(has_header(&tokens, MARKER_HEADER_NAME));
    }

    #[test]
    fn leaves_unmarked_invocations_to_the_system_curl() {
        let tokens = argv(&["-sS", "-H", "Accept: */*", "https://example.com/"]);
        assert!(!has_header(&tokens, MARKER_HEADER_NAME));
    }

    /// A header named something else is not the marker, and neither is a
    /// bare name with no `:` / `;` separator.
    #[test]
    fn does_not_match_other_headers() {
        for header in [
            "X-Imbue-Impersonation:",
            "Authorization: Bearer x",
            "X-Imbue-Impersonate",
        ] {
            let tokens = argv(&["-H", header, "https://example.com/"]);
            assert!(
                !has_header(&tokens, MARKER_HEADER_NAME),
                "unexpectedly matched {header:?}"
            );
        }
    }

    /// A value belongs to the flag before it: something that merely looks
    /// like a marker header argument is not read as one.
    #[test]
    fn does_not_match_inside_a_header_value() {
        let tokens = argv(&[
            "-H",
            "X-Echo: -H",
            "X-Imbue-Impersonate:",
            "https://example.com/",
        ]);
        assert!(!has_header(&tokens, MARKER_HEADER_NAME));
    }

    /// A dangling `-H` with no value is not a marker (and is left for the
    /// target binary to reject).
    #[test]
    fn tolerates_dangling_header_flag() {
        let tokens = argv(&["https://example.com/", "-H"]);
        assert!(!has_header(&tokens, MARKER_HEADER_NAME));
    }

    fn test_gateway(password: Option<&str>) -> DesktopGateway {
        DesktopGateway {
            base_url: "http://127.0.0.1:1988".to_string(),
            password: password.map(str::to_string),
        }
    }

    fn marked_gateway_invocation() -> Vec<String> {
        argv(&[
            "-sS",
            "-D",
            "/tmp/headers",
            "-X",
            "POST",
            "-H",
            "User-Agent: curl/8.7.1",
            "-H",
            "X-Imbue-Impersonate: 1",
            "-H",
            "X-Imbue-Desktop-Proxy: 1",
            "-H",
            "Authorization: Bearer injected-on-the-vps",
            "--data-binary",
            "@-",
            "https://slack.com/api/conversations.history?channel=C1&limit=100",
        ])
    }

    /// The desktop-proxy marker wins over the impersonation marker, so
    /// the latter reaches the desktop gateway's own curl intact.
    #[test]
    fn desktop_proxy_marker_decides_the_route_before_impersonation() {
        assert_eq!(
            choose_route(&marked_gateway_invocation()),
            Route::DesktopProxy
        );
        assert_eq!(
            choose_route(&argv(&[
                "-H",
                "X-Imbue-Impersonate: 1",
                "https://example.com/"
            ])),
            Route::Impersonate
        );
        assert_eq!(
            choose_route(&argv(&["-H", "Accept: */*", "https://example.com/"])),
            Route::SystemCurl
        );
    }

    #[test]
    fn desktop_proxy_marker_is_matched_by_name_whatever_its_value() {
        for marker in [
            "X-Imbue-Desktop-Proxy: 1",
            "x-imbue-desktop-proxy: 1",
            "X-Imbue-Desktop-Proxy:",
            "X-Imbue-Desktop-Proxy;",
        ] {
            let tokens = argv(&["-H", marker, "https://example.com/"]);
            assert_eq!(
                choose_route(&tokens),
                Route::DesktopProxy,
                "not recognized: {marker:?}"
            );
        }
        // A value that merely looks like the marker is not one.
        let tokens = argv(&[
            "-H",
            "X-Echo: -H",
            "X-Imbue-Desktop-Proxy: 1",
            "https://example.com/",
        ]);
        assert_eq!(choose_route(&tokens), Route::SystemCurl);
    }

    #[test]
    fn rewrites_a_marked_invocation_onto_the_desktop_gateway() {
        let rewritten =
            rewrite_for_desktop_proxy(&marked_gateway_invocation(), &test_gateway(Some("hunter2")))
                .expect("rewrite succeeds");
        assert_eq!(
            rewritten,
            argv(&[
                "-H",
                "X-Latchkey-Gateway-No-Credentials: 1",
                "-H",
                "X-Latchkey-Gateway-Password: hunter2",
                "-sS",
                "-D",
                "/tmp/headers",
                "-X",
                "POST",
                "-H",
                "User-Agent: curl/8.7.1",
                "-H",
                "X-Imbue-Impersonate: 1",
                "-H",
                "Authorization: Bearer injected-on-the-vps",
                "--data-binary",
                "@-",
                "http://127.0.0.1:1988/gateway/https://slack.com/api/conversations.history?channel=C1&limit=100",
            ])
        );
    }

    #[test]
    fn desktop_proxy_rewrite_sends_no_password_header_when_none_is_configured() {
        let rewritten = rewrite_for_desktop_proxy(
            &argv(&["-H", "X-Imbue-Desktop-Proxy: 1", "https://example.com/x"]),
            &test_gateway(None),
        )
        .expect("rewrite succeeds");
        assert_eq!(
            rewritten,
            argv(&[
                "-H",
                "X-Latchkey-Gateway-No-Credentials: 1",
                "http://127.0.0.1:1988/gateway/https://example.com/x",
            ])
        );
    }

    /// The gateway slices its prefix back off the raw path, so anything
    /// that re-encoded or normalized the target here would change the
    /// request the third party actually receives.
    #[test]
    fn desktop_proxy_rewrite_keeps_the_target_url_verbatim() {
        for url in [
            "https://slack.com/files/a%20b?u=x%2Fy",
            "https://a.example.com/x?q=https://b.example.com/y",
            "http://a.example.com/a/../b",
        ] {
            let rewritten = rewrite_for_desktop_proxy(
                &argv(&["-H", "X-Imbue-Desktop-Proxy: 1", url]),
                &test_gateway(None),
            )
            .expect("rewrite succeeds");
            assert_eq!(
                rewritten.last().map(String::as_str),
                Some(format!("http://127.0.0.1:1988/gateway/{url}").as_str())
            );
        }
    }

    /// A header whose value happens to look like a flag is copied as a
    /// unit, never re-read as one.
    #[test]
    fn desktop_proxy_rewrite_copies_header_pairs_as_units() {
        let rewritten = rewrite_for_desktop_proxy(
            &argv(&[
                "-H",
                "X-Echo: -H",
                "-H",
                "X-Imbue-Desktop-Proxy: 1",
                "https://example.com/x",
            ]),
            &test_gateway(None),
        )
        .expect("rewrite succeeds");
        assert_eq!(
            rewritten,
            argv(&[
                "-H",
                "X-Latchkey-Gateway-No-Credentials: 1",
                "-H",
                "X-Echo: -H",
                "http://127.0.0.1:1988/gateway/https://example.com/x",
            ])
        );
    }

    /// The shape the latchkey gateway hands us — its client's
    /// `User-Agent: curl/...` included — becomes a curl-impersonate
    /// invocation: the profile flags in front, the marker and the UA
    /// gone, everything else untouched and in order.
    #[test]
    fn impersonate_rewrite_adds_the_profile_flags_and_strips_our_headers() {
        let rewritten = impersonate_args(
            &argv(&[
                "-sS",
                "-D",
                "-",
                "-o",
                "/tmp/body",
                "-X",
                "POST",
                "-H",
                "User-Agent: curl/8.7.1",
                "-H",
                "Accept: application/json",
                "-H",
                "X-Imbue-Impersonate: 1",
                "--data-binary",
                "@-",
                "https://claude.ai/api/organizations",
            ]),
            "chrome150",
        );
        assert_eq!(
            rewritten,
            argv(&[
                "--compressed",
                "--noproxy",
                "*",
                "--impersonate",
                "chrome150",
                "-sS",
                "-D",
                "-",
                "-o",
                "/tmp/body",
                "-X",
                "POST",
                "-H",
                "Accept: application/json",
                "--data-binary",
                "@-",
                "https://claude.ai/api/organizations",
            ])
        );
    }

    /// Every spelling the marker and a User-Agent can arrive in is
    /// dropped: curl matches header names case-insensitively and accepts
    /// both the `Name: value` and the valueless `Name;` forms.
    #[test]
    fn impersonate_rewrite_strips_every_spelling_of_our_headers() {
        for header in [
            "X-Imbue-Impersonate: 1",
            "X-Imbue-Impersonate:",
            "X-Imbue-Impersonate;",
            "x-imbue-impersonate: 1",
            "User-Agent: curl/8.7.1",
            "user-agent: curl/8.7.1",
            "USER-AGENT;",
        ] {
            let rewritten =
                impersonate_args(&argv(&["-H", header, "https://example.com/"]), "chrome150");
            assert_eq!(
                rewritten,
                argv(&[
                    "--compressed",
                    "--noproxy",
                    "*",
                    "--impersonate",
                    "chrome150",
                    "https://example.com/",
                ]),
                "{header:?} survived"
            );
        }
    }

    /// Credentials and other caller headers are not ours to touch, and a
    /// value that merely looks like one of our headers is a value.
    #[test]
    fn impersonate_rewrite_keeps_every_other_header_pair() {
        let rewritten = impersonate_args(
            &argv(&[
                "-H",
                "Cookie: sessionKey=secret",
                "-H",
                "X-Echo: -H",
                "-H",
                "X-User-Agent-Hint: User-Agent: fake",
                "https://example.com/",
            ]),
            "chrome131",
        );
        assert_eq!(
            rewritten,
            argv(&[
                "--compressed",
                "--noproxy",
                "*",
                "--impersonate",
                "chrome131",
                "-H",
                "Cookie: sessionKey=secret",
                "-H",
                "X-Echo: -H",
                "-H",
                "X-User-Agent-Hint: User-Agent: fake",
                "https://example.com/",
            ])
        );
    }

    #[test]
    fn impersonate_rewrite_tolerates_a_dangling_header_flag() {
        let rewritten = impersonate_args(&argv(&["https://example.com/", "-H"]), "chrome150");
        assert_eq!(
            rewritten,
            argv(&[
                "--compressed",
                "--noproxy",
                "*",
                "--impersonate",
                "chrome150",
                "https://example.com/",
                "-H",
            ])
        );
    }

    #[test]
    fn desktop_proxy_rewrite_refuses_an_invocation_that_does_not_end_in_a_url() {
        for tokens in [
            argv(&[]),
            argv(&["-H", "X-Imbue-Desktop-Proxy: 1"]),
            argv(&["https://example.com/x", "-H", "X-Imbue-Desktop-Proxy: 1"]),
            argv(&["-H", "X-Imbue-Desktop-Proxy: 1", "ftp://example.com/x"]),
        ] {
            let result = rewrite_for_desktop_proxy(&tokens, &test_gateway(None));
            assert!(result.is_err(), "unexpectedly rewrote {tokens:?}");
        }
    }
}
