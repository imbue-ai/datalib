//! Locate the Node runtime + npm package trees bundled with the Tauri
//! app, so `latchkey` and `qmd` run without any Node/npm on the host.
//!
//! `datalib/tauri/stage-runtime.sh` stages (and the app bundles
//! under `Contents/Resources/`) this layout:
//!
//! ```text
//! runtime/
//!   node/bin/node                  pinned Node runtime
//!   latchkey/<version>/node_modules/latchkey/dist/src/cli.js
//!   qmd/<version>/node_modules/@tobilu/qmd/dist/cli/qmd.js
//! ```
//!
//! Trees are keyed by the exact version the Rust callers pin, so a
//! version bump that isn't re-staged simply misses here and falls back
//! to `npx` — same behavior as today, never a stale tree. The staging
//! script greps its versions out of the Rust sources (see its header),
//! which keeps the two sides from drifting silently.
//!
//! Resolution order for the `runtime/` root (first hit wins):
//!   1. `$DATALIB_RUNTIME_DIR` — explicit override; tests, dev runs,
//!      and non-Tauri packagers that ship the tree elsewhere.
//!   2. `<exe_dir>/../runtime` — the macOS .app layout: our binaries are
//!      bundled resources under `Contents/Resources/binaries/`, and the
//!      runtime tree sits next to them at `Contents/Resources/runtime/`.
//!   3. `<exe_dir>/runtime` — flat layouts (a release tarball unpacked
//!      into one directory).
//!
//! A miss anywhere returns `None` and callers fall back to
//! `npx -y <pkg>@<version>` via [`npx_command`], which is exactly the
//! pre-bundling behavior.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Relative path of the Node executable inside `runtime/`.
const NODE_REL: &str = "node/bin/node";

/// The ONE canonical latchkey version pin (see the qmd twin,
/// `DEFAULT_QMD_VERSION` in `datalib_unified_index::qmd`): used for the `npx` fallback
/// spec, as the key into the staged `runtime/latchkey/<version>/` tree,
/// and re-exported by `datalib_etl::latchkey`.
/// `datalib/tauri/stage-runtime.sh` greps this constant to decide
/// what to stage — keep the `LATCHKEY_VERSION` name and string-literal
/// shape.
pub const LATCHKEY_VERSION: &str = "3.7.0";

/// The latchkey invocation to show in user-facing instructions and
/// error messages: the app-bundled launcher when present (the
/// `latchkey` wrapper staged next to our binaries — see
/// `datalib/tauri/latchkey-wrapper.sh`), else the `npx` form.
/// Returns a shell-ready command prefix, quoted if the path needs it,
/// so callers can render e.g. `{hint} auth set slack …` and the user
/// can paste it verbatim.
pub fn latchkey_cli_hint() -> String {
    if let Ok(exe) = std::env::current_exe() {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = exe.parent() {
            let wrapper = dir.join("latchkey");
            if wrapper.is_file() {
                return shell_quote(&wrapper.to_string_lossy());
            }
        }
    }
    format!("npx -y latchkey@{LATCHKEY_VERSION}")
}

/// Single-quote `s` for POSIX shells unless it's plainly safe. Good
/// enough for rendering paths inside copy-pasteable instructions.
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._+-@%".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Resolve the staged `runtime/` root, or `None` when not bundled.
pub fn runtime_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DATALIB_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        // An explicitly-set override that doesn't exist is a
        // misconfiguration; still just miss (callers fall back to npx)
        // but keep the check so we never return a dangling root.
        return dir.is_dir().then_some(dir);
    }
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let exe_dir = exe.parent()?;
    [exe_dir.parent()?.join("runtime"), exe_dir.join("runtime")]
        .into_iter()
        .find(|root| root.join(NODE_REL).is_file())
}

/// `Command` running `entry_rel` (a path under the staged tree, e.g.
/// `node_modules/latchkey/dist/src/cli.js`) of the bundled
/// `<kind>/<version>` package with the bundled Node. `None` unless both
/// the Node binary and the entry file are staged.
pub fn bundled_command(kind: &str, version: &str, entry_rel: &str) -> Option<Command> {
    let root = runtime_root()?;
    let node = root.join(NODE_REL);
    let entry = root.join(kind).join(version).join(entry_rel);
    if !node.is_file() || !entry.is_file() {
        return None;
    }
    let mut cmd = Command::new(node);
    cmd.arg(entry);
    Some(cmd)
}

/// The pre-bundling fallback: `npx -y <pkg_spec>`. Honors `$NPX_BIN` as
/// a runtime override (handy outside bazel; bazel actions rely on the
/// pinned `PATH` from `.bazelrc` instead — see the resolver note there).
///
/// The command gets its own npm cache, scoped by the ABI of the Node
/// that will run it — see [`npx_cache_dir`] for why.
pub fn npx_command(pkg_spec: &str) -> Command {
    let npx = std::env::var_os("NPX_BIN").unwrap_or_else(|| "npx".into());
    let mut cmd = Command::new(&npx);
    if let Some(cache) = npx_cache_dir(&npx) {
        cmd.env("npm_config_cache", cache);
    }
    cmd.arg("-y").arg(pkg_spec);
    cmd
}

