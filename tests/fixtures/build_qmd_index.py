#!/usr/bin/env python3
"""Driver invoked by the Bazel genrule that runs the qmd indexer against the
TNG fixture's rendered markdown tree and emits an overlay tar containing the
resulting SQLite index.

The output is an *overlay* on top of `qmd.tar`: it shares the same `qmd/`
staging prefix so the two tars layer cleanly. Extracting both with
`tar -x --strip-components=1` into a directory yields a complete root data
directory — markdown trees under `<root>/<stanza>/rendered_md/...` plus the
qmd index at `<root>/unified_index/qmd/index.sqlite`.

Why a script:
  1. The ingested fixture is a tar (`qmd.tar`) — we have to extract it to a
     real directory before qmd's `collection add` can walk it.
  2. qmd writes its index under `$XDG_CACHE_HOME/qmd/index.sqlite`. The
     indexer binary pins XDG_CACHE_HOME at the data root, so we pull
     `qmd/` back out as a tar overlay.
  3. qmd used to be invoked via `npx -y @tobilu/qmd@<version>`, which
     resolved the whole package tree from the live npm registry on every
     cache miss, with no lockfile and no integrity checking, and ran every
     package's install scripts. We now stage a `DATALIB_RUNTIME_DIR` tree
     from Bazel-managed inputs instead (see `_stage_runtime`), which
     `datalib_core::node_runtime::bundled_command` picks up in preference
     to npx. Nothing here touches a registry.

Args (positional):
    1: path to the qmd_indexer rust_binary
    2: path to qmd.tar (the rendered markdown archive)
    3: output path for qmd-index.tar (Bazel-supplied overlay tar)
    4: qmd npm package version to pin (e.g. "2.1.0")
    5: path to the Node binary (@nodejs_host//:node_bin)
    6: path to the linked `@tobilu/qmd` package dir, used to locate the
       root of the pnpm store it lives in
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path


def _stage_runtime(
    work: Path, qmd_version: str, node_bin: Path, qmd_pkg_dir: Path
) -> Path:
    """Build a `DATALIB_RUNTIME_DIR` tree and return its root.

    Layout is the one `datalib_core::node_runtime` resolves (and that
    `datalib/tauri/stage-runtime.sh` produces for the packaged app):

        runtime/node/bin/node
        runtime/qmd/<version>/node_modules/@tobilu/qmd/dist/cli/qmd.js

    Two symlinks, no copying. That works only because the package store
    is already complete: better-sqlite3's native binding is baked into
    the package by `npm.npm_replace_package` in MODULE.bazel, so nothing
    here has to write into a read-only build output.
    """
    runtime = work / "runtime"

    node_dir = runtime / "node" / "bin"
    node_dir.mkdir(parents=True, exist_ok=True)
    (node_dir / "node").symlink_to(node_bin.resolve())

    # `$(execpath)` on the link target points INSIDE the pnpm virtual
    # store (`<root>/node_modules/.aspect_rules_js/@tobilu+qmd@<v>/node_modules/@tobilu/qmd`),
    # so cut at the FIRST `/node_modules/` to get the store root rather
    # than qmd's own dependency directory.
    store = Path(str(qmd_pkg_dir).split("/node_modules/")[0]) / "node_modules"
    staged = runtime / "qmd" / qmd_version
    staged.mkdir(parents=True, exist_ok=True)
    (staged / "node_modules").symlink_to(store.resolve())

    return runtime


def main() -> int:
    indexer, qmd_tar, out_tar, qmd_version = sys.argv[1:5]
    node_bin, qmd_pkg_dir = (Path(p) for p in sys.argv[5:7])
    qmd_tar_path = Path(qmd_tar).resolve()
    out_tar_path = Path(out_tar).resolve()
    out_tar_path.parent.mkdir(parents=True, exist_ok=True)

    # Capture the host user's $HOME *before* we scramble it for the
    # subprocess, so the qmd embedding model lands in a shared, persistent
    # cache instead of being re-downloaded into the sandbox each run.
    host_home = Path(
        os.environ.get("CLAUDE_MIRROR_HOST_HOME") or os.path.expanduser("~")
    )
    models_dir = host_home / ".cache" / "qmd" / "models"

    work = out_tar_path.parent / "qmd_work"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)

    # The tar is rooted at "qmd/<provider>/..." (see build_ingested.py); strip
    # that leading dir so `root` is the rendered markdown tree directly.
    with tarfile.open(qmd_tar_path, "r") as tf:
        for member in tf.getmembers():
            if not member.name.startswith("qmd/"):
                continue
            rel = member.name[len("qmd/") :]
            if not rel:
                continue
            member.name = rel
            tf.extract(member, work)

    env = os.environ.copy()
    env["HOME"] = str(work)  # nothing should be reaching for a real home
    # Point the indexer at the Bazel-staged Node + qmd tree. With this
    # set, `qmd_command()` resolves via `bundled_command` and the
    # `npx -y @tobilu/qmd@<v>` fallback is never reached — so the build
    # no longer needs `npx` (or any host Node) on PATH.
    env["DATALIB_RUNTIME_DIR"] = str(
        _stage_runtime(work, qmd_version, node_bin, qmd_pkg_dir)
    )

    cmd = [
        indexer,
        "--root",
        str(work),
        "--qmd-version",
        qmd_version,
        "--models-dir",
        str(models_dir),
    ]
    r = subprocess.run(cmd, env=env, check=False)
    if r.returncode != 0:
        return r.returncode

    # The indexer pins XDG_CACHE_HOME at `<root>/unified_index`, so qmd
    # writes its index under `<root>/unified_index/qmd/` (see core::layout).
    produced = work / "unified_index" / "qmd" / "index.sqlite"
    if not produced.exists():
        sys.stderr.write(f"qmd_indexer did not produce {produced}\n")
        return 1

    # Emit an overlay tar that layers onto qmd.tar: every entry is prefixed
    # with the `qmd/` staging dir so callers strip one component and land the
    # index at `<root>/unified_index/qmd/index.sqlite`. Skip the `models` symlink —
    # it points at a shared cache outside the data root.
    overlay_root = work / "unified_index" / "qmd"
    models_link = overlay_root / "models"

    def is_under(p: Path, parent: Path) -> bool:
        try:
            p.relative_to(parent)
            return True
        except ValueError:
            return False

    entries: list[Path] = sorted(
        p
        for p in overlay_root.rglob("*")
        if (p.is_file() or p.is_dir())
        and p != models_link
        and not is_under(p, models_link)
    )
    with tarfile.open(out_tar_path, "w") as tf:
        # Include the `qmd/unified_index/qmd/` directory entry itself for completeness.
        ti = tf.gettarinfo(str(overlay_root), arcname="qmd/unified_index/qmd")
        ti.mtime = 0
        ti.uid = 0
        ti.gid = 0
        ti.uname = ""
        ti.gname = ""
        tf.addfile(ti)
        for p in entries:
            arcname = "qmd/unified_index/qmd/" + str(p.relative_to(overlay_root))
            ti = tf.gettarinfo(str(p), arcname=arcname)
            ti.mtime = 0
            ti.uid = 0
            ti.gid = 0
            ti.uname = ""
            ti.gname = ""
            if p.is_file():
                with open(p, "rb") as f:
                    tf.addfile(ti, f)
            else:
                tf.addfile(ti)
    return 0


if __name__ == "__main__":
    sys.exit(main())
