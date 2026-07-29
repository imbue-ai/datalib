// Standalone curl-shim binary spawned as a subprocess by latchkey. The
// shim writes response bodies / status to stdout/stderr because that's
// the contract callers consume. No MultiProgress / no bars in its
// process. Exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! `latchkey-curl-impersonate` — minimal curl-CLI-compatible front-end backed
//! by `wreq`'s Chrome TLS impersonation. Mirror of
//! `src/download/latchkey_curl_impersonate.py`.
//!
//! Latchkey's `LATCHKEY_CURL` env var lets us substitute our own curl.
//! Point it at this binary and Cloudflare-protected hosts (claude.ai,
//! chatgpt.com, ...) see a Chrome JA3/JA4 instead of plain curl.
//!
//! Supports just the flags latchkey + our downloaders actually emit:
//!
//! ```text
//! -X / --request          method
//! -H / --header           "Name: value" (repeatable)
//! -d / --data / --data-raw / --data-binary
//! -o / --output           write body here ("-" = stdout)
//! -D / --dump-header      write response headers here ("-" = stdout)
//! -w / --write-out        only %{http_code} is interpreted
//! -s / --silent           accepted, no-op
//! -S / --show-error       accepted, no-op
//! -L / --location         enable redirect following
//! -f / --fail             exit 22 on HTTP >= 400 (no body to -o)
//! --compressed            accepted, no-op
//! -v / --verbose          accepted, no-op
//! ```
//!
//! Combined short flags (`-sSL`, `-sSLo`) are exploded; a value-taking
//! short must be last in the bundle.
//!
//! Two request headers are dropped rather than forwarded, whatever the
//! caller passed — see [`is_suppressed_header`].

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::process::ExitCode;

use wreq::header::{HeaderMap, HeaderName, HeaderValue};
use wreq::{redirect, Client, Method};
use wreq_util::Emulation;

#[derive(Default)]
struct Args {
    method: Option<String>,
    headers: Vec<(String, String)>,
    data: Option<String>,
    out_path: Option<String>,
    dump_header_path: Option<String>,
    write_out: Option<String>,
    follow_redirects: bool,
    fail_on_http_error: bool,
    url: Option<String>,
}

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("latchkey-curl-impersonate: {}", msg.as_ref());
    std::process::exit(2);
}

/// Headers a caller may not set on an impersonated request.
///
/// * `User-Agent` — the entire point of this binary is to look like
///   Chrome, and the emulation profile supplies the matching UA. A
///   caller-supplied one silently wins over it, which is worse than not
///   impersonating at all: a Chrome TLS fingerprint announcing itself as
///   `curl/8.7.1` is a louder signal than either alone. The latchkey
///   gateway forwards its client's `User-Agent` into every curl
///   invocation it builds, so this is the normal case, not a corner one.
/// * `X-Imbue-Impersonate` — the dispatch curl's routing marker
///   (`src/bin/latchkey_curl_dispatch.rs`). That binary routes on it and
///   forwards it to us untouched, so this is the one place it is removed:
///   whether we were reached through the dispatch curl or used as
///   `LATCHKEY_CURL` directly, the private marker never reaches the wire.
const SUPPRESSED_HEADERS: &[&str] = &["User-Agent", "X-Imbue-Impersonate"];

/// Whether `name` is a header this binary refuses to let a caller set.
/// HTTP header names are case-insensitive, and the gateway echoes back
/// whatever case its client sent, so compare that way.
fn is_suppressed_header(name: &str) -> bool {
    SUPPRESSED_HEADERS
        .iter()
        .any(|suppressed| name.eq_ignore_ascii_case(suppressed))
}

fn valueless_shorts() -> HashSet<char> {
    "sSLvf".chars().collect()
}
fn value_shorts() -> HashSet<char> {
    "XHdoOwD".chars().collect()
}

fn split_combined(tok: &str) -> Vec<String> {
    if tok.len() <= 2 || !tok.starts_with('-') || tok.starts_with("--") {
        return vec![tok.to_string()];
    }
    let chars: Vec<char> = tok[1..].chars().collect();
    let vless = valueless_shorts();
    let vful = value_shorts();
    let mut out = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if vful.contains(&c) {
            if i != chars.len() - 1 {
                die(format!(
                    "combined short flag bundle {tok:?} has value-taking option {c:?} before end"
                ));
            }
            out.push(format!("-{c}"));
            return out;
        }
        if !vless.contains(&c) {
            die(format!("unsupported short flag {c:?} in bundle {tok:?}"));
        }
        out.push(format!("-{c}"));
    }
    out
}

