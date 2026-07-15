//! Locate the Node runtime + npm package trees bundled with the Tauri
//! app, so `latchkey` and `qmd` run without any Node/npm on the host.
//!
//! `frankweiler/tauri/stage-runtime.sh` stages (and the app bundles
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
//!   1. `$FRANKWEILER_RUNTIME_DIR` — explicit override; tests, dev runs,
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

use std::path::{Path, PathBuf};
use std::process::Command;

/// Relative path of the Node executable inside `runtime/`.
const NODE_REL: &str = "node/bin/node";

/// Resolve the staged `runtime/` root, or `None` when not bundled.
pub fn runtime_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FRANKWEILER_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        // An explicitly-set override that doesn't exist is a
        // misconfiguration; still just miss (callers fall back to npx)
        // but keep the check so we never return a dangling root.
        return dir.is_dir().then_some(dir);
    }
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let exe_dir = exe.parent()?;
    for root in [exe_dir.parent()?.join("runtime"), exe_dir.join("runtime")] {
        if root.join(NODE_REL).is_file() {
            return Some(root);
        }
    }
    None
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
pub fn npx_command(pkg_spec: &str) -> Command {
    let npx = std::env::var_os("NPX_BIN").unwrap_or_else(|| "npx".into());
    let mut cmd = Command::new(npx);
    cmd.arg("-y").arg(pkg_spec);
    cmd
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
    /// through `$FRANKWEILER_RUNTIME_DIR`.
    ///
    /// One test body covers hit + both miss shapes (missing entry,
    /// missing version) because they share the env var, and Rust tests
    /// in one crate share a process — splitting them would race on
    /// `set_var` (same pattern as qmd_indexer's env tests).
    #[test]
    fn bundled_command_resolves_staged_tree() {
        let base = std::env::temp_dir().join(format!("fw-runtime-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = base.join(NODE_REL);
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, b"#!/bin/sh\n").unwrap();
        let entry = base.join("latchkey/1.2.3/node_modules/latchkey/dist/src/cli.js");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, b"// cli\n").unwrap();

        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("FRANKWEILER_RUNTIME_DIR", &base) };

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
        unsafe { std::env::remove_var("FRANKWEILER_RUNTIME_DIR") };
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn npx_command_honors_npx_bin() {
        // Default program is `npx` (don't set NPX_BIN here — the other
        // test owns FRANKWEILER_RUNTIME_DIR; this one only reads).
        let cmd = npx_command("latchkey@1.2.3");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec!["-y", "latchkey@1.2.3"]);
    }
}
