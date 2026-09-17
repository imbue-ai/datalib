//! Locate the Node runtime + npm package trees shipped beside the
//! binaries (the .app's `Resources/runtime/`, the tarball's `runtime/`),
//! so `latchkey` and `qmd` run without any Node/npm on the host.
//!
//! The `npx -y <pkg>@<v>` fallback is a supply-chain hole — only the
//! top-level version is pinned, ~170 transitive packages float, and
//! their install scripts run — so it is off unless [`ALLOW_NPX_ENV`] is
//! set, and it announces itself on stderr when it fires.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Relative path of the Node executable inside `runtime/`.
const NODE_REL: &str = "node/bin/node";

/// Points at a staged `runtime/` tree, overriding the sibling lookup.
pub const RUNTIME_DIR_ENV: &str = "DATALIB_RUNTIME_DIR";

/// Set to `1` to let a tool that has no staged tree run through
/// `npx -y` from the live registry. A dev convenience, never a default.
pub const ALLOW_NPX_ENV: &str = "DATALIB_ALLOW_NPX";

/// Entry script of the `latchkey` npm package inside a staged tree (its
/// package.json `bin` target), equivalent to what `npx latchkey` execs.
pub const LATCHKEY_ENTRY_REL: &str = "node_modules/latchkey/dist/src/cli.js";

/// The ONE canonical latchkey version pin (see the qmd twin,
/// `DEFAULT_QMD_VERSION` in [`crate::qmd`]): used for the `npx` fallback
/// spec, as the key into the staged `runtime/latchkey/<version>/` tree,
/// and re-exported by `datalib_etl::latchkey`.
/// `scripts/stage_runtime.sh` greps this constant to decide
/// what to stage — keep the `LATCHKEY_VERSION` name and string-literal
/// shape.
pub const LATCHKEY_VERSION: &str = "3.11.0";

/// The latchkey invocation to show in user-facing instructions and
/// error messages: the app-bundled launcher when present (the
/// `latchkey` wrapper staged next to our binaries — see
/// `scripts/latchkey-wrapper.sh`), else the `npx` form.
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
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV) {
        let dir = PathBuf::from(dir);
        // An explicitly-set override that doesn't exist is a
        // misconfiguration; still just miss (the caller reports where it
        // looked) but keep the check so we never return a dangling root.
        return dir.is_dir().then_some(dir);
    }
    runtime_root_candidates()
        .into_iter()
        .find(|root| root.join(NODE_REL).is_file())
}

/// Where a staged tree is looked for when `$DATALIB_RUNTIME_DIR` is
/// unset: `runtime/` beside the running binary (the tarball layout), or
/// one level up (the .app's `Resources/{binaries,runtime}`).
fn runtime_root_candidates() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let Some(exe_dir) = exe.parent() else {
        return Vec::new();
    };
    let mut out = vec![exe_dir.join("runtime")];
    if let Some(up) = exe_dir.parent() {
        out.push(up.join("runtime"));
    }
    out
}

/// No staged tree holds `<kind>@<version>`, and the npx fallback is not
/// enabled. The message says where the lookup went and how to fix it,
/// because it is the first thing a tarball user sees when `runtime/`
/// did not come along with the binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRuntime {
    pub kind: String,
    pub version: String,
    pub looked_in: Vec<PathBuf>,
}

impl fmt::Display for MissingRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no bundled runtime for {}@{} — looked for `{}` under",
            self.kind, self.version, NODE_REL
        )?;
        if self.looked_in.is_empty() {
            write!(f, " (nowhere: the running binary's path is unknown)")?;
        }
        for dir in &self.looked_in {
            write!(f, " {}", dir.display())?;
        }
        write!(
            f,
            ". Ship the `runtime/` tree beside the binaries (the release \
             tarball and the .app carry it; `scripts/stage_runtime.sh` \
             builds one from a checkout), point {RUNTIME_DIR_ENV} at a \
             staged tree, or set {ALLOW_NPX_ENV}=1 to fetch {}@{} through \
             `npx -y` from the npm registry — unpinned below the top \
             level, install scripts on.",
            self.kind, self.version
        )
    }
}

impl std::error::Error for MissingRuntime {}

