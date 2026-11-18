#!/usr/bin/env python3
"""Hermetic tars of the per-stanza `render_markdown/` trees.

Run by the `:ingested_tng` genrule after the pipeline has produced its
raw outputs. We deliberately keep tar packaging out of the sync binary
so the pipeline operates at the layer of DB + rendered files; the genrule is
the Bazel-distribution boundary that wants archives.

Layout: `data_root` holds one dir per source stanza (`<stanza>/render_markdown/…`)
plus the reserved `system/` and `unified_grid_and_search_index/` dirs. We tar every stanza's
`render_markdown` subtree, each entry prefixed with `root/<stanza>/render_markdown/<rel>`,
so callers can extract with `--strip-components=1` to land
`<stanza>/render_markdown/…` at a root data directory.

TWO archives come out, and the split is a build-cache decision:

  * `rendered.tar` — the whole rendered tree: each source's
    `indexed_markdown.doltlite_db` (which carries its render cursor and
    every document), and each source's blob store
    (`<stanza>/ingest/blobs.sqlite`), which a page links its attachments
    in as `/data/<stanza>/ingest/blob/<hash>`. This is what
    `materialize_tng_root.sh` extracts to build a data root you can
    actually browse.

  * `rendered_md.tar` — markdown only: every document as `datalib-step
    markdown --out` exports it, at `root/<group>/<id>.md`. The UI's
    render preview reads it. Bazel keys an action on the content of its
    inputs, and the stores change on every pipeline run (each
    `indexed_markdown.doltlite_db` chains its commit hashes off a
    wall-clock initial commit, and the pdf provider's rows carry the
    absolute sandbox path), so an action that reads only markdown takes
    this archive and is not re-run for nothing.

A third, `time_series.tar`, holds the time-series stores: each
source's `<stanza>/render_time_series/` and the index over them,
`unified_time_series_index/time_series_index/`. The index names each
store by its path under the data root and the commit it read, so the
stores extract next to it unchanged.

Determinism guarantees for every archive: mtime / uid / gid / uname /
gname zeroed, entries sorted.

Args (positional):
    1: path to the data root (containing `<stanza>/render_markdown/`)
    2: output path for rendered.tar (the whole rendered tree)
    3: output path for rendered_md.tar (markdown only)
    4: output path for time_series.tar (the time-series stores)
    5: the folder `datalib-step markdown --out` exported the documents to
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
            _add(tf, p, "root/" + str(p.relative_to(src_root)))


def main() -> int:
    src_root = Path(sys.argv[1]).resolve()
    out_tar = Path(sys.argv[2]).resolve()
    out_md_tar = Path(sys.argv[3]).resolve()

    # Every top-level dir that is not owned by the app itself is a source
    # stanza with a `render_markdown/` subtree. Tar them all, rooted at
    # `root/<stanza>/...`. `system/` is the server's own state and
    # `unified_grid_and_search_index/` is the index the steps write, so neither is a
    # stanza even though both sit at the same level.
    not_stanzas = {"system", "unified_grid_and_search_index"}
    rendered_dirs = sorted(
        d
        for d in src_root.glob("*/render_markdown")
        if d.is_dir() and d.parent.name not in not_stanzas
    )

    all_entries: list[Path] = []
    for rendered in rendered_dirs:
        children = sorted(p for p in rendered.rglob("*") if p.is_file() or p.is_dir())
        all_entries.extend([rendered] + children)
        blob_store = rendered.parent / "ingest" / "blobs.sqlite"
        if blob_store.is_file():
            all_entries.extend([blob_store.parent, blob_store])
    _write_tar(out_tar, src_root, all_entries)

    # The folders come along so the archive stands on its own rather than
    # relying on the extractor to create parents implicitly.
    exported = Path(sys.argv[5]).resolve()
    _write_tar(out_md_tar, exported, sorted(exported.rglob("*")))

    series_dirs = sorted(src_root.glob("*/render_time_series")) + sorted(
        src_root.glob("unified_time_series_index/time_series_index")
    )
    series_entries: list[Path] = []
    for d in series_dirs:
        series_entries.extend([d.parent, d] + sorted(d.rglob("*")))
    _write_tar(Path(sys.argv[4]).resolve(), src_root, sorted(set(series_entries)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
