"""The fixture's search index: `:ingested_tng`'s rendered tree and grid
index, with each source's `keyword_index` step run over them by the real
`datalib-step`, as a sync runs it.

Writes the grid index with the search index's tables in it, and every
chunk's prompt (`search_prompts`), which `:ingested_tng_vectors` embeds.
The embedding is its own action so that Bazel reruns it only when a
prompt changes: this one's inputs carry wall-clock stamps and change on
every run (tar_qmd.py).

Usage: build_search_index.py <datalib-step> <search_prompts> <qmd.tar>
       <backend_index.sqlite> <out grid> <out prompts>
"""

import os
import shutil
import subprocess
import sys
import sqlite3
import tempfile
from pathlib import Path


def main() -> int:
    step, prompts_bin, qmd_tar, grid, out_grid, out_prompts = sys.argv[1:]
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "root"
        root.mkdir()
        subprocess.run(
            ["tar", "-xf", qmd_tar, "-C", str(root), "--strip-components=1"],
            check=True,
        )
        db = root / "unified_document_index" / "grid_index" / "db.sqlite"
        db.parent.mkdir(parents=True)
        shutil.copyfile(grid, db)
        os.chmod(db, 0o644)
        groups = sorted(
            p.name for p in root.iterdir() if (p / "render_markdown").is_dir()
        )
        for group in groups:
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "DATALIB_DAG_DATA_ROOT": str(root),
                "DATALIB_DAG_GROUP": group,
                "DATALIB_DAG_FUNCTION": "keyword_index",
                "DATALIB_DAG_STEP": f"{group}/keyword_index",
                "DATALIB_DAG_INPUTS": f"{group}/render_markdown",
            }
            subprocess.run([step], env=env, check=True)
        subprocess.run([prompts_bin, str(root), out_prompts], check=True)
        # One file, no write-ahead log beside it.
        with sqlite3.connect(db) as conn:
            conn.execute("PRAGMA journal_mode=DELETE")
        shutil.copyfile(db, out_grid)
    print(f"[build_search_index] {len(groups)} sources indexed", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
