//! Locate the Node runtime + npm package trees `latchkey` and `qmd` run
//! from, so neither needs any Node/npm on the host. Three places, in
//! order: `$DATALIB_RUNTIME_DIR`; `runtime/` beside the binaries (the
//! docker image, and the .app one level up); and the runtime a release
//! tarball names in a manifest beside its binaries, which is fetched
//! into the user's cache on first use — see `docs/dev/runtime_fetch.md`.
//! The fetch itself lives in `datalib_fetch` (this crate has no
//! dependencies, on purpose); a binary that wants it installs a
//! [`RuntimeFetcher`] through [`enable_fetch`].
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

pub use crate::runtime_manifest::{AssetKind, Manifest, RuntimeAsset, MANIFEST_FILE};

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
pub const LATCHKEY_VERSION: &str = "3.14.0";

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

/// Resolve the runtime root, or `None` when there is none to run from.
pub fn runtime_root() -> Option<PathBuf> {
    resolve_root().ok()
}

/// Where the lookup went and why it missed, for the error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub looked_in: Vec<PathBuf>,
    pub fetch: FetchOutcome,
}

/// What the manifest-driven fetch had to say when the staged candidates
/// missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// `$DATALIB_RUNTIME_DIR` was set and is not a directory: the
    /// override is the whole answer.
    Overridden,
    /// A runtime was found, and it holds no `<kind>/<version>` tree
    /// with the entry script — a stale stage, or a pin that moved.
    NotInTree,
    /// No manifest beside the binary — a checkout build, or an
    /// installation that lost the file.
    NoManifest,
    /// The manifest could not be parsed.
    BadManifest(String),
    /// A manifest names a runtime, but this process installed no
    /// [`RuntimeFetcher`], so it was only looked for in the cache.
    NotEnabled { asset: String, cache: PathBuf },
    /// The fetcher ran and could not deliver.
    Failed { asset: String, reason: String },
}

fn resolve_root() -> Result<PathBuf, Miss> {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV) {
        let dir = PathBuf::from(dir);
        // An explicitly-set override that doesn't exist is a
        // misconfiguration; still just miss (the caller reports where it
        // looked) but keep the check so we never return a dangling root.
        return if dir.is_dir() {
            Ok(dir)
        } else {
            Err(Miss {
                looked_in: vec![dir],
                fetch: FetchOutcome::Overridden,
            })
        };
    }
    let looked_in = runtime_root_candidates();
    if let Some(root) = looked_in.iter().find(|root| root.join(NODE_REL).is_file()) {
        return Ok(root.clone());
    }
    let mut looked_in = looked_in;
    match fetched_root() {
        Ok(root) => Ok(root.clone()),
        Err(outcome) => {
            if let FetchOutcome::NotEnabled { cache, .. } = &outcome {
                looked_in.push(cache.clone());
            }
            Err(Miss {
                looked_in,
                fetch: outcome.clone(),
            })
        }
    }
}

/// Where a staged tree is looked for when `$DATALIB_RUNTIME_DIR` is
/// unset: `runtime/` beside the running binary (the tarball layout), or
/// one level up (the .app's `Resources/{binaries,runtime}`).
fn runtime_root_candidates() -> Vec<PathBuf> {
    let Some(exe_dir) = exe_dir() else {
        return Vec::new();
    };
    let mut out = vec![exe_dir.join("runtime")];
    if let Some(up) = exe_dir.parent() {
        out.push(up.join("runtime"));
    }
    out
}

fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// The manifest beside the running binary, read once per process. A
/// release tarball carries one; a checkout build and the .app do not.
pub fn manifest() -> Result<&'static Manifest, FetchOutcome> {
    static MANIFEST: OnceLock<Result<Manifest, FetchOutcome>> = OnceLock::new();
    MANIFEST
        .get_or_init(|| {
            let path = exe_dir()
                .map(|d| d.join(MANIFEST_FILE))
                .ok_or(FetchOutcome::NoManifest)?;
            let text = std::fs::read_to_string(&path).map_err(|_| FetchOutcome::NoManifest)?;
            Manifest::parse(&text)
                .map_err(|e| FetchOutcome::BadManifest(format!("{}: {e}", path.display())))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Where fetched runtimes live: `$XDG_CACHE_HOME/datalib/runtime`, else
/// `~/.cache/datalib/runtime` — beside qmd's model cache, and the same
/// on every platform. A cache directory rather than the data root so
/// two roots on one machine share one copy and the `latchkey` launcher
/// can find it with no data root in hand.
pub fn runtime_cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("datalib").join("runtime"))
}

/// The tree a manifest's CPU runtime unpacks into: named by the asset's
/// sha256, so a directory that exists is one that was fetched, verified
/// and unpacked whole (the fetcher renames it into place last), and a
/// new release lands beside the old one rather than over it.
pub fn fetched_runtime_dir(cache_dir: &Path, asset: &RuntimeAsset) -> PathBuf {
    cache_dir.join(asset.dir_name())
}