/// The one resolution every Node-based tool goes through: the staged
/// tree, else — only with [`ALLOW_NPX_ENV`] set — `npx -y <pkg_spec>`.
pub fn tool_command(
    kind: &str,
    version: &str,
    entry_rel: &str,
    pkg_spec: &str,
) -> Result<Command, MissingRuntime> {
    if let Some(cmd) = bundled_command(kind, version, entry_rel) {
        return Ok(cmd);
    }
    if !npx_allowed() {
        let looked_in = match std::env::var_os(RUNTIME_DIR_ENV) {
            Some(dir) => vec![PathBuf::from(dir)],
            None => runtime_root_candidates(),
        };
        return Err(MissingRuntime {
            kind: kind.to_string(),
            version: version.to_string(),
            looked_in,
        });
    }
    warn_npx_once(pkg_spec);
    Ok(npx_command(pkg_spec))
}

fn npx_allowed() -> bool {
    matches!(
        std::env::var(ALLOW_NPX_ENV).ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Once per process per package: the fallback must be visible in every
/// step log, and a `latchkey curl` fan-out would otherwise print it a
/// thousand times. Raw stderr because this crate links nothing (see
/// BUILD.bazel); a step's stderr is captured into its run log anyway.
#[allow(clippy::disallowed_macros)]
fn warn_npx_once(pkg_spec: &str) {
    static WARNED: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(Default::default);
    let mut warned = warned.lock().unwrap_or_else(|e| e.into_inner());
    if warned.iter().any(|w| w == pkg_spec) {
        return;
    }
    warned.push(pkg_spec.to_string());
    eprintln!(
        "WARNING: no bundled runtime for {pkg_spec}; running it through `npx -y` \
         because {ALLOW_NPX_ENV} is set. Its transitive packages are unpinned and \
         their install scripts run — not a configuration to ship."
    );
}

/// `latchkey` at the ONE pin, through [`tool_command`].
pub fn latchkey_command() -> Result<Command, MissingRuntime> {
    tool_command(
        "latchkey",
        LATCHKEY_VERSION,
        LATCHKEY_ENTRY_REL,
        &format!("latchkey@{LATCHKEY_VERSION}"),
    )
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

/// `npx -y <pkg_spec>`, unguarded: reach it through [`tool_command`].
/// Honors `$NPX_BIN` as a runtime override (handy outside bazel; bazel
/// actions rely on the pinned `PATH` from `.bazelrc` instead).
fn npx_command(pkg_spec: &str) -> Command {
    let npx = std::env::var_os("NPX_BIN").unwrap_or_else(|| "npx".into());
    let mut cmd = Command::new(&npx);
    if let Some(cache) = npx_cache_dir(&npx) {
        cmd.env("npm_config_cache", cache);
    }
    cmd.arg("-y").arg(pkg_spec);
    cmd
}

fn npx_cache_dir(npx: &OsStr) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(cache_dir_for(Path::new(&home), &node_abi(npx)?))
}

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

fn node_beside(npx: &OsStr) -> OsString {
    let dir = Path::new(npx)
        .parent()
        .filter(|d| !d.as_os_str().is_empty());
    match dir.map(|d| d.join("node")) {
        Some(node) if node.is_file() => node.into_os_string(),
        _ => "node".into(),
    }
}

pub fn display_command(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

pub fn is_bundled(cmd: &Command) -> bool {
    runtime_root().is_some_and(|root| Path::new(cmd.get_program()).starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end resolution against a synthetic staged tree, driven
    /// through `$DATALIB_RUNTIME_DIR`.
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
        unsafe { std::env::set_var(RUNTIME_DIR_ENV, &base) };

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

        // A miss with the fallback off is an error that names the
        // override directory, not a silent `npx`.
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var(ALLOW_NPX_ENV) };
        let err = tool_command(
            "latchkey",
            "9.9.9",
            "node_modules/latchkey/nope.js",
            "latchkey@9.9.9",
        )
        .expect_err("no tree and no opt-in must not spawn npx");
        assert_eq!(err.looked_in, vec![base.clone()]);
        let text = err.to_string();
        assert!(text.contains("latchkey@9.9.9"), "{text}");
        assert!(text.contains(ALLOW_NPX_ENV), "{text}");

        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var(RUNTIME_DIR_ENV) };
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
