// Standalone curl-dispatch binary. Like the shim it writes directly to
// stderr and is exempt from the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `latchkey-curl-dispatch` — a drop-in `curl` that routes each
//! invocation to one of two real implementations based on a private
//! signature in the arguments. It exists so a single `LATCHKEY_CURL`
//! binary can serve both impersonating and non-impersonating callers
//! without breaking the latter: only callers that opt in get the
//! Chrome-impersonating shim; everyone else keeps getting the system
//! curl they expect.
//!
//! Routing:
//!   * If the request carries the value-less marker header
//!     `-H "X-Imbue-Impersonate:"`, the marker is stripped and the
//!     remaining args are handed to the Chrome-impersonating shim
//!     (`latchkey-curl-shim`), resolved from
//!     `$FRANKWEILER_IMPERSONATE_CURL`, else a sibling-of-self lookup.
//!   * Otherwise the args are passed through verbatim to the system
//!     curl, resolved from `$FRANKWEILER_REAL_CURL`, else `curl` on
//!     `$PATH` (skipping this binary), else `/usr/bin/curl`.
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

/// The private routing marker. Matched case-insensitively on the header
/// *name*; the value is ignored (we emit it value-less). Namespaced so
/// it can't collide with a header a caller legitimately wants to strip.
const MARKER_HEADER: &str = "x-imbue-impersonate";

/// Filenames to look for next to `current_exe()` when
/// `$FRANKWEILER_IMPERSONATE_CURL` is unset — mirrors `SIBLING_NAMES` in
/// `latchkey.rs` so an installed release (both binaries side by side in
/// the same dir) resolves the shim without any env var.
const SHIM_SIBLING_NAMES: &[&str] = &[
    "frankweiler-latchkey-curl-shim",
    "latchkey-curl-shim",
    "latchkey_curl_shim",
];

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-curl-dispatch: {}", msg.as_ref());
    std::process::exit(2);
}

/// Extract the header name from an `-H` value: everything before the
/// first `:` or `;` (curl's two header-value separators), trimmed.
fn header_name(value: &str) -> &str {
    let end = value
        .find(|c| c == ':' || c == ';')
        .unwrap_or(value.len());
    value[..end].trim()
}

fn is_marker(value: &str) -> bool {
    header_name(value).eq_ignore_ascii_case(MARKER_HEADER)
}

/// Scan argv (already sans program name) for the marker header. Returns
/// the argv with every marker occurrence removed and whether at least
/// one was found. Recognizes the marker only as a standalone header
/// argument: `-H VALUE`, `--header VALUE`, `-HVALUE`, `--header=VALUE`.
/// (Callers always emit it that way; a marker buried inside a combined
/// short bundle like `-sSHVALUE` is intentionally not special-cased.)
fn strip_marker(argv: Vec<String>) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::with_capacity(argv.len());
    let mut found = false;
    let mut it = argv.into_iter();
    while let Some(tok) = it.next() {
        if tok == "-H" || tok == "--header" {
            match it.next() {
                Some(val) if is_marker(&val) => found = true,
                Some(val) => {
                    out.push(tok);
                    out.push(val);
                }
                // Dangling flag with no value: leave it for the target
                // binary to reject rather than silently swallowing it.
                None => out.push(tok),
            }
        } else if let Some(val) = tok.strip_prefix("--header=") {
            if is_marker(val) {
                found = true;
            } else {
                out.push(tok);
            }
        } else if tok.len() > 2 && tok.starts_with("-H") && !tok.starts_with("--") {
            if is_marker(&tok[2..]) {
                found = true;
            } else {
                out.push(tok);
            }
        } else {
            out.push(tok);
        }
    }
    (out, found)
}

fn env_existing(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    let p = PathBuf::from(v);
    p.exists().then_some(p)
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
    if let Some(p) = env_existing("FRANKWEILER_IMPERSONATE_CURL") {
        return p;
    }
    if let Some(p) = sibling_of_exe(SHIM_SIBLING_NAMES) {
        return p;
    }
    die(
        "impersonation requested but no impersonator curl found; set \
         $FRANKWEILER_IMPERSONATE_CURL to the latchkey-curl-shim path",
    );
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
    if let Some(p) = env_existing("FRANKWEILER_REAL_CURL") {
        return p;
    }
    if let Some(p) = curl_on_path(self_exe) {
        return p;
    }
    PathBuf::from("/usr/bin/curl")
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
