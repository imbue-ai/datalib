//! Single entrypoint for spawning the `latchkey` CLI.
//!
//! Every binary or test that runs `latchkey curl …` must construct its
//! `Command` via [`latchkey_command`] / [`latchkey_tokio_command`] so
//! that `LATCHKEY_CURL` is set exactly once, to the in-tree
//! Chrome-impersonating shim. Cloudflare-protected hosts (claude.ai,
//! chatgpt.com, files.slack.com) reject vanilla curl's TLS fingerprint;
//! the shim (`src/bin/latchkey_curl_shim.rs`) replays a Chrome 131
//! handshake via `wreq`.
//!
//! Resolution order for the shim path (first hit wins):
//!   1. `$LATCHKEY_CURL` — caller's explicit override; trusted as-is.
//!   2. `$FRANKWEILER_CURL_SHIM` — our own override (parallel to
//!      `LATCHKEY_CURL` but specifically the shim binary, so Bazel can
//!      inject the runfiles path without stomping a user-set
//!      `LATCHKEY_CURL`).
//!   3. Bazel runfiles lookup for `_main/frankweiler/backend/etl/latchkey-curl-shim`.
//!   4. Cargo dev fallback: walk up from CWD and the etl crate dir
//!      looking for `frankweiler/backend/target/{debug,release}/latchkey-curl-shim`
//!      or `target/{debug,release}/latchkey-curl-shim`.
//!   5. `which latchkey-curl-shim` on `$PATH`.
//!
//! On miss, the `Command` is still returned but a `warn!` is logged so
//! the caller can see why CF-fronted endpoints are 403-ing.

use std::path::PathBuf;
use std::sync::OnceLock;

const SHIM_BIN: &str = "latchkey-curl-shim";
// Cargo emits the binary as `latchkey-curl-shim` (dashes — from
// `[[bin]] name = "latchkey-curl-shim"` in Cargo.toml). Bazel emits it
// as `latchkey_curl_shim` (underscores — the `rust_binary` target
// name). Try both under `_main/` (bzlmod's main-repo canonical name).
const RUNFILES_PATHS: &[&str] = &[
    "_main/frankweiler/backend/etl/latchkey_curl_shim",
    "_main/frankweiler/backend/etl/latchkey-curl-shim",
];

static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
#[error(
    "could not locate {SHIM_BIN}; set $FRANKWEILER_CURL_SHIM or $LATCHKEY_CURL, \
     or build it (`cargo build -p frankweiler-etl --bin latchkey-curl-shim` \
     or `bazel build //frankweiler/backend/etl:latchkey_curl_shim`)"
)]
pub struct ShimNotFound;

/// Ensure `LATCHKEY_CURL` points at the in-tree shim and return its
/// resolved path. Idempotent — the first call resolves and caches; later
/// calls are a `OnceLock` read.
pub fn ensure_curl_shim() -> Result<PathBuf, ShimNotFound> {
    match RESOLVED.get_or_init(resolve) {
        Some(path) => {
            if std::env::var_os("LATCHKEY_CURL").is_none() {
                std::env::set_var("LATCHKEY_CURL", path);
            }
            Ok(path.clone())
        }
        None => Err(ShimNotFound),
    }
}

fn resolve() -> Option<PathBuf> {
    if let Some(p) = env_path("LATCHKEY_CURL") {
        return Some(p);
    }
    if let Some(p) = env_path("FRANKWEILER_CURL_SHIM") {
        return Some(p);
    }
    if let Some(p) = from_runfiles() {
        return Some(p);
    }
    if let Some(p) = from_workspace_walk() {
        return Some(p);
    }
    which_on_path(SHIM_BIN)
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
                "frankweiler/backend/target/debug",
                "frankweiler/backend/target/release",
                "target/debug",
                "target/release",
            ] {
                let candidate = dir.join(rel).join(SHIM_BIN);
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

/// `std::process::Command` for `latchkey`. Sets `LATCHKEY_CURL` to the
/// shim on first call. If the shim can't be found, logs a warning and
/// returns the `Command` anyway — callers may still succeed against
/// non-CF endpoints.
///
/// We invoke latchkey via `npx -y latchkey` (same pattern as qmd in
/// `frankweiler_qmd_indexer::run_qmd`) so callers don't need a global
/// install. `NPX_BIN` lets direnv / `.bazelrc --action_env` pin the
/// binary independent of whatever PATH the action inherits.
pub fn latchkey_command() -> std::process::Command {
    warn_if_missing();
    let npx = std::env::var_os("NPX_BIN").unwrap_or_else(|| "npx".into());
    let mut cmd = std::process::Command::new(npx);
    cmd.arg("-y").arg("latchkey");
    cmd
}

/// Tokio variant.
pub fn latchkey_tokio_command() -> tokio::process::Command {
    warn_if_missing();
    let npx = std::env::var_os("NPX_BIN").unwrap_or_else(|| "npx".into());
    let mut cmd = tokio::process::Command::new(npx);
    cmd.arg("-y").arg("latchkey");
    cmd
}

fn warn_if_missing() {
    if let Err(e) = ensure_curl_shim() {
        tracing::warn!(error = %e, "running latchkey without the in-tree curl shim; Cloudflare-protected endpoints will likely 403");
    }
}