/// A private npm cache for [`npx_command`], scoped by Node's ABI.
///
/// npm keys the npx package directory on the **package spec alone** —
/// `<cache>/_npx/<hash of "@tobilu/qmd@2.8.3">` — with no Node version
/// anywhere in it. So by default every Node on a machine shares one
/// installed tree, while a native module inside that tree may be built
/// for exactly one `NODE_MODULE_VERSION`.
///
/// The original offender was named: qmd's `better-sqlite3` 12 shipped a
/// `node-v<abi>` prebuilt. qmd 2.8.3 moved to better-sqlite3 13, whose
/// bindings live in the tarball and are chosen by platform, and the
/// tree-sitter grammars and node-llama-cpp resolve per-platform too — so
/// as of that bump there is no *known* ABI-bound module left in the
/// tree. The scoping stays anyway: it costs one re-install per ABI once,
/// it is not re-audited on every qmd bump, and the failure it prevents
/// surfaces as a `require()` abort deep inside a genrule.
///
/// Two Nodes therefore poison each other. Whichever installs first wins
/// the directory, and the other dies in `require()` with "compiled
/// against a different Node.js version" — every time, until someone
/// deletes the cache, which only re-runs the race. It is not a stale
/// cache and clearing it is not a fix.
///
/// Putting the ABI in the path supplies the dimension npm's key is
/// missing. This lives here rather than at the qmd call site so the next
/// `npx` consumer inherits the fix instead of rediscovering the bug;
/// `latchkey` is unaffected either way, since its one native module
/// (`@napi-rs/keyring`) is N-API and ABI-stable across Node majors.
///
/// Costs one re-install per ABI, once. `None` — meaning npm's own
/// default applies, exactly as before — when there is no `$HOME` or the
/// ABI can't be read, because a shared cache that usually works beats no
/// qmd at all.
fn npx_cache_dir(npx: &OsStr) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(cache_dir_for(Path::new(&home), &node_abi(npx)?))
}

/// Path construction, split out so it is testable without a Node on the
/// host. Sits under `~/.cache/datalib/` beside the `~/.cache/qmd/models`
/// tree qmd itself uses.
fn cache_dir_for(home: &Path, abi: &str) -> PathBuf {
    home.join(".cache").join("datalib").join("npx").join(abi)
}

/// `process.versions.modules` of the Node that `npx` runs under.
///
/// Probed once per process — it is a subprocess, every caller wants the
/// same answer, and `$NPX_BIN` does not change under a running process.
fn node_abi(npx: &OsStr) -> Option<String> {
    static ABI: OnceLock<Option<String>> = OnceLock::new();
    ABI.get_or_init(|| {
        let out = Command::new(node_beside(npx))
            .arg("-p")
            .arg("process.versions.modules")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| parse_abi(&String::from_utf8_lossy(&out.stdout)))?
    })
    .clone()
}

/// The ABI as a path-safe token, or `None` if Node printed anything but
/// a bare number. Validated rather than trusted: this becomes a
/// directory name.
fn parse_abi(stdout: &str) -> Option<String> {
    let s = stdout.trim();
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit())).then(|| s.to_string())
}

/// The `node` that will run `npx`: its sibling when `$NPX_BIN` names a
/// real path, else whatever `node` `PATH` resolves — which is what npx
/// itself would use.
fn node_beside(npx: &OsStr) -> OsString {
    let dir = Path::new(npx)
        .parent()
        .filter(|d| !d.as_os_str().is_empty());
    match dir.map(|d| d.join("node")) {
        Some(node) if node.is_file() => node.into_os_string(),
        _ => "node".into(),
    }
}

