//! Single entrypoint for spawning the `latchkey` CLI.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::OnceLock;

const ROUTER_BIN: &str = "latchkey-curl-router";
/// Where we publish the resolved router curl for the `latchkey` CLI.
const CURL_ENV_VAR: &str = "LATCHKEY_CURL";
/// Set by latchkey's callers to route every request through a gateway.
const GATEWAY_ENV_VAR: &str = "LATCHKEY_GATEWAY";
/// A developer's explicit override, consulted after `LATCHKEY_CURL`.
const ROUTER_ENV_VAR: &str = "DATALIB_CURL_ROUTER";
/// Where `//third-party/latchkey-curl-shims` puts the router under
/// `_main/` (bzlmod's main-repo canonical name).
const RUNFILES_PATH: &str = "_main/third-party/latchkey-curl-shims/latchkey-curl-router";

static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
#[error(
    "could not locate {ROUTER_BIN}; set ${ROUTER_ENV_VAR} or ${CURL_ENV_VAR}, \
     or fetch it (`bazel build //third-party/latchkey-curl-shims`)"
)]
pub struct CurlRouterNotFound;

/// Ensure `LATCHKEY_CURL` points at the bundled router curl and return
/// its resolved path. Idempotent — the first call resolves and caches;
/// later calls are a `OnceLock` read.
pub fn ensure_curl_router() -> Result<PathBuf, CurlRouterNotFound> {
    match RESOLVED.get_or_init(resolve) {
        Some(path) => {
            if should_export_curl_router(
                std::env::var_os(CURL_ENV_VAR).as_deref(),
                std::env::var_os(GATEWAY_ENV_VAR).as_deref(),
            ) {
                std::env::set_var(CURL_ENV_VAR, path);
            }
            Ok(path.clone())
        }
        None => Err(CurlRouterNotFound),
    }
}

fn is_gateway_mode(gateway: Option<&OsStr>) -> bool {
    matches!(gateway, Some(value) if !value.is_empty())
}

/// Whether [`ensure_curl_router`] should point `LATCHKEY_CURL` at the
/// router curl it resolved. Two reasons not to:
fn should_export_curl_router(existing_curl: Option<&OsStr>, gateway: Option<&OsStr>) -> bool {
    existing_curl.is_none() && !is_gateway_mode(gateway)
}

fn resolve() -> Option<PathBuf> {
    if let Some(p) = env_path(CURL_ENV_VAR) {
        return Some(p);
    }
    if let Some(p) = env_path(ROUTER_ENV_VAR) {
        return Some(p);
    }
    if let Some(p) = from_runfiles() {
        return Some(p);
    }
    if let Some(p) = from_exe_dir() {
        return Some(p);
    }
    which_on_path(ROUTER_BIN)
}

/// Look for the router curl next to `current_exe()`. This is how an
/// installed release (e.g. `~/.local/bin/datalib-step`) finds its
/// bundled `latchkey-curl-router` sibling without needing `~/.local/bin`
/// on `PATH` or any env override. Follow the symlink that
/// scripts/install.sh resolved to so we look in the real install dir,
/// not a shim dir.
fn from_exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let candidate = exe.parent()?.join(ROUTER_BIN);
    candidate.is_file().then_some(candidate)
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
    let p = rf.rlocation(RUNFILES_PATH)?;
    p.exists().then_some(p)
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
/// invocation hint (`datalib_runtime::node_runtime`) — re-exports
/// rather than literals so this crate and the provider crates cannot
/// drift from the hint text / staged tree (same discipline as the qmd
/// pin).
pub use datalib_runtime::node_runtime::{latchkey_cli_hint, LATCHKEY_VERSION};

/// `std::process::Command` for `latchkey` from the staged runtime tree
/// (`datalib_runtime::node_runtime::latchkey_command` decides; the
/// error says where it looked). Sets `LATCHKEY_CURL` to the router on
/// first call. If the router can't be found, logs a warning and returns
/// the `Command` anyway — callers may still succeed against non-CF
/// endpoints.
pub fn latchkey_command() -> anyhow::Result<std::process::Command> {
    warn_if_missing();
    Ok(datalib_runtime::node_runtime::latchkey_command()?)
}

pub fn latchkey_tokio_command() -> anyhow::Result<tokio::process::Command> {
    Ok(tokio::process::Command::from(latchkey_command()?))
}

pub fn latchkey_curl_command(
    settings: &datalib_source_common::LatchkeySettings,
) -> anyhow::Result<tokio::process::Command> {
    let mut cmd = latchkey_tokio_command()?;
    if let Some(account) = settings.account() {
        cmd.arg("--account").arg(account);
    }
    cmd.arg("curl");
    Ok(cmd)
}

fn warn_if_missing() {
    // In gateway mode the gateway supplies its own router curl, so a
    // missing local one costs nothing and the warning would be misleading.
    if is_gateway_mode(std::env::var_os(GATEWAY_ENV_VAR).as_deref()) {
        return;
    }
    if let Err(e) = ensure_curl_router() {
        tracing::warn!(error = %e, "running latchkey without the bundled curl router; Cloudflare-protected endpoints will likely 403");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(value: &str) -> &OsStr {
        OsStr::new(value)
    }

    /// With no gateway and nothing preset, we publish our router curl --
    /// the standalone app's configuration, where `latchkey curl` makes the
    /// request to the third party itself.
    #[test]
    fn exports_router_curl_when_latchkey_talks_to_the_third_party() {
        assert!(should_export_curl_router(None, None));
        assert!(should_export_curl_router(None, Some(os(""))));
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
        // SAFETY: the test reads the env for no other reason.
        unsafe { std::env::set_var(datalib_runtime::node_runtime::ALLOW_NPX_ENV, "1") };
        let cmd = latchkey_curl_command(&settings).expect("npx fallback enabled above");
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
        // SAFETY: the test reads the env for no other reason.
        unsafe { std::env::set_var(datalib_runtime::node_runtime::ALLOW_NPX_ENV, "1") };
        let cmd = latchkey_curl_command(&datalib_source_common::LatchkeySettings::default())
            .expect("npx fallback enabled above");
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
        assert!(!should_export_curl_router(Some(os("/usr/bin/curl")), None));
        assert!(!should_export_curl_router(
            Some(os("/usr/bin/curl")),
            Some(os("http://127.0.0.1:9"))
        ));
    }

    /// In gateway mode the gateway's own router curl makes the request
    /// that reaches the third party. Putting one on the client hop instead
    /// would consume the marker header there and impersonate the hop to the
    /// gateway, leaving the one that matters unimpersonated.
    #[test]
    fn leaves_latchkey_curl_alone_in_gateway_mode() {
        assert!(!should_export_curl_router(
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
