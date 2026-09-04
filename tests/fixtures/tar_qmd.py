#!/usr/bin/env python3
"""Hermetic tars of the per-stanza `rendered_md/` trees.

Run by the `:ingested_tng` genrule after the pipeline has produced its
raw outputs. We deliberately keep tar packaging out of the sync binary
so the pipeline operates at the layer of DB + qmd files; the genrule is
the Bazel-distribution boundary that wants archives.

Layout: `data_root` holds one dir per source stanza (`<stanza>/rendered_md/…`)
plus the reserved `system/` and `unified_index/` dirs. We tar every stanza's
`rendered_md` subtree, each entry prefixed with `qmd/<stanza>/rendered_md/<rel>`,
so callers can extract with `--strip-components=1` to land
`<stanza>/rendered_md/…` at a root data directory.

TWO archives come out, and the split is a build-cache decision:

  * `qmd.tar` — the whole rendered tree. Markdown, the `*.grid_rows.json`
    sidecars, the `_render_cursor.json` bookkeeping files, and the
    attachment blobs (images, audio, PDFs). This is what
    `materialize_tng_root.sh` extracts to build a data root you can
    actually browse.

  * `qmd_md.tar` — markdown only, matching the mask the qmd indexer scans
    with (`datalib_qmd_indexer::DEFAULT_MASK` = `*/rendered_md/**/*.md`).
    This is the ONLY input to the `:ingested_tng_qmd` embedding action.

Why the second archive exists: bazel keys an action on the content of
its inputs, so any byte that can change without changing the action's
OUTPUT is pure cache poison. The embedder opens nothing but `*.md`, and
two of the excluded kinds change on literally every pipeline run —
`_render_cursor.json` carries a wall-clock `last_render_at`, and the pdf
provider's `*.grid_rows.json` carries a `source_url` holding the
absolute bazel sandbox path (…/darwin-sandbox/4914/… vs …/5269/…). With
those in the archive the ~90s CPU-only embed on CI re-ran for every
change anywhere upstream, including changes that left all 57 markdown
files byte-identical.

Determinism guarantees for both archives: mtime / uid / gid / uname /
gname zeroed, entries sorted.

Args (positional):
    1: path to the data root (containing `<stanza>/rendered_md/`)
    2: output path for qmd.tar (the whole rendered tree)
    3: output path for qmd_md.tar (markdown only)
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


def _write_tar(out_tar: Path, src_root: Path, entries: list[Path]) -> None:
    out_tar.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(out_tar, "w") as tf:
        for p in entries:
            _add(tf, p, "qmd/" + str(p.relative_to(src_root)))


def main() -> int:
    src_root = Path(sys.argv[1]).resolve()
    out_tar = Path(sys.argv[2]).resolve()
    out_md_tar = Path(sys.argv[3]).resolve()

    # Every top-level dir that is not owned by the app itself is a source
    # stanza with a `rendered_md/` subtree. Tar them all, rooted at
    # `qmd/<stanza>/...`. `system/` is the server's own state and
    # `unified_index/` is the index the steps write, so neither is a
    # stanza even though both sit at the same level.
    not_stanzas = {"system", "unified_index"}
    rendered_dirs = sorted(
        d
        for d in src_root.glob("*/rendered_md")
        if d.is_dir() and d.parent.name not in not_stanzas
    )

    all_entries: list[Path] = []
    md_entries: set[Path] = set()
    for rendered in rendered_dirs:
        children = sorted(p for p in rendered.rglob("*") if p.is_file() or p.is_dir())
        all_entries.extend([rendered] + children)
        for p in children:
            if p.is_file() and p.suffix == ".md":
                md_entries.add(p)
                # Carry the ancestor directories so the archive stands on
                # its own rather than relying on the extractor to create
                # parents implicitly.
                for parent in p.parents:
                    if parent == src_root:
                        break
                    md_entries.add(parent)

    _write_tar(out_tar, src_root, all_entries)
    _write_tar(out_md_tar, src_root, sorted(md_entries))
    return 0


if __name__ == "__main__":
    sys.exit(main())
