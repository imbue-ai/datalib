// Standalone credential-capture binary. Like the other two curl shims it
// writes directly to stderr and is exempt from the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `latchkey-cred-extract` — a `curl` stand-in that captures the
//! credential latchkey injected and makes **no request**.
//!
//! ## Why this exists
//!
//! latchkey's whole delivery mechanism is "spawn curl with the
//! credential already in argv". That is a perfect fit for the HTTP
//! providers in this tree and no fit at all for a transport that isn't
//! HTTP: an IMAP session needs the username and password (or the OAuth
//! access token) as *values*, to put inside a SASL exchange, and there
//! is no curl invocation in the picture to inject them into.
//!
//! `$LATCHKEY_CURL` is the seam. latchkey resolves the binary it spawns
//! from that variable (`config.js`'s `curlCommand`), so pointing it at
//! this binary makes latchkey hand us the credential arguments it would
//! otherwise have handed curl. We write them down and exit; nothing
//! dials the sentinel URL.
//!
//! This is the same seam `latchkey-curl-dispatch` uses, for a different
//! purpose — that one forwards argv to a real curl, this one terminates
//! it.
//!
//! ## Contract
//!
//! * `$DATALIB_CRED_CAPTURE` must name the file to write. Unset is a
//!   hard error (exit 2) rather than a fallthrough to a real request:
//!   this binary must never be the thing that makes a network call, and
//!   failing loudly is how we keep it that way.
//! * The file is created `0600`, truncated, and filled with argv (minus
//!   the program name) **NUL-separated**, exactly like
//!   `/proc/<pid>/cmdline`. No escaping, so no escaping bugs — a
//!   password containing quotes, backslashes, or newlines round-trips
//!   byte-for-byte.
//! * Nothing is written to stdout. `latchkey curl`'s stdout is normally
//!   the response body, and a caller may be capturing it.
//! * Nothing is written to stderr on success either — argv holds a live
//!   secret, and stderr is the one stream that tends to end up in logs.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

/// Env var naming the file to write the captured arguments to.
const CAPTURE_ENV_VAR: &str = "DATALIB_CRED_CAPTURE";

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-cred-extract: {}", msg.as_ref());
    std::process::exit(2);
}

/// Serialize argv NUL-separated. A trailing NUL after every element
/// (rather than a separator between them) makes the empty-argv case and
/// the trailing-empty-argument case unambiguous on the read side.
fn encode(argv: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in argv {
        out.extend_from_slice(arg.as_bytes());
        out.push(0);
    }
    out
}

fn main() {
    let Some(path) = std::env::var_os(CAPTURE_ENV_VAR) else {
        die(format!(
            "${CAPTURE_ENV_VAR} is not set; refusing to run (this binary \
             captures credentials and never makes requests)"
        ));
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();

    // 0600 from the moment the inode exists — never a window where the
    // secret is world-readable. `create(true).truncate(true)` rather than
    // `create_new` so a caller that reuses a path it already owns works.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => die(format!("cannot open capture file: {e}")),
    };

    if let Err(e) = file.write_all(&encode(&argv)).and_then(|()| file.flush()) {
        die(format!("cannot write capture file: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn encodes_each_argument_nul_terminated() {
        assert_eq!(
            encode(&argv(&["-u", "me:pw", "https://example.com/"])),
            b"-u\0me:pw\0https://example.com/\0".to_vec()
        );
    }

    /// The reason for NUL rather than a text format: secrets are opaque
    /// bytes and may contain any of the characters an escaping scheme
    /// would have to handle.
    #[test]
    fn round_trips_awkward_secrets_without_escaping() {
        let nasty = r#"pa"ss\wo rd'	x"#;
        let encoded = encode(&argv(&["-u", &format!("me:{nasty}")]));
        let decoded: Vec<&[u8]> = encoded
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(decoded[1], format!("me:{nasty}").as_bytes());
    }

    #[test]
    fn encodes_empty_argv_as_empty() {
        assert!(encode(&[]).is_empty());
    }

    /// A trailing empty argument still produces its own NUL, so the
    /// reader can tell `["-H", ""]` from `["-H"]`.
    #[test]
    fn keeps_a_trailing_empty_argument_distinguishable() {
        assert_eq!(encode(&argv(&["-H", ""])), b"-H\0\0".to_vec());
    }
}
