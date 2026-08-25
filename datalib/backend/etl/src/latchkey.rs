//! Single entrypoint for spawning the `latchkey` CLI.
//!
//! Every binary or test that runs `latchkey curl …` must construct its
//! `Command` via [`latchkey_command`] / [`latchkey_tokio_command`] so
//! that `LATCHKEY_CURL` is set exactly once, to the in-tree dispatch
//! curl (`src/bin/latchkey_curl_dispatch.rs`). The dispatch curl routes
//! requests carrying the `X-Imbue-Impersonate:` marker header to the
//! Chrome-impersonating curl (`src/bin/latchkey_curl_impersonate.rs`,
//! found as a sibling), and everything else to the system curl.
//! Cloudflare-protected hosts (claude.ai, chatgpt.com, files.slack.com)
//! reject vanilla curl's TLS fingerprint, so the providers that hit them
//! add the marker to their requests (see `http::latchkey_curl`).
//!
//! **Except in gateway mode** (`$LATCHKEY_GATEWAY`, how minds workspaces
//! reach third-party services), where we deliberately leave
//! `LATCHKEY_CURL` alone. There, `latchkey curl` does not talk to the
//! third party at all: it re-points the URL at the gateway, and the
//! *gateway* rebuilds the invocation it hands to its own `LATCHKEY_CURL`,
//! which is where impersonation belongs. Exporting ours would put a
//! dispatch curl on the client hop instead, and that hop would consume
//! the marker header and impersonate the connection to the gateway --
//! leaving the hop that actually reaches the third party unimpersonated.
//! Leaving it unset lets the system curl carry the marker to the gateway
//! as an ordinary header, which is why the marker has a value (see
//! `http::IMPERSONATE_MARKER_HEADER`).
//!
//! Resolution order for the dispatch-curl path (first hit wins):
//!   1. `$LATCHKEY_CURL` — caller's explicit override; trusted as-is.
//!   2. `$DATALIB_CURL_DISPATCH` — our own override (parallel to
//!      `LATCHKEY_CURL` but specifically the in-tree binary, so Bazel can
//!      inject the runfiles path without stomping a user-set
//!      `LATCHKEY_CURL`).
//!   3. Bazel runfiles lookup for `_main/datalib/backend/etl/latchkey-curl-dispatch`.
//!   4. Cargo dev fallback: walk up from CWD and the etl crate dir
//!      looking for `datalib/backend/target/{debug,release}/latchkey-curl-dispatch`
//!      or `target/{debug,release}/latchkey-curl-dispatch`.
//!   5. Sibling of `current_exe()` — installed releases drop the dispatch
//!      curl (and the impersonator next to it) beside `datalib-step` (see
//!      scripts/install.sh + .github/workflows/release.yml), so a user who
//!      only has `~/.local/bin/{datalib-step,latchkey-curl-dispatch,latchkey-curl-impersonate}`
//!      and never sets `LATCHKEY_CURL` still gets CF impersonation.
//!   6. `which latchkey-curl-dispatch` on `$PATH`.
//!
//! On miss, the `Command` is still returned but a `warn!` is logged so
//! the caller can see why CF-fronted endpoints are 403-ing.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::OnceLock;

const DISPATCH_BIN: &str = "latchkey-curl-dispatch";
/// Curl stand-in that captures latchkey's injected credential arguments
/// instead of making a request — how non-HTTP transports (IMAP) read a
/// credential out of latchkey. See [`extract_credential`].
const CRED_EXTRACT_BIN: &str = "latchkey-cred-extract";
/// Where we publish the resolved dispatch curl for the `latchkey` CLI.
const CURL_ENV_VAR: &str = "LATCHKEY_CURL";
/// Set by latchkey's callers to route every request through a gateway.
const GATEWAY_ENV_VAR: &str = "LATCHKEY_GATEWAY";
/// Caller override for the credential-capture shim, parallel to
/// `$DATALIB_CURL_DISPATCH`.
const CRED_EXTRACT_ENV_VAR: &str = "DATALIB_CRED_EXTRACT";
/// Names the file the capture shim writes its argv into. Must agree with
/// the constant of the same name in `bin/latchkey_cred_extract.rs`.
const CAPTURE_ENV_VAR: &str = "DATALIB_CRED_CAPTURE";

