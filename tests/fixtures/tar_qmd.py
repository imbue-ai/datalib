#!/usr/bin/env python3
"""Hermetic tar of a `rendered_md/` tree into `qmd.tar`.

Run by the `:ingested_tng` genrule after `frankweiler-sync` has produced
its raw outputs. We deliberately keep tar packaging out of the sync
binary so the pipeline operates at the layer of DB + qmd files; the
genrule is the Bazel-distribution boundary that wants archives.

Layout matches the previous in-binary tar: every entry is prefixed with
`qmd/<rel>` so callers can extract with `--strip-components=1` to land
the markdown tree at a root data directory. Determinism guarantees:
mtime / uid / gid / uname / gname zeroed, entries sorted.

Args (positional):
    1: path to the directory containing `rendered_md/`
    2: output path for qmd.tar
"""

from __future__ import annotations

import sys
import tarfile
from pathlib import Path


def main() -> int:
    src_root = Path(sys.argv[1]).resolve()
    out_tar = Path(sys.argv[2]).resolve()
    out_tar.parent.mkdir(parents=True, exist_ok=True)

    rendered = src_root / "rendered_md"
    entries: list[Path] = []
    if rendered.is_dir():
        entries = sorted(p for p in rendered.rglob("*") if p.is_file() or p.is_dir())

    with tarfile.open(out_tar, "w") as tf:
        # Include `qmd/rendered_md/` itself for completeness, matching
        # the previous tar's behavior.
        if rendered.is_dir():
            ti = tf.gettarinfo(str(rendered), arcname="qmd/rendered_md")
            ti.mtime = 0
            ti.uid = 0
            ti.gid = 0
            ti.uname = ""
            ti.gname = ""
            tf.addfile(ti)
        for p in entries:
            arcname = "qmd/rendered_md/" + str(p.relative_to(rendered))
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
