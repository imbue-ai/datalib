"""Run the qmd indexer over the rendered markdown tree at `<root>`.

Inline counterpart to `frankweiler/backend/qmd_indexer` (Rust). Shape:

  * Pin `XDG_CACHE_HOME` to `<root>/.frankweiler` so the index lands at
    `<root>/.frankweiler/qmd/index.sqlite` alongside the rest of the
    backend's state.
  * Symlink `<root>/.frankweiler/qmd/models -> <models_dir>` so qmd's
    ~300MB embedding model isn't duplicated inside every data root.
  * Drive qmd via `npx -y @tobilu/qmd@<version>`: `collection add` on
    first run only (it errors on duplicate names), then `update` and
    `embed` every run.

Incremental: we do *not* wipe the index dir between runs. `qmd update`
keys on a per-file content hash (`(collection, path, hash)` in the
`documents` table), so unchanged files are skipped; missing files are
deactivated; only new/changed files get rechunked. `qmd embed` similarly
only embeds content hashes that don't yet have vectors. Both stream
live progress bars to stderr; we pipe stdin/stdout/stderr through so
the user sees them.

Bazel still uses the Rust binary (non-incremental by design — fixture
builds want a clean rebuild). Keep the two roughly in sync.
"""

from __future__ import annotations

import logging
import shutil
import subprocess
from pathlib import Path

DEFAULT_QMD_VERSION = "2.1.0"
DEFAULT_COLLECTION_NAME = "mirror"
DEFAULT_MASK = "**/*.qmd"

logger = logging.getLogger(__name__)


def _default_models_dir() -> Path:
    return Path.home() / ".cache" / "qmd-models"


def build_qmd_index(
    root: Path,
    *,
    qmd_version: str = DEFAULT_QMD_VERSION,
    collection_name: str = DEFAULT_COLLECTION_NAME,
    mask: str = DEFAULT_MASK,
    models_dir: Path | None = None,
    embed: bool = True,
) -> Path:
    """(Re)build the qmd index for `root`. Returns the index.sqlite path."""
    if shutil.which("npx") is None:
        raise RuntimeError(
            "npx not found on PATH — install Node.js to run the qmd indexer "
            "(or pass --no-qmd-index to skip)"
        )

    root = root.resolve(strict=True)
    cache_home = root / ".frankweiler"
    qmd_dir = cache_home / "qmd"
    qmd_dir.mkdir(parents=True, exist_ok=True)

    models_link = qmd_dir / "models"
    md = (models_dir or _default_models_dir()).expanduser()
    md.mkdir(parents=True, exist_ok=True)
    if not models_link.exists() and not models_link.is_symlink():
        models_link.symlink_to(md)

    index = qmd_dir / "index.sqlite"
    first_run = not index.exists()

    pkg = f"@tobilu/qmd@{qmd_version}"
    logger.info(
        "qmd-indexer: root=%s pkg=%s embed=%s first_run=%s",
        root,
        pkg,
        embed,
        first_run,
    )

    def _run(args: list[str]) -> None:
        cmd = ["npx", "-y", pkg, *args]
        logger.info("qmd-indexer: $ %s", " ".join(cmd))
        proc = subprocess.run(cmd, env=_env_with_cache(cache_home), check=False)
        if proc.returncode != 0:
            raise RuntimeError(f"qmd {args!r} failed (rc={proc.returncode})")

    if first_run:
        _run(
            [
                "collection",
                "add",
                str(root),
                "--name",
                collection_name,
                "--mask",
                mask,
            ]
        )
    _run(["update"])
    if embed:
        _run(["embed"])

    index = qmd_dir / "index.sqlite"
    if not index.exists():
        raise RuntimeError(f"qmd reported success but {index} is missing")
    return index


def _env_with_cache(cache_home: Path) -> dict[str, str]:
    import os

    env = os.environ.copy()
    env["XDG_CACHE_HOME"] = str(cache_home)
    return env