// Cargo emits these binaries with dashes (from the `[[bin]] name = …`
// entries in Cargo.toml); Bazel emits them with underscores (the
// `rust_binary` target name). Every lookup below tries both spellings.
fn spellings(base: &str) -> [String; 2] {
    [base.to_string(), base.replace('-', "_")]
}

static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
#[error(
    "could not locate {DISPATCH_BIN}; set $DATALIB_CURL_DISPATCH or $LATCHKEY_CURL, \
     or build it (`cargo build -p datalib-etl --bin latchkey-curl-dispatch` \
     or `bazel build //datalib/backend/etl:latchkey_curl_dispatch`)"
)]
pub struct CurlDispatchNotFound;

/// Ensure `LATCHKEY_CURL` points at the in-tree dispatch curl and return
/// its resolved path. Idempotent — the first call resolves and caches;
/// later calls are a `OnceLock` read.
pub fn ensure_curl_dispatch() -> Result<PathBuf, CurlDispatchNotFound> {
    match RESOLVED.get_or_init(resolve) {
        Some(path) => {
            if should_export_curl_dispatch(
                std::env::var_os(CURL_ENV_VAR).as_deref(),
                std::env::var_os(GATEWAY_ENV_VAR).as_deref(),
            ) {
                std::env::set_var(CURL_ENV_VAR, path);
            }
            Ok(path.clone())
        }
        None => Err(CurlDispatchNotFound),
    }
}

/// Whether latchkey is configured to route requests through a gateway.
fn is_gateway_mode(gateway: Option<&OsStr>) -> bool {
    matches!(gateway, Some(value) if !value.is_empty())
}

/// Whether [`ensure_curl_dispatch`] should point `LATCHKEY_CURL` at the
/// dispatch curl it resolved. Two reasons not to:
///
///   * the caller already set it — their override wins, and it is the
///     first thing `resolve` consults anyway;
///   * latchkey is in gateway mode, where the request reaching the third
///     party is made by the gateway's curl, not ours. See the module docs
///     for why putting a dispatch curl on the client hop actively breaks
///     impersonation rather than merely failing to help.
fn should_export_curl_dispatch(existing_curl: Option<&OsStr>, gateway: Option<&OsStr>) -> bool {
    existing_curl.is_none() && !is_gateway_mode(gateway)
}

fn resolve() -> Option<PathBuf> {
    if let Some(p) = env_path(CURL_ENV_VAR) {
        return Some(p);
    }
    if let Some(p) = env_path("DATALIB_CURL_DISPATCH") {
        return Some(p);
    }
    locate(DISPATCH_BIN)
}

/// Find one of our sibling shim binaries by name, trying every place an
/// installed release / bazel run / cargo build could have put it. Shared
/// by the dispatch curl and the credential-capture shim; the env-var
/// overrides are the caller's business, since they differ per binary.
fn locate(base: &str) -> Option<PathBuf> {
    let names = spellings(base);
    if let Some(p) = from_runfiles(&names) {
        return Some(p);
    }
    if let Some(p) = from_workspace_walk(&names) {
        return Some(p);
    }
    if let Some(p) = from_exe_dir(&names) {
        return Some(p);
    }
    names.iter().find_map(|n| which_on_path(n))
}

/// Look for a shim next to `current_exe()`. This is how an installed
/// release (e.g. `~/.local/bin/datalib-step`) finds its bundled siblings
/// without needing `~/.local/bin` on `PATH` or any env override. Follow
/// the symlink that scripts/install.sh resolved to so we look in the real
/// install dir, not a shim dir.
fn from_exe_dir(names: &[String]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?;
    names
        .iter()
        .map(|n| dir.join(n))
        .find(|candidate| candidate.is_file())
}

