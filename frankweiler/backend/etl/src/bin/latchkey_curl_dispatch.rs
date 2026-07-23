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
//!   * If the request carries the value-less marker header
//!     `-H "X-Imbue-Impersonate:"`, the marker is stripped and the
//!     remaining args are handed to the Chrome-impersonating curl
//!     (`latchkey-curl-impersonate`), found next to this binary
//!     (installers ship the two side by side).
//!   * Otherwise the args are passed through verbatim to the system
//!     curl: `curl` on `$PATH` (skipping this binary, so a
//!     `LATCHKEY_CURL`-on-PATH setup can't recurse).
//!
//! Why a header-*removal* marker: `-H "Name:"` with an empty right-hand
//! side is curl's syntax for removing an internal header named `Name`.
//! `X-Imbue-Impersonate` is a header curl never emits, so a real curl
//! that ever sees the marker (e.g. if it leaks past this dispatcher)
//! removes a header that was never there — a genuine no-op on the wire.
//! The signature is therefore invisible to a real curl; only this
//! dispatcher gives it meaning.
//!
//! Unix only (macOS + Linux): it `exec`s the chosen binary, replacing
//! the process so exit status, signals, and stdio pass through
//! unchanged.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The private routing marker, matched as the exact value of a `-H`
/// header argument. Value-less so on a real curl it's a no-op header
/// removal (curl never emits `X-Imbue-Impersonate`); namespaced so it
/// can't collide with a header a caller legitimately wants to strip.
///
/// Every caller — datalib's `latchkey curl` invocations and the minds
/// latchkey gateway — emits it in exactly this two-token form
/// (`-H` `X-Imbue-Impersonate:`), so the parser matches that spelling
/// literally rather than reimplementing curl's header-argument grammar.
const MARKER_HEADER_ARG: &str = "X-Imbue-Impersonate:";

/// Filenames to look for next to `current_exe()` — mirrors
/// `SIBLING_NAMES` in `latchkey.rs`. Installers ship the impersonator and
/// this dispatcher side by side in the same dir, so a sibling lookup
/// resolves it without any configuration.
const IMPERSONATE_SIBLING_NAMES: &[&str] = &[
    "latchkey-curl-impersonate",
    "latchkey_curl_impersonate",
];

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-curl-dispatch: {}", msg.as_ref());
    std::process::exit(2);
}

/// Scan argv (already sans program name) for the marker, recognized only
/// as the exact two-token header argument `-H X-Imbue-Impersonate:` (or
/// the `--header` long form). Returns argv with every marker occurrence
/// removed and whether at least one was found. This is deliberately
/// strict: callers emit exactly this spelling, so we don't reimplement
/// curl's `-HVALUE` / `--header=VALUE` / combined-bundle grammar.
fn strip_marker(argv: Vec<String>) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::with_capacity(argv.len());
    let mut found = false;
    let mut it = argv.into_iter();
    while let Some(tok) = it.next() {
        if tok == "-H" || tok == "--header" {
            match it.next() {
                Some(val) if val == MARKER_HEADER_ARG => found = true,
                Some(val) => {
                    out.push(tok);
                    out.push(val);
                }
                // Dangling flag with no value: leave it for the target
                // binary to reject rather than silently swallowing it.
                None => out.push(tok),
            }
        } else {
            out.push(tok);
        }
    }
    (out, found)
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

    let (forwarded, impersonate) = strip_marker(argv);

    let target = if impersonate {
        resolve_impersonator()
    } else {
        resolve_real_curl(self_exe.as_deref())
    };

    // `exec` replaces this process on success and only returns on error.
    let err = Command::new(&target).args(&forwarded).exec();
    die(format!("failed to exec {}: {err}", target.display()));
}
