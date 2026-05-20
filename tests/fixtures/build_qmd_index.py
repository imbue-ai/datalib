#!/usr/bin/env python3
"""Driver invoked by the Bazel genrule that runs the qmd indexer against the
TNG fixture's rendered markdown tree and emits an overlay tar containing the
resulting SQLite index.

The output is an *overlay* on top of `qmd.tar`: it shares the same `qmd/`
prefix so the two tars layer cleanly. Extracting both with
`tar -x --strip-components=1` into a directory yields a complete root data
directory — markdown tree under `<root>/rendered_md/...` plus the qmd index
at `<root>/qmd/index.sqlite`.

Why a script:
  1. The ingested fixture is a tar (`qmd.tar`) — we have to extract it to a
     real directory before qmd's `collection add` can walk it.
  2. qmd writes its index under `$XDG_CACHE_HOME/qmd/index.sqlite`. The
     indexer binary pins XDG_CACHE_HOME at the data root, so we pull
     `qmd/` back out as a tar overlay.
  3. qmd is invoked via `npx`, which needs `node` on PATH and writes to a
     per-user cache. Bazel scrubs PATH and HOME for hermeticity, so we
     re-add the common host install locations and point HOME at the sandbox.

Args (positional):
    1: path to the qmd_indexer rust_binary
    2: path to qmd.tar (the rendered markdown archive)
    3: output path for qmd-index.tar (Bazel-supplied overlay tar)
    4: qmd npm package version to pin (e.g. "2.1.0")
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path


def main() -> int:
    indexer, qmd_tar, out_tar, qmd_version = sys.argv[1:5]
    qmd_tar_path = Path(qmd_tar).resolve()
    out_tar_path = Path(out_tar).resolve()
    out_tar_path.parent.mkdir(parents=True, exist_ok=True)

    # Capture the host user's $HOME *before* we scramble it for the
    # subprocess, so the qmd embedding model lands in a shared, persistent
    # cache instead of being re-downloaded into the sandbox each run.
    host_home = Path(
        os.environ.get("CLAUDE_MIRROR_HOST_HOME") or os.path.expanduser("~")
    )
    models_dir = host_home / ".cache" / "qmd-models"

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
    env["HOME"] = str(work)  # isolate npm/npx cache to the sandbox
    extra_paths = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
    ]
    parent_path = os.environ.get("CLAUDE_MIRROR_HOST_PATH") or os.environ.get(
        "PATH", ""
    )
    env["PATH"] = ":".join([p for p in extra_paths + parent_path.split(":") if p])

    cmd = [
        indexer,
        "--root",
        str(work),
        "--qmd-version",
        qmd_version,
        "--models-dir",
        str(models_dir),
    ]
    r = subprocess.run(cmd, env=env)
    if r.returncode != 0:
        return r.returncode

    produced = work / "qmd" / "index.sqlite"
    if not produced.exists():
        sys.stderr.write(f"qmd_indexer did not produce {produced}\n")
        return 1

    # Emit an overlay tar mirroring qmd.tar's layout: paths rooted at
    # "qmd/..." so callers can extract both archives into the same root
    # directory and have everything end up in the right place. Skip the
    # `models` symlink — it points at a shared cache outside the data root.
    overlay_root = work / "qmd"
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
        # Include the `qmd/qmd/` directory entry itself for completeness.
        ti = tf.gettarinfo(str(overlay_root), arcname="qmd/qmd")
        ti.mtime = 0
        ti.uid = 0
        ti.gid = 0
        ti.uname = ""
        ti.gname = ""
        tf.addfile(ti)
        for p in entries:
            arcname = "qmd/qmd/" + str(p.relative_to(overlay_root))
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