/// Puts the runtime a manifest names in place and returns its root.
/// Implemented by `datalib_fetch`; this crate only knows the shape.
pub trait RuntimeFetcher: Send + Sync {
    fn fetch(&self, manifest: &Manifest, cache_dir: &Path) -> Result<PathBuf, String>;
}

static FETCHER: OnceLock<Box<dyn RuntimeFetcher>> = OnceLock::new();

/// Let a miss fetch. Called once, early, by the binaries that run
/// `qmd` or `latchkey` from a release tarball; a second call is
/// ignored. Must precede the first resolution: the result is cached.
pub fn enable_fetch(fetcher: Box<dyn RuntimeFetcher>) {
    let _ = FETCHER.set(fetcher);
}

/// The manifest's runtime, resolved at most once per process: a present
/// tree costs a stat, a missing one costs the fetch, and a fetch that
/// failed is not retried by the next `latchkey curl` in the same run.
fn fetched_root() -> Result<&'static PathBuf, FetchOutcome> {
    static FETCHED: OnceLock<Result<PathBuf, FetchOutcome>> = OnceLock::new();
    FETCHED
        .get_or_init(|| {
            let manifest = manifest()?;
            let cache = runtime_cache_dir().ok_or_else(|| FetchOutcome::Failed {
                asset: manifest.cpu().name.clone(),
                reason: "neither $XDG_CACHE_HOME nor $HOME is set, so there is no cache \
                         directory to fetch into"
                    .to_string(),
            })?;
            match FETCHER.get() {
                Some(fetcher) => {
                    fetcher
                        .fetch(manifest, &cache)
                        .map_err(|reason| FetchOutcome::Failed {
                            asset: manifest.cpu().name.clone(),
                            reason,
                        })
                }
                None => {
                    let dir = fetched_runtime_dir(&cache, manifest.cpu());
                    if dir.join(NODE_REL).is_file() {
                        Ok(dir)
                    } else {
                        Err(FetchOutcome::NotEnabled {
                            asset: manifest.cpu().name.clone(),
                            cache,
                        })
                    }
                }
            }
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// No staged tree holds `<kind>@<version>`, and the npx fallback is not
/// enabled. The message says where the lookup went and how to fix it,
/// because it is the first thing a tarball user sees when the runtime
/// could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRuntime {
    pub kind: &'static str,
    pub version: String,
    pub looked_in: Vec<PathBuf>,
    pub fetch: FetchOutcome,
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
        match &self.fetch {
            FetchOutcome::Overridden => {
                write!(f, " ({RUNTIME_DIR_ENV} is set and is not a directory)")?
            }
            FetchOutcome::NotInTree => write!(
                f,
                " (a runtime is there, but holds no {}/{} tree with its entry script)",
                self.kind, self.version
            )?,
            FetchOutcome::NoManifest => write!(
                f,
                ". No `{MANIFEST_FILE}` beside the binary, so nothing says which runtime to fetch"
            )?,
            FetchOutcome::BadManifest(e) => write!(f, ". The runtime manifest is unreadable: {e}")?,
            FetchOutcome::NotEnabled { asset, .. } => write!(
                f,
                ". The manifest names {asset}, but this program does not fetch; \
                 `datalib-step pull-runtime` does"
            )?,
            FetchOutcome::Failed { asset, reason } => {
                write!(f, ". Fetching {asset} failed: {reason}")?
            }
        }
        write!(
            f,
            ". Ship the `runtime/` tree beside the binaries (the .app and the \
             docker image carry it; `scripts/stage_runtime.sh` builds one from \
             a checkout), point {RUNTIME_DIR_ENV} at a staged tree, or set \
             {ALLOW_NPX_ENV}=1 to fetch {}@{} through `npx -y` from the npm \
             registry — unpinned below the top level, install scripts on.",
            self.kind, self.version
        )
    }
}

impl std::error::Error for MissingRuntime {}

/// The one resolution every Node-based tool goes through: the staged
/// tree, else — only with [`ALLOW_NPX_ENV`] set — `npx -y <pkg_spec>`.
pub fn tool_command(
    kind: &'static str,
    version: &str,
    entry_rel: &str,
    pkg_spec: &str,
) -> Result<Command, MissingRuntime> {
    let miss = match resolve_root() {
        Ok(root) => match command_in(&root, kind, version, entry_rel) {
            Some(cmd) => return Ok(cmd),
            None => Miss {
                looked_in: vec![root],
                fetch: FetchOutcome::NotInTree,
            },
        },
        Err(miss) => miss,
    };
    if !npx_allowed() {
        return Err(MissingRuntime {
            kind,
            version: version.to_string(),
            looked_in: miss.looked_in,
            fetch: miss.fetch,
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
    command_in(&runtime_root()?, kind, version, entry_rel)
}

fn command_in(root: &Path, kind: &str, version: &str, entry_rel: &str) -> Option<Command> {
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
        assert_eq!(err.fetch, FetchOutcome::NotInTree);
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