fn parse(argv: Vec<String>) -> Args {
    // Handle --version / -V before any other parsing so it works without
    // a URL. Matches `datalib-dag --version` by
    // printing `<bin-name> <DATALIB_VERSION>` where the version is
    // the `git describe --tags --always --dirty` slug stamped at build
    // time by cargo's build.rs. Bazel intentionally does NOT stamp this
    // binary (see //datalib/backend/etl:latchkey_curl_impersonate in
    // BUILD.bazel for why) so under bazel we fall back to "unknown".
    for tok in &argv {
        if tok == "--version" || tok == "-V" {
            println!(
                "latchkey-curl-impersonate {}",
                option_env!("DATALIB_VERSION").unwrap_or("unknown")
            );
            std::process::exit(0);
        }
    }

    let mut expanded: Vec<String> = Vec::new();
    for tok in argv {
        if tok.starts_with('-') && !tok.starts_with("--") && tok.len() > 2 {
            expanded.extend(split_combined(&tok));
        } else {
            expanded.push(tok);
        }
    }

    let mut out = Args::default();
    let mut it = expanded.into_iter();
    while let Some(tok) = it.next() {
        let need = |flag: &str, it: &mut dyn Iterator<Item = String>| -> String {
            it.next()
                .unwrap_or_else(|| die(format!("{flag} requires a value")))
        };
        match tok.as_str() {
            "-X" | "--request" => out.method = Some(need(&tok, &mut it).to_uppercase()),
            "-H" | "--header" => {
                // curl spells a header argument two ways: `Name: value`,
                // and `Name;` to send one with no value (a colon with an
                // empty right-hand side means *remove* the header, so it
                // can't express that). Split on whichever separator comes
                // first so both parse — the dispatch curl matches its
                // routing marker in either spelling and forwards it here
                // for us to drop.
                let raw = need(&tok, &mut it);
                match raw.find([':', ';']) {
                    Some(index) => {
                        let name = raw[..index].trim();
                        if !is_suppressed_header(name) {
                            let value = raw[index + 1..].trim();
                            out.headers.push((name.to_string(), value.to_string()));
                        }
                    }
                    None => die(format!("malformed header {raw:?}")),
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" => {
                out.data = Some(need(&tok, &mut it));
                if out.method.is_none() {
                    out.method = Some("POST".into());
                }
            }
            "-o" | "--output" => out.out_path = Some(need(&tok, &mut it)),
            "-D" | "--dump-header" => out.dump_header_path = Some(need(&tok, &mut it)),
            "-w" | "--write-out" => out.write_out = Some(need(&tok, &mut it)),
            "-s" | "--silent" | "-S" | "--show-error" | "--compressed" | "-v" | "--verbose" => {}
            "-L" | "--location" => out.follow_redirects = true,
            "-f" | "--fail" => out.fail_on_http_error = true,
            other if other.starts_with('-') => die(format!("unsupported flag {other:?}")),
            _ => {
                if out.url.is_some() {
                    die(format!("multiple URLs: {:?}, {tok:?}", out.url));
                }
                out.url = Some(tok);
            }
        }
    }
    if out.url.is_none() {
        die("no URL provided");
    }
    out
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse(argv);

    let mut header_map = HeaderMap::new();
    for (name, value) in &args.headers {
        let n = match HeaderName::from_bytes(name.as_bytes()) {
            Ok(n) => n,
            Err(_) => die(format!("invalid header name {name:?}")),
        };
        let v = match HeaderValue::from_str(value) {
            Ok(v) => v,
            Err(_) => die(format!("invalid header value for {name}")),
        };
        header_map.append(n, v);
    }

    let client = match Client::builder()
        .emulation(Emulation::Chrome131)
        .redirect(if args.follow_redirects {
            redirect::Policy::limited(10)
        } else {
            redirect::Policy::none()
        })
        .build()
    {
        Ok(c) => c,
        Err(e) => die(format!("wreq build: {e}")),
    };

    let method = Method::from_bytes(args.method.as_deref().unwrap_or("GET").as_bytes())
        .unwrap_or(Method::GET);
    let url = args.url.as_deref().unwrap();
    let mut req = client.request(method, url).headers(header_map);
    if let Some(spec) = args.data.as_ref() {
        // curl convention for --data-binary / --data: a leading `@` means
        // "read from this source": `@-` is stdin, `@<path>` is a file.
        // Bare strings are sent verbatim. Our downloaders rely on `@-`
        // to stream JSON bodies through stdin.
        let body_bytes: Vec<u8> = if let Some(rest) = spec.strip_prefix('@') {
            if rest == "-" {
                let mut buf = Vec::new();
                if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                    die(format!("read stdin for --data: {e}"));
                }
                buf
            } else {
                match std::fs::read(rest) {
                    Ok(b) => b,
                    Err(e) => die(format!("read {rest}: {e}")),
                }
            }
        } else {
            spec.clone().into_bytes()
        };
        req = req.body(body_bytes);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("latchkey-curl-impersonate: transport error: {e}");
            return ExitCode::from(7);
        }
    };

    let status = resp.status();
    let resp_headers = resp.headers().clone();

    // curl -f / --fail: suppress body, return 22 on HTTP >= 400. We still
    // dump headers via -D so callers can inspect what happened, matching
    // real curl's behavior with -f -D.
    if args.fail_on_http_error && status.as_u16() >= 400 {
        if let Some(ref path) = args.dump_header_path {
            let mut buf = String::new();
            let reason = status.canonical_reason().unwrap_or("");
            buf.push_str(&format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason));
            for (n, v) in resp_headers.iter() {
                let val = v.to_str().unwrap_or("");
                buf.push_str(&format!("{}: {}\r\n", n.as_str(), val));
            }
            buf.push_str("\r\n");
            if path == "-" {
                let _ = std::io::stdout().write_all(buf.as_bytes());
            } else if let Ok(mut f) = File::create(path) {
                let _ = f.write_all(buf.as_bytes());
            }
        }
        eprintln!(
            "latchkey-curl-impersonate: HTTP {} for {}",
            status.as_u16(),
            url,
        );
        return ExitCode::from(22);
    }

    // -D dump headers
    if let Some(ref path) = args.dump_header_path {
        let mut buf = String::new();
        let reason = status.canonical_reason().unwrap_or("");
        buf.push_str(&format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason));
        for (n, v) in resp_headers.iter() {
            let val = v.to_str().unwrap_or("");
            buf.push_str(&format!("{}: {}\r\n", n.as_str(), val));
        }
        buf.push_str("\r\n");
        if path == "-" {
            let _ = std::io::stdout().write_all(buf.as_bytes());
        } else {
            match File::create(path) {
                Ok(mut f) => {
                    let _ = f.write_all(buf.as_bytes());
                }
                Err(e) => die(format!("open {path}: {e}")),
            }
        }
    }

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("latchkey-curl-impersonate: body read: {e}");
            return ExitCode::from(8);
        }
    };

    match args.out_path.as_deref() {
        Some("-") | None => {
            let _ = std::io::stdout().write_all(&body);
        }
        Some(path) => match File::create(path) {
            Ok(mut f) => {
                let _ = f.write_all(&body);
            }
            Err(e) => die(format!("open {path}: {e}")),
        },
    }

    if let Some(fmt) = args.write_out.as_ref() {
        let rendered = fmt.replace("%{http_code}", &status.as_u16().to_string());
        let _ = std::io::stdout().write_all(rendered.as_bytes());
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_argv(tokens: &[&str]) -> Args {
        parse(tokens.iter().map(|t| t.to_string()).collect())
    }

    /// A caller-supplied User-Agent is dropped in every casing, so the
    /// Chrome emulation profile's own UA is what goes on the wire. This
    /// is the latchkey gateway's normal behavior: it forwards its
    /// client's `User-Agent: curl/...` into the invocation it builds.
    #[test]
    fn drops_caller_supplied_user_agent() {
        for name in ["User-Agent", "user-agent", "USER-AGENT"] {
            let args = parse_argv(&["-H", &format!("{name}: curl/8.7.1"), "https://example.com/"]);
            assert!(
                args.headers.is_empty(),
                "{name} survived as {:?}",
                args.headers,
            );
        }
    }

    /// The dispatch curl's private routing marker never reaches the wire.
    /// It forwards the marker to us untouched, in whichever spelling it
    /// matched, so every one of those has to be dropped here.
    #[test]
    fn drops_impersonation_marker() {
        for marker in [
            "X-Imbue-Impersonate: 1",
            "X-Imbue-Impersonate:",
            "X-Imbue-Impersonate;",
            "x-imbue-impersonate: 1",
        ] {
            let args = parse_argv(&["-H", marker, "https://example.com/"]);
            assert!(args.headers.is_empty(), "{marker:?} survived");
        }
    }

    /// curl's `Name;` spelling sends a header with no value; a colon with
    /// an empty right-hand side cannot express that (it means *remove*).
    /// Both have to parse rather than being rejected as malformed.
    #[test]
    fn parses_valueless_header_spellings() {
        let args = parse_argv(&["-H", "X-Trace;", "https://example.com/"]);
        assert_eq!(args.headers, vec![("X-Trace".to_string(), String::new())]);
    }

    #[test]
    fn keeps_every_other_header() {
        let args = parse_argv(&[
            "-H",
            "Accept: application/json",
            "-H",
            "Authorization: Bearer token",
            "https://example.com/",
        ]);
        assert_eq!(
            args.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer token".to_string()),
            ],
        );
    }
}
