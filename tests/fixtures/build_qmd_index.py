#!/usr/bin/env python3
"""Driver invoked by the Bazel genrule that builds the TNG fixture's qmd
index — through `build_qmd_index.rs`, which drives the same
`datalib_qmd_indexer` library a sync does — and emits an overlay tar
containing the resulting SQLite index.

The INPUT is `qmd_md.tar` — markdown only — and not the full `qmd.tar`.
Bazel keys this action on the content of its inputs, and the embedder
reads nothing but `*.md`, so anything else in the archive is a byte that
can invalidate a ~90s embed without changing its output. See
`tar_qmd.py` for which files were doing exactly that.

The OUTPUT is an *overlay* on top of `qmd.tar`: it shares the same `qmd/`
staging prefix so the two tars layer cleanly. Extracting both with
`tar -x --strip-components=1` into a directory yields a complete root data
directory — markdown trees under `<root>/<stanza>/render_markdown/...` plus the
qmd index at `<root>/unified_index/qmd_index/qmd/index.sqlite`.

Why a script:
  1. The ingested fixture is a tar (`qmd_md.tar`) — we have to extract it to
     a real directory before qmd can walk it.
  2. The library writes the index under `<root>/unified_index/qmd_index/qmd/`,
     so we pull that tree back out as a tar overlay.
  3. Node and the qmd package are Bazel inputs, handed to the library as
     paths, so nothing here touches a registry or a host Node.
  4. The embedding model is a Bazel input as well, so this script stages
     a models directory holding it (see `_stage_models`) instead of
     pointing qmd at the host's shared `~/.cache/qmd/models`. That is
     what makes the action work on a machine that has never run qmd —
     every CI container — without downloading anything.

Args (positional):
    1: path to the build_qmd_index binary
    2: path to qmd_md.tar (the rendered markdown, and nothing else)
    3: output path for qmd-index.tar (Bazel-supplied overlay tar)
    4: path to the Node binary (@nodejs_host//:node_bin)
    5: path to the `@tobilu/qmd` package dir
    6: path to the embedding GGUF (//third-party/qmd_models:embeddinggemma)
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tarfile
from pathlib import Path


def _stage_models(work: Path, embed_model: Path) -> Path:
    """Build a qmd models dir holding the Bazel-supplied GGUF.

    A symlink, not a copy: the file is ~318 MB and lives in a read-only
    bazel output, which is exactly what we want to read it from.

    The link's NAME is the contract. node-llama-cpp decides a model is
    already present by looking for the filename it would itself have
    downloaded to (`hf_<owner>_<file>` — `findMatchingFilesInDirectory`
    in `resolveModelFile.js`), and when it doesn't find that name it
    quietly downloads its own copy instead of failing. So the name comes
    from `downloaded_file_path` in MODULE.bazel and is checked here
    rather than trusted: a silent re-download is the failure mode this
    whole arrangement exists to remove.
    """
    if not embed_model.name.startswith("hf_") or not embed_model.name.endswith(".gguf"):
        raise SystemExit(
            f"embedding model staged under an unexpected name: {embed_model.name}\n"
            "node-llama-cpp will not recognize it as cached and will download "
            "its own copy. Fix `downloaded_file_path` in MODULE.bazel."
        )
    models = work / "models"
    models.mkdir(parents=True, exist_ok=True)
    (models / embed_model.name).symlink_to(embed_model.resolve())
    return models


def main() -> int:
    build_bin, qmd_tar, out_tar = sys.argv[1:4]
    node_bin, qmd_pkg_dir, embed_model = (Path(p) for p in sys.argv[4:7])
    qmd_tar_path = Path(qmd_tar).resolve()
    out_tar_path = Path(out_tar).resolve()
    out_tar_path.parent.mkdir(parents=True, exist_ok=True)

    work = out_tar_path.parent / "qmd_work"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    models_dir = _stage_models(work, embed_model)

    # The tar is rooted at "qmd/<provider>/..." (see tar_qmd.py); strip
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

    groups = sorted(p.name for p in work.iterdir() if (p / "render_markdown").is_dir())
    cmd = [
        str(Path(build_bin).resolve()),
        str(work),
        str(node_bin.resolve()),
        str(qmd_pkg_dir.resolve()),
        str(models_dir),
        *groups,
    ]
    # HOME too: nothing should be reaching for a real one.
    r = subprocess.run(
        cmd, env={"PATH": "/usr/bin:/bin", "HOME": str(work)}, check=False
    )
    if r.returncode != 0:
        return r.returncode

    # The one index file, under the `qmd_index` step's tree (see
    # runtime::qmd).
    produced = work / "unified_index" / "qmd_index" / "qmd" / "index.sqlite"
    if not produced.exists():
        sys.stderr.write(f"build_qmd_index did not produce {produced}\n")
        return 1

    # Emit an overlay tar that layers onto qmd.tar: every entry is prefixed
    # with the `qmd/` staging dir so callers strip one component and land the
    # index at `<root>/unified_index/qmd_index/qmd/index.sqlite`. Skip the
    # `models` symlink — it points at a shared cache outside the data root.
    overlay_root = work / "unified_index" / "qmd_index"
    models_link = overlay_root / "qmd" / "models"

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
        # Include the `qmd/unified_index/qmd_index/` directory entry itself
        # for completeness.
        ti = tf.gettarinfo(str(overlay_root), arcname="qmd/unified_index/qmd_index")
        ti.mtime = 0
        ti.uid = 0
        ti.gid = 0
        ti.uname = ""
        ti.gname = ""
        tf.addfile(ti)
        for p in entries:
            arcname = "qmd/unified_index/qmd_index/" + str(p.relative_to(overlay_root))
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
