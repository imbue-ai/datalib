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
/// Where we publish the resolved dispatch curl for the `latchkey` CLI.
const CURL_ENV_VAR: &str = "LATCHKEY_CURL";
/// Set by latchkey's callers to route every request through a gateway.
const GATEWAY_ENV_VAR: &str = "LATCHKEY_GATEWAY";
// Cargo emits the binary as `latchkey-curl-dispatch` (dashes — from
// `[[bin]] name = "latchkey-curl-dispatch"` in Cargo.toml). Bazel emits it
// as `latchkey_curl_dispatch` (underscores — the `rust_binary` target
// name). Try both under `_main/` (bzlmod's main-repo canonical name).
const RUNFILES_PATHS: &[&str] = &[
    "_main/datalib/backend/etl/latchkey_curl_dispatch",
    "_main/datalib/backend/etl/latchkey-curl-dispatch",
];

// Filenames to look for next to `current_exe()`: the cargo/release dash
// form and the bazel underscore form. (The release tarball drops the
// `datalib-` prefix; see .github/workflows/release.yml's stage step
// and //datalib/backend:dist.)
const SIBLING_NAMES: &[&str] = &["latchkey-curl-dispatch", "latchkey_curl_dispatch"];

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
    if let Some(p) = env_path("LATCHKEY_CURL") {
        return Some(p);
    }
    if let Some(p) = env_path("DATALIB_CURL_DISPATCH") {
        return Some(p);
    }
    if let Some(p) = from_runfiles() {
        return Some(p);
    }
    if let Some(p) = from_workspace_walk() {
        return Some(p);
    }
    if let Some(p) = from_exe_dir() {
        return Some(p);
    }
    which_on_path(DISPATCH_BIN)
}

/// Look for the shim next to `current_exe()`. This is how an installed
/// release (e.g. `~/.local/bin/datalib-step`)
/// finds its bundled `latchkey-curl-impersonate` sibling without
/// needing `~/.local/bin` on `PATH` or any env override. Follow the
/// symlink that scripts/install.sh resolved to so we look in the real
/// install dir, not a shim dir.
fn from_exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?;
    for name in SIBLING_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn env_path(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    let p = PathBuf::from(v);
    p.exists().then_some(p)
}

fn from_runfiles() -> Option<PathBuf> {
    // The `runfiles` crate's `Runfiles::create` only succeeds when one of
    // RUNFILES_DIR / RUNFILES_MANIFEST_FILE is set, which Bazel does for
    // `bazel run` and `bazel test`. Outside Bazel it returns Err and we
    // fall through. We use the method form rather than the `rlocation!`
    // macro because the macro requires `REPOSITORY_NAME` to be set at
    // compile time (which only happens when this crate is built by
    // rules_rust under Bazel — cargo builds it without that env var).
    let rf = runfiles::Runfiles::create().ok()?;
    for path in RUNFILES_PATHS {
        let p = rf.rlocation(path)?;
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn from_workspace_walk() -> Option<PathBuf> {
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
                let candidate = dir.join(rel).join(DISPATCH_BIN);
                if candidate.is_file() {
                    return Some(candidate);
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

/// `latchkey [--account <acct>] curl` — a tokio `Command` with the
/// account selector and the `curl` subcommand already pushed.
///
/// The ordering is the whole reason this exists: `--account` is a latchkey
/// **global** option, so it must precede the subcommand. Two providers
/// (slack's file fetch, chatgpt's image fetch) build their curl invocation
/// by hand rather than going through [`crate::http::HttpRequest`], and a
/// hand-rolled `--account` placed after `curl` is rejected by latchkey — so
/// both they and [`crate::http`] come through here and the rule is written
/// down once.
pub fn latchkey_curl_command(
    settings: &datalib_source_common::LatchkeySettings,
) -> tokio::process::Command {
    let mut cmd = latchkey_tokio_command();
    if let Some(account) = settings.account() {
        cmd.arg("--account").arg(account);
    }
    cmd.arg("curl");
    cmd
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `--account` is a latchkey **global** option: placed after the
    /// subcommand it is rejected outright. Nothing else in the tree
    /// asserts the order, and getting it wrong fails only at runtime
    /// against a real multi-account store -- which is exactly the setup
    /// nobody has while developing.
    #[test]
    fn account_selector_precedes_the_curl_subcommand() {
        let settings = datalib_source_common::LatchkeySettings {
            account: Some("thad@imbue.com".to_string()),
        };
        let cmd = latchkey_curl_command(&settings);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let account_at = args.iter().position(|a| a == "--account");
        let curl_at = args.iter().position(|a| a == "curl");
        let (Some(account_at), Some(curl_at)) = (account_at, curl_at) else {
            panic!("expected both `--account` and `curl` in {args:?}");
        };
        assert!(
            account_at < curl_at,
            "`--account` must precede the subcommand, got {args:?}",
        );
        assert_eq!(args[account_at + 1], "thad@imbue.com", "{args:?}");
    }

    /// The common case: no account configured means no selector at all,
    /// so latchkey resolves the single stored credential itself.
    #[test]
    fn no_account_configured_passes_no_selector() {
        let cmd = latchkey_curl_command(&datalib_source_common::LatchkeySettings::default());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "--account"), "{args:?}");
        assert!(args.iter().any(|a| a == "curl"), "{args:?}");
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
