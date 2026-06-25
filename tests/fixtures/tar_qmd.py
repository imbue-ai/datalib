#!/usr/bin/env python3
"""Hermetic tar of the per-stanza `rendered_md/` trees into `qmd.tar`.

Run by the `:ingested_tng` genrule after `frankweiler-sync` has produced
its raw outputs. We deliberately keep tar packaging out of the sync
binary so the pipeline operates at the layer of DB + qmd files; the
genrule is the Bazel-distribution boundary that wants archives.

Layout: `data_root` holds one dir per source stanza (`<stanza>/rendered_md/…`)
plus the reserved `system/` dir. We tar every stanza's `rendered_md` subtree,
each entry prefixed with `qmd/<stanza>/rendered_md/<rel>`, so callers can
extract with `--strip-components=1` to land `<stanza>/rendered_md/…` at a root
data directory. `system/` is excluded (the aggregate indices live there, not
markdown). Determinism guarantees: mtime / uid / gid / uname / gname zeroed,
entries sorted.

Args (positional):
    1: path to the data root (containing `<stanza>/rendered_md/`)
    2: output path for qmd.tar
"""

from __future__ import annotations

import sys
import tarfile
from pathlib import Path


def _add(tf: tarfile.TarFile, path: Path, arcname: str) -> None:
    ti = tf.gettarinfo(str(path), arcname=arcname)
    ti.mtime = 0
    ti.uid = 0
    ti.gid = 0
    ti.uname = ""
    ti.gname = ""
    if path.is_file():
        with open(path, "rb") as f:
            tf.addfile(ti, f)
    else:
        tf.addfile(ti)


def main() -> int:
    src_root = Path(sys.argv[1]).resolve()
    out_tar = Path(sys.argv[2]).resolve()
    out_tar.parent.mkdir(parents=True, exist_ok=True)

    # Each top-level dir except `system/` is a source stanza with a
    # `rendered_md/` subtree. Tar them all, rooted at `qmd/<stanza>/...`.
    rendered_dirs = sorted(
        d
        for d in src_root.glob("*/rendered_md")
        if d.is_dir() and d.parent.name != "system"
    )

    with tarfile.open(out_tar, "w") as tf:
        for rendered in rendered_dirs:
            entries = [rendered] + sorted(
                p for p in rendered.rglob("*") if p.is_file() or p.is_dir()
            )
            for p in entries:
                _add(tf, p, "qmd/" + str(p.relative_to(src_root)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
