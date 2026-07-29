// Standalone curl-dispatch binary. Like the impersonator it writes
// directly to stderr and is exempt from the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `latchkey-curl-dispatch` — a drop-in `curl` that routes each
//! invocation to one of two real implementations based on a private
//! signature in the arguments. It exists so a single `LATCHKEY_CURL`
//! binary can serve both impersonating and non-impersonating callers
//! without breaking the latter: only callers that opt in get the
//! Chrome-impersonating curl; everyone else keeps getting the system
//! curl they expect.
//!
//! Routing:
//!   * If the request carries the marker header `X-Imbue-Impersonate`,
//!     the args are handed to the Chrome-impersonating curl
//!     (`latchkey-curl-impersonate`), found next to this binary
//!     (installers ship the two side by side).
//!   * Otherwise they go to the system curl: `curl` on `$PATH` (skipping
//!     this binary, so a `LATCHKEY_CURL`-on-PATH setup can't recurse).
//!
//! Either way the args are passed on verbatim — this binary only reads
//! them. Removing the marker so it never reaches the wire is the
//! impersonator's job (see `SUPPRESSED_HEADERS` there), which has to
//! handle it regardless because it can be pointed at by `LATCHKEY_CURL`
//! directly, with no dispatcher in front of it.
//!
//! The marker is matched by header *name*, with any value, because it
//! reaches us two different ways. Called directly, latchkey passes on
//! the value-less `-H "X-Imbue-Impersonate:"` its caller wrote. Called
//! by the latchkey *gateway* — how minds workspaces reach third-party
//! services — the request first crossed an HTTP hop, so it can only
//! have arrived with a value (a value-less header has no representation
//! on the wire; see `IMPERSONATE_MARKER_HEADER` in `../../http.rs`), and
//! the gateway rebuilds it as `-H "X-Imbue-Impersonate: 1"` in the
//! invocation it hands us. Matching on the name covers both without the
//! two sides having to agree on a spelling.
//!
//! Unix only (macOS + Linux): it `exec`s the chosen binary, replacing
//! the process so exit status, signals, and stdio pass through
//! unchanged.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Name of the private routing marker header. Namespaced so it can't
/// collide with a header a caller legitimately wants to set or strip.
const MARKER_HEADER_NAME: &str = "X-Imbue-Impersonate";

/// Filenames to look for next to `current_exe()` — mirrors
/// `SIBLING_NAMES` in `latchkey.rs`. Installers ship the impersonator and
/// this dispatcher side by side in the same dir, so a sibling lookup
/// resolves it without any configuration.
const IMPERSONATE_SIBLING_NAMES: &[&str] =
    &["latchkey-curl-impersonate", "latchkey_curl_impersonate"];

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-curl-dispatch: {}", msg.as_ref());
    std::process::exit(2);
}

/// Whether a `-H` / `--header` argument names the impersonation marker.
///
/// Only the header name is compared, so every value the marker can
/// arrive with counts: none at all (`X-Imbue-Impersonate:`, what callers
/// write), a value (`X-Imbue-Impersonate: 1`, the only form that
/// survives an HTTP hop through the latchkey gateway), and curl's
/// send-empty spelling (`X-Imbue-Impersonate;`). Header names are
/// case-insensitive in HTTP and the gateway echoes back whatever case
/// its client sent, so we compare that way too.
fn is_marker(header_argument: &str) -> bool {
    match header_argument.find([':', ';']) {
        Some(index) => header_argument[..index]
            .trim()
            .eq_ignore_ascii_case(MARKER_HEADER_NAME),
        // No separator: not a header argument curl would accept, so not
        // a marker either.
        None => false,
    }
}

/// Whether argv (already sans program name) carries the marker, as the
/// value of a two-token `-H` / `--header` argument.
///
/// Two tokens is the only form we need to handle: it is what latchkey's
/// gateway emits when it rebuilds a curl invocation from an inbound
/// request, and what `http::latchkey_curl` emits directly. Curl's glued
/// spellings (`-HVALUE`, `--header=VALUE`) are not recognized — nothing
/// that reaches us produces them, and missing one costs impersonation,
/// never correctness.
fn has_marker(argv: &[String]) -> bool {
    let mut it = argv.iter();
    while let Some(tok) = it.next() {
        // Consume the value along with the flag, so a header value that
        // happens to look like a flag is never read as one.
        if tok == "-H" || tok == "--header" {
            if it.next().is_some_and(|value| is_marker(value)) {
                return true;
            }
        }
    }
    false
}

/// Look for one of `names` next to `current_exe()`, following the exe
/// symlink so we look in the real install dir.
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

/// Find `curl` on `$PATH`, skipping any candidate that resolves to this
/// dispatcher itself (so a `LATCHKEY_CURL`-on-PATH setup can't recurse).
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
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let self_exe = std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    let target = if has_marker(&argv) {
        resolve_impersonator()
    } else {
        resolve_real_curl(self_exe.as_deref())
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
                assert!(has_marker(&tokens), "not recognized: {flag} {marker:?}");
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
        assert!(has_marker(&tokens));
    }

    #[test]
    fn leaves_unmarked_invocations_to_the_system_curl() {
        let tokens = argv(&["-sS", "-H", "Accept: */*", "https://example.com/"]);
        assert!(!has_marker(&tokens));
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
            assert!(!has_marker(&tokens), "unexpectedly matched {header:?}");
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
        assert!(!has_marker(&tokens));
    }

    /// A dangling `-H` with no value is not a marker (and is left for the
    /// target binary to reject).
    #[test]
    fn tolerates_dangling_header_flag() {
        let tokens = argv(&["https://example.com/", "-H"]);
        assert!(!has_marker(&tokens));
    }
}