/// One-line rendering of a `Command` (program + args) for status-line
/// logging, so call sites can show the real invocation whether it
/// resolved to the bundled runtime or npx.
pub fn display_command(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

/// True when `cmd`'s program is under the staged runtime — lets
/// diagnostics say which flavor ran.
pub fn is_bundled(cmd: &Command) -> bool {
    runtime_root().is_some_and(|root| Path::new(cmd.get_program()).starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end resolution against a synthetic staged tree, driven
    /// through `$DATALIB_RUNTIME_DIR`.
    ///
    /// One test body covers hit + both miss shapes (missing entry,
    /// missing version) because they share the env var, and Rust tests
    /// in one crate share a process — splitting them would race on
    /// `set_var` (same pattern as qmd_indexer's env tests).
    #[test]
    fn bundled_command_resolves_staged_tree() {
        let base =
            std::env::temp_dir().join(format!("datalib-runtime-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = base.join(NODE_REL);
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, b"#!/bin/sh\n").unwrap();
        let entry = base.join("latchkey/1.2.3/node_modules/latchkey/dist/src/cli.js");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, b"// cli\n").unwrap();

        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("DATALIB_RUNTIME_DIR", &base) };

        let cmd = bundled_command("latchkey", "1.2.3", "node_modules/latchkey/dist/src/cli.js")
            .expect("staged tree should resolve");
        assert_eq!(cmd.get_program(), node.as_os_str());
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![entry.as_os_str()]);
        assert!(is_bundled(&cmd));
        assert_eq!(
            display_command(&cmd),
            format!("{} {}", node.display(), entry.display())
        );

        // Version not staged → miss.
        assert!(
            bundled_command("latchkey", "9.9.9", "node_modules/latchkey/dist/src/cli.js").is_none()
        );
        // Entry file absent → miss.
        assert!(bundled_command("latchkey", "1.2.3", "node_modules/latchkey/nope.js").is_none());

        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var("DATALIB_RUNTIME_DIR") };
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shell_quote_paths() {
        assert_eq!(
            shell_quote("/Applications/F.app/binaries/latchkey"),
            "/Applications/F.app/binaries/latchkey"
        );
        assert_eq!(
            shell_quote("/Users/a b/Datalib.app/latchkey"),
            "'/Users/a b/Datalib.app/latchkey'"
        );
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn npx_command_honors_npx_bin() {
        // Default program is `npx` (don't set NPX_BIN here — the other
        // test owns DATALIB_RUNTIME_DIR; this one only reads).
        let cmd = npx_command("latchkey@1.2.3");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec!["-y", "latchkey@1.2.3"]);
    }

    /// The env var actually lands on the command. Both branches assert:
    /// with a Node on the host the cache must be set AND ABI-scoped;
    /// without one it must be absent, so npm's default still applies.
    /// Written this way because a plain `if node exists` test would pass
    /// vacuously on a host without Node — which is most sandboxes.
    #[test]
    fn npx_command_scopes_the_cache_by_abi() {
        let cmd = npx_command("latchkey@1.2.3");
        let cache = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("npm_config_cache"))
            .and_then(|(_, v)| v)
            .map(PathBuf::from);

        // Both inputs are required; `$HOME` is absent in some sandboxes.
        let abi = node_abi(OsStr::new("npx")).filter(|_| std::env::var_os("HOME").is_some());
        match abi {
            Some(abi) => {
                let cache = cache.expect("with node and $HOME, the cache must be scoped");
                assert_eq!(cache.file_name().unwrap(), OsStr::new(&abi));
                assert_eq!(cache.parent().unwrap().file_name().unwrap(), "npx");
            }
            None => assert_eq!(cache, None, "without an ABI, leave npm's default alone"),
        }
    }

    /// The ABI is a directory name, so anything that isn't a bare
    /// number has to be rejected rather than pasted into a path.
    #[test]
    fn parse_abi_takes_only_bare_numbers() {
        assert_eq!(parse_abi("127\n").as_deref(), Some("127"));
        assert_eq!(parse_abi("  147  ").as_deref(), Some("147"));
        assert_eq!(parse_abi(""), None);
        assert_eq!(parse_abi("\n"), None);
        assert_eq!(parse_abi("v22.22.0"), None);
        assert_eq!(parse_abi("../../etc"), None);
        assert_eq!(parse_abi("127 128"), None);
    }

    /// Different ABIs must not land in one directory — that collision is
    /// the entire bug this scoping exists to prevent.
    #[test]
    fn cache_dir_separates_abis() {
        let home = Path::new("/home/u");
        let a = cache_dir_for(home, "127");
        let b = cache_dir_for(home, "147");
        assert_ne!(a, b);
        assert_eq!(a, Path::new("/home/u/.cache/datalib/npx/127"));
    }

    /// `$NPX_BIN` pointing at a real directory means the Node beside it
    /// is the one npx will use; a bare `npx` (or a path with no sibling
    /// node) falls back to PATH resolution.
    #[test]
    fn node_beside_prefers_the_npx_sibling() {
        let base = std::env::temp_dir().join(format!("datalib-nodebeside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("node"), b"#!/bin/sh\n").unwrap();
        std::fs::write(base.join("npx"), b"#!/bin/sh\n").unwrap();

        assert_eq!(
            node_beside(base.join("npx").as_os_str()),
            base.join("node").into_os_string()
        );
        // Bare program name — nothing to sit beside.
        assert_eq!(node_beside(OsStr::new("npx")), OsString::from("node"));
        // Real directory, but no node in it.
        assert_eq!(
            node_beside(base.join("sub/npx").as_os_str()),
            OsString::from("node")
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