fn env_path(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    let p = PathBuf::from(v);
    p.exists().then_some(p)
}

fn from_runfiles(names: &[String]) -> Option<PathBuf> {
    // The `runfiles` crate's `Runfiles::create` only succeeds when one of
    // RUNFILES_DIR / RUNFILES_MANIFEST_FILE is set, which Bazel does for
    // `bazel run` and `bazel test`. Outside Bazel it returns Err and we
    // fall through. We use the method form rather than the `rlocation!`
    // macro because the macro requires `REPOSITORY_NAME` to be set at
    // compile time (which only happens when this crate is built by
    // rules_rust under Bazel — cargo builds it without that env var).
    let rf = runfiles::Runfiles::create().ok()?;
    for name in names {
        // `_main` is bzlmod's canonical name for the main repo.
        let Some(p) = rf.rlocation(format!("_main/datalib/backend/etl/{name}")) else {
            continue;
        };
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn from_workspace_walk(names: &[String]) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    // Bazel sets these for `bazel run` / `bazel test`. BUILD_WORKING_DIRECTORY
    // is where the user invoked bazel from (usually workspace root);
    // BUILD_WORKSPACE_DIRECTORY is the workspace root itself.
    for var in ["BUILD_WORKING_DIRECTORY", "BUILD_WORKSPACE_DIRECTORY"] {
        if let Some(v) = std::env::var_os(var) {
            roots.push(PathBuf::from(v));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    // CARGO_MANIFEST_DIR of *this* crate (the etl crate) — useful for
    // tests that cargo runs with arbitrary CWDs.
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for root in roots {
        let mut cur: Option<&std::path::Path> = Some(&root);
        while let Some(dir) = cur {
            for rel in [
                "datalib/backend/target/debug",
                "datalib/backend/target/release",
                "target/debug",
                "target/release",
            ] {
                for name in names {
                    let candidate = dir.join(rel).join(name);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            cur = dir.parent();
        }
    }
    None
}

fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Re-exports of the ONE canonical latchkey pin and the user-facing
/// invocation hint (`datalib_core::node_runtime`) — re-exports
/// rather than literals so this crate and the provider crates cannot
/// drift from the hint text / staged tree (same discipline as the qmd
/// pin).
pub use datalib_core::node_runtime::{latchkey_cli_hint, LATCHKEY_VERSION};

/// Entry script of the `latchkey` npm package inside a staged runtime
/// tree (its package.json `bin` target), equivalent to what
/// `npx latchkey` execs.
const LATCHKEY_ENTRY_REL: &str = "node_modules/latchkey/dist/src/cli.js";

/// `std::process::Command` for `latchkey`. Sets `LATCHKEY_CURL` to the
/// shim on first call. If the shim can't be found, logs a warning and
/// returns the `Command` anyway — callers may still succeed against
/// non-CF endpoints.
///
/// Resolution: the app-bundled Node runtime + latchkey tree when staged
/// (Tauri bundles ship one — see `datalib_core::node_runtime`),
/// else `npx -y latchkey@<pin>` (same pattern as qmd in
/// `datalib_qmd_indexer::run_qmd`) so callers don't need a global
/// install. Runtime overrides: `$DATALIB_RUNTIME_DIR` points at a
/// staged runtime tree; `$NPX_BIN` lets a developer pin a specific npx
/// when running outside bazel. Bazel actions don't get these vars
/// forwarded (it would bust the action cache key per shell); they rely
/// on the pinned `PATH` from `.bazelrc` instead.
pub fn latchkey_command() -> std::process::Command {
    warn_if_missing();
    datalib_core::node_runtime::bundled_command("latchkey", LATCHKEY_VERSION, LATCHKEY_ENTRY_REL)
        .unwrap_or_else(|| {
            datalib_core::node_runtime::npx_command(&format!("latchkey@{LATCHKEY_VERSION}"))
        })
}

/// Tokio variant. Same resolution as [`latchkey_command`].
pub fn latchkey_tokio_command() -> tokio::process::Command {
    tokio::process::Command::from(latchkey_command())
}

fn warn_if_missing() {
    // In gateway mode the gateway supplies its own dispatch curl, so a
    // missing local one costs nothing and the warning would be misleading.
    if is_gateway_mode(std::env::var_os(GATEWAY_ENV_VAR).as_deref()) {
        return;
    }
    if let Err(e) = ensure_curl_dispatch() {
        tracing::warn!(error = %e, "running latchkey without the in-tree curl shim; Cloudflare-protected endpoints will likely 403");
    }
}

// ─────────────────────────────────────────────────────────────────────
// Credential extraction (non-HTTP transports)
// ─────────────────────────────────────────────────────────────────────
//
// latchkey delivers credentials by putting them in a curl argv. A
// transport that isn't HTTP has no curl invocation to inject into, so
// it has to read the credential out as values instead. We do that by
// pointing `$LATCHKEY_CURL` at `latchkey-cred-extract`, which writes
// argv to a file and makes no request. See that binary's module docs.

/// A credential read back out of latchkey, classified by what an IMAP
/// (or other SASL) client can do with it.
///
/// `Debug` is hand-written to redact. These values are live secrets and
/// this type will end up inside `anyhow` context chains, `tracing`
/// fields, and test failure output sooner or later.
#[derive(Clone, PartialEq, Eq)]
pub enum LatchkeyCredential {
    /// From `-u user:pass`. Drives IMAP `AUTHENTICATE PLAIN` / `LOGIN`.
    Basic { username: String, password: String },
    /// From `-H "Authorization: Bearer <token>"`. Drives IMAP
    /// `AUTHENTICATE XOAUTH2`. latchkey refreshes OAuth access tokens on
    /// use, so a per-run extraction always yields a live token.
    Bearer { token: String },
}

impl std::fmt::Debug for LatchkeyCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The username is not the secret and is load-bearing when
            // diagnosing "wrong account" mistakes, so it stays legible.
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(
        "could not locate {CRED_EXTRACT_BIN}; set ${CRED_EXTRACT_ENV_VAR}, or build it \
         (`cargo build -p datalib-etl --bin latchkey-cred-extract` \
         or `bazel build //datalib/backend/etl:latchkey_cred_extract`)"
    )]
    ShimNotFound,
    #[error("`latchkey services info {service}` failed: {detail}")]
    ServiceLookup { service: String, detail: String },
    #[error(
        "latchkey service {service:?} has no usable stored credential ({detail}). Attach one:\n  \
         latchkey auth set {service} -u \"<user>:$(pbpaste)\""
    )]
    NoCredential { service: String, detail: String },
    #[error(
        "latchkey service {service:?} returned a credential shape this transport can't use \
         ({found}). Expected `-u user:pass` or `-H \"Authorization: Bearer …\"`."
    )]
    UnsupportedShape { service: String, found: String },
    #[error("running latchkey to extract the {service:?} credential: {source}")]
    Spawn {
        service: String,
        #[source]
        source: std::io::Error,
    },
}

