#!/usr/bin/env python3
"""Driver invoked by the Bazel genrule that builds the TNG fixture's qmd
index — each source's `keyword_index` and then `embed`, run through
`datalib-step` exactly as the runner runs them — and emits an overlay tar
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
  2. The steps write the index under `<root>/unified_index/qmd_index/qmd/`,
     so we pull that tree back out as a tar overlay.
  3. qmd used to be invoked via `npx -y @tobilu/qmd@<version>`, which
     resolved the whole package tree from the live npm registry on every
     cache miss, with no lockfile and no integrity checking, and ran every
     package's install scripts. We now stage a `DATALIB_RUNTIME_DIR` tree
     from Bazel-managed inputs instead (see `_stage_runtime`), which the
     steps resolve qmd from in preference to npx. Nothing here touches a
     registry.
  4. The embedding model is a Bazel input as well, so this script stages
     a models directory holding it (see `_stage_models`) instead of
     pointing qmd at the host's shared `~/.cache/qmd/models`. That is
     what makes the action work on a machine that has never run qmd —
     every CI container — without downloading anything.

Args (positional):
    1: path to the datalib-step binary
    2: path to qmd_md.tar (the rendered markdown, and nothing else)
    3: output path for qmd-index.tar (Bazel-supplied overlay tar)
    4: qmd npm package version to pin (e.g. "2.1.0")
    5: path to the Node binary (@nodejs_host//:node_bin)
    6: path to the linked `@tobilu/qmd` package dir, used to locate the
       root of the pnpm store it lives in
    7: path to the embedding GGUF (//third-party/qmd_models:embeddinggemma)
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
    step_bin, qmd_tar, out_tar, qmd_version = sys.argv[1:5]
    node_bin, qmd_pkg_dir, embed_model = (Path(p) for p in sys.argv[5:8])
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

    env = os.environ.copy()
    env["HOME"] = str(work)  # nothing should be reaching for a real home
    # Point the steps at the Bazel-staged Node + qmd tree, so qmd resolves
    # without `npx` (or any host Node) on PATH.
    env["DATALIB_RUNTIME_DIR"] = str(
        _stage_runtime(work, qmd_version, node_bin, qmd_pkg_dir)
    )
    # The embedding model is an input; a missing one must fail the action
    # rather than be downloaded.
    env["DATALIB_QMD_MODELS_NO_FETCH"] = "1"
    env["DATALIB_DAG_DATA_ROOT"] = str(work)

    groups = sorted(p.name for p in work.iterdir() if (p / "render_markdown").is_dir())
    for group in groups:
        for function, inputs in [
            ("keyword_index", [f"{group}/render_markdown", "unified_index/qmd_index"]),
            ("embed", [f"{group}/keyword_index"]),
        ]:
            step_env = dict(
                env,
                DATALIB_DAG_STEP=f"{group}/{function}",
                DATALIB_DAG_GROUP=group,
                DATALIB_DAG_FUNCTION=function,
                DATALIB_DAG_INPUTS="\n".join(inputs),
            )
            cmd = [str(Path(step_bin).resolve()), "--models-dir", str(models_dir)]
            r = subprocess.run(cmd, env=step_env, cwd=work, check=False)
            if r.returncode != 0:
                sys.stderr.write(f"{group}/{function} failed: exit {r.returncode}\n")
                return r.returncode

    # The steps write the one index file under the `qmd_index` step's
    # tree (see runtime::qmd).
    produced = work / "unified_index" / "qmd_index" / "qmd" / "index.sqlite"
    if not produced.exists():
        sys.stderr.write(f"the qmd steps did not produce {produced}\n")
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