/// Read the credential latchkey holds for `service`, without making any
/// network request.
///
/// Mechanism: ask latchkey for the service's base API URL, then run
/// `latchkey curl <that url>` with `$LATCHKEY_CURL` pointed at our
/// capture shim. latchkey injects the credential into argv as it always
/// does; the shim writes argv to a private file and exits; we read and
/// classify it. The URL is a routing key for latchkey's host-based
/// credential lookup and is never dialed.
///
/// The capture file is created `0600` inside a fresh temp dir and both
/// are removed before returning — including on the error paths.
pub async fn extract_credential(service: &str) -> Result<LatchkeyCredential, CredentialError> {
    let shim = env_path(CRED_EXTRACT_ENV_VAR)
        .or_else(|| locate(CRED_EXTRACT_BIN))
        .ok_or(CredentialError::ShimNotFound)?;
    let sentinel = service_base_url(service).await?;

    // `TempDir` unlinks the whole directory on drop, so the secret is
    // gone whichever way we leave this function.
    let dir = tempfile::tempdir().map_err(|e| CredentialError::Spawn {
        service: service.to_string(),
        source: e,
    })?;
    let capture = dir.path().join("cred");

    let output = latchkey_tokio_command()
        .arg("curl")
        .arg(&sentinel)
        .env(CURL_ENV_VAR, &shim)
        .env(CAPTURE_ENV_VAR, &capture)
        .output()
        .await
        .map_err(|e| CredentialError::Spawn {
            service: service.to_string(),
            source: e,
        })?;

    // A missing capture file means latchkey never spawned curl at all.
    // `service_base_url` above already proved the service resolves — an
    // unknown one fails there — so the only remaining reason is that no
    // credential is attached to it. That is the first-run case, so carry
    // latchkey's own wording through as the detail.
    let Ok(bytes) = tokio::fs::read(&capture).await else {
        return Err(CredentialError::NoCredential {
            service: service.to_string(),
            detail: first_line(&String::from_utf8_lossy(&output.stderr)),
        });
    };

    classify(service, &decode(&bytes))
}

/// Split the capture file back into arguments. Every argument was
/// written NUL-terminated, so the trailing empty element is dropped
/// rather than being read as an empty final argument.
fn decode(bytes: &[u8]) -> Vec<String> {
    let mut args: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    args.pop();
    args
}

/// Pick the credential out of the captured argv.
///
/// latchkey appends the URL and whatever the caller passed, so we scan
/// for the shapes we understand rather than assuming a position.
fn classify(service: &str, argv: &[String]) -> Result<LatchkeyCredential, CredentialError> {
    let mut it = argv.iter();
    let mut shapes: Vec<&str> = Vec::new();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "-u" | "--user" => {
                let Some(value) = it.next() else { continue };
                // Only the FIRST colon separates user from password:
                // passwords contain colons, usernames cannot.
                let Some((username, password)) = value.split_once(':') else {
                    shapes.push("-u with no colon");
                    continue;
                };
                return Ok(LatchkeyCredential::Basic {
                    username: username.to_string(),
                    password: password.to_string(),
                });
            }
            "-H" | "--header" => {
                let Some(value) = it.next() else { continue };
                let Some((name, v)) = value.split_once(':') else {
                    continue;
                };
                if !name.trim().eq_ignore_ascii_case("authorization") {
                    continue;
                }
                let v = v.trim();
                if let Some(token) = strip_prefix_ci(v, "Bearer ") {
                    return Ok(LatchkeyCredential::Bearer {
                        token: token.trim().to_string(),
                    });
                }
                // Basic-over-HTTP is base64(user:pass), which IMAP cannot
                // use directly. Name it rather than silently ignoring it —
                // it is the shape a CardDAV-style registration produces,
                // so someone will hit this.
                if starts_with_ci(v, "Basic ") {
                    shapes.push("Authorization: Basic (base64) — use `-u user:pass` instead");
                } else {
                    shapes.push("Authorization: <unrecognized scheme>");
                }
            }
            _ => {}
        }
    }
    Err(if shapes.is_empty() {
        CredentialError::NoCredential {
            service: service.to_string(),
            detail: "latchkey injected no credential arguments".to_string(),
        }
    } else {
        CredentialError::UnsupportedShape {
            service: service.to_string(),
            found: shapes.join("; "),
        }
    })
}

fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    starts_with_ci(haystack, prefix).then(|| &haystack[prefix.len()..])
}

/// First non-empty line, for folding a multi-line latchkey error into a
/// one-line `thiserror` field.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

/// `latchkey services info <service>` → its first base API URL. That URL
/// is what latchkey's host-based credential lookup keys on, so it is
/// what we must pass to `latchkey curl` for the right credential to be
/// injected.
async fn service_base_url(service: &str) -> Result<String, CredentialError> {
    let output = latchkey_tokio_command()
        .arg("services")
        .arg("info")
        .arg(service)
        .output()
        .await
        .map_err(|e| CredentialError::Spawn {
            service: service.to_string(),
            source: e,
        })?;
    if !output.status.success() {
        return Err(CredentialError::ServiceLookup {
            service: service.to_string(),
            detail: first_line(&String::from_utf8_lossy(&output.stderr)),
        });
    }
    base_url_from_info(service, &output.stdout)
}

fn base_url_from_info(service: &str, stdout: &[u8]) -> Result<String, CredentialError> {
    let info: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| CredentialError::ServiceLookup {
            service: service.to_string(),
            detail: format!("unparseable JSON: {e}"),
        })?;
    info.get("baseApiUrls")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| CredentialError::ServiceLookup {
            service: service.to_string(),
            detail: "no baseApiUrls in `latchkey services info` output".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    /// The shape `latchkey auth set <svc> -u "user:pass"` produces —
    /// verified against latchkey 3.6.0, which emits exactly
    /// `["-u", "user:pass", "<url>"]`.
    #[test]
    fn classifies_a_basic_credential() {
        let got = classify("svc", &args(&["-u", "me@gmail.com:apppw", "https://x/"])).unwrap();
        assert_eq!(
            got,
            LatchkeyCredential::Basic {
                username: "me@gmail.com".into(),
                password: "apppw".into(),
            }
        );
    }

    /// Only the first colon separates the two. App passwords are opaque
    /// and may contain colons; usernames may not.
    #[test]
    fn splits_only_on_the_first_colon() {
        let got = classify("svc", &args(&["-u", "me:pa:ss:word"])).unwrap();
        let LatchkeyCredential::Basic { password, .. } = got else {
            panic!("expected Basic");
        };
        assert_eq!(password, "pa:ss:word");
    }

    #[test]
    fn classifies_a_bearer_credential() {
        for header in [
            "Authorization: Bearer ya29.tok",
            "authorization:Bearer ya29.tok",
            "Authorization: bearer ya29.tok",
        ] {
            let got = classify("svc", &args(&["-H", header, "https://x/"])).unwrap();
            assert_eq!(
                got,
                LatchkeyCredential::Bearer {
                    token: "ya29.tok".into()
                },
                "failed on {header:?}"
            );
        }
    }

    /// Headers latchkey adds for other reasons must not be mistaken for
    /// the credential.
    #[test]
    fn ignores_unrelated_headers() {
        let err = classify(
            "svc",
            &args(&[
                "-H",
                "Accept: */*",
                "-H",
                "X-Imbue-Impersonate:",
                "https://x/",
            ]),
        )
        .unwrap_err();
        assert!(matches!(err, CredentialError::NoCredential { .. }));
    }

    /// HTTP Basic is base64(user:pass) — an IMAP client cannot use it as
    /// a SASL credential, and quietly ignoring it would surface as a
    /// baffling "no credential" much later. Name the shape instead.
    #[test]
    fn rejects_http_basic_with_a_pointer_to_the_fix() {
        let err = classify(
            "svc",
            &args(&["-H", "Authorization: Basic bWU6cHc=", "https://x/"]),
        )
        .unwrap_err();
        let CredentialError::UnsupportedShape { found, .. } = &err else {
            panic!("expected UnsupportedShape, got {err:?}");
        };
        assert!(found.contains("-u user:pass"), "unhelpful message: {found}");
    }

    /// A latchkey service with nothing attached: curl is spawned with
    /// just the URL.
    #[test]
    fn reports_a_missing_credential_as_such() {
        let err = classify("svc", &args(&["https://x/"])).unwrap_err();
        assert!(matches!(err, CredentialError::NoCredential { .. }));
        // The message has to be actionable — it is the first thing a user
        // sees when they haven't run `latchkey auth set` yet.
        assert!(err.to_string().contains("latchkey auth set svc"));
    }

    /// A dangling flag with no value must not panic or consume the URL.
    #[test]
    fn tolerates_a_dangling_flag() {
        let err = classify("svc", &args(&["-u"])).unwrap_err();
        assert!(matches!(err, CredentialError::NoCredential { .. }));
    }

    /// Round-trip through the wire format the shim writes. The point of
    /// NUL-separation is that no password needs escaping.
    #[test]
    fn decodes_what_the_shim_encodes() {
        let raw = b"-u\0me:pa\"ss\\w rd\0https://x/\0";
        assert_eq!(decode(raw), args(&["-u", "me:pa\"ss\\w rd", "https://x/"]));
    }

    #[test]
    fn decodes_an_empty_capture_as_no_arguments() {
        assert!(decode(b"").is_empty());
    }

    /// Real `latchkey services info` output, both flavors (built-in and
    /// user-registered), captured from latchkey 3.6.0.
    #[test]
    fn reads_the_base_url_out_of_services_info() {
        let info = br#"{"type":"user-registered","baseApiUrls":["https://imap.gmail.com/"],
                        "authOptions":["set"],"credentials":{}}"#;
        assert_eq!(
            base_url_from_info("gmail-imap", info).unwrap(),
            "https://imap.gmail.com/"
        );
    }

    #[test]
    fn surfaces_a_services_info_shape_it_cannot_read() {
        let err = base_url_from_info("svc", br#"{"type":"user-registered"}"#).unwrap_err();
        assert!(matches!(err, CredentialError::ServiceLookup { .. }));
    }

    /// The secret must not survive a `{:?}` — this type will end up in
    /// anyhow chains and tracing fields.
    #[test]
    fn debug_redacts_the_secret() {
        let basic = LatchkeyCredential::Basic {
            username: "me@gmail.com".into(),
            password: "hunter2".into(),
        };
        let rendered = format!("{basic:?}");
        assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
        assert!(
            rendered.contains("me@gmail.com"),
            "over-redacted: {rendered}"
        );

        let bearer = LatchkeyCredential::Bearer {
            token: "ya29.secret".into(),
        };
        let rendered = format!("{bearer:?}");
        assert!(!rendered.contains("ya29.secret"), "leaked: {rendered}");
    }

    fn os(value: &str) -> &OsStr {
        OsStr::new(value)
    }

    /// With no gateway and nothing preset, we publish our dispatch curl --
    /// the standalone app's configuration, where `latchkey curl` makes the
    /// request to the third party itself.
    #[test]
    fn exports_dispatch_curl_when_latchkey_talks_to_the_third_party() {
        assert!(should_export_curl_dispatch(None, None));
        assert!(should_export_curl_dispatch(None, Some(os(""))));
    }

    /// A caller's explicit `LATCHKEY_CURL` always wins.
    #[test]
    fn never_overrides_an_explicit_setting() {
        assert!(!should_export_curl_dispatch(
            Some(os("/usr/bin/curl")),
            None
        ));
        assert!(!should_export_curl_dispatch(
            Some(os("/usr/bin/curl")),
            Some(os("http://127.0.0.1:9"))
        ));
    }

    /// In gateway mode the gateway's own dispatch curl makes the request
    /// that reaches the third party. Putting one on the client hop instead
    /// would consume the marker header there and impersonate the hop to the
    /// gateway, leaving the one that matters unimpersonated.
    #[test]
    fn leaves_latchkey_curl_alone_in_gateway_mode() {
        assert!(!should_export_curl_dispatch(
            None,
            Some(os("http://127.0.0.1:9"))
        ));
    }

    #[test]
    fn treats_an_empty_gateway_setting_as_no_gateway() {
        assert!(!is_gateway_mode(None));
        assert!(!is_gateway_mode(Some(os(""))));
        assert!(is_gateway_mode(Some(os("http://127.0.0.1:9"))));
    }
}
