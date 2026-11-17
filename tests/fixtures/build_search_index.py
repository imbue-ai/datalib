"""The fixture's search index: `:ingested_tng`'s rendered tree and grid
index, with the `grid_index` step run over them by the real
`datalib-step`, as a sync runs it: the grid is already loaded, so the
step's keyword pass over every source is what it writes.

Writes the grid index with the search index's tables in it, and every
chunk's prompt (`search_prompts`), which `:ingested_tng_vectors` embeds.
The embedding is its own action so that Bazel reruns it only when a
prompt changes: this one's inputs carry wall-clock stamps and change on
every run (tar_rendered.py).

Usage: build_search_index.py <datalib-step> <search_prompts> <model.gguf>
       <rendered.tar> <backend_index.sqlite> <out grid> <out prompts>
"""

import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    step, prompts_bin, model, rendered_tar, grid, out_grid, out_prompts = sys.argv[1:]
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "root"
        root.mkdir()
        subprocess.run(
            ["tar", "-xf", rendered_tar, "-C", str(root), "--strip-components=1"],
            check=True,
        )
        db = root / "unified_grid_and_search_index" / "grid_index" / "db.sqlite"
        db.parent.mkdir(parents=True)
        shutil.copyfile(grid, db)
        os.chmod(db, 0o644)
        # Where the steps look for the model, which they read the tokenizer
        # from to cut the chunks; fetching is off, so a missing one fails.
        models = root / "unified_grid_and_search_index" / "models"
        models.mkdir()
        os.symlink(
            os.path.abspath(model),
            models / "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
        )
        groups = sorted(
            p.name for p in root.iterdir() if (p / "render_markdown").is_dir()
        )
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "DATALIB_DAG_DATA_ROOT": str(root),
            "DATALIB_DAG_GROUP": "unified_grid_and_search_index",
            "DATALIB_DAG_FUNCTION": "grid_index",
            "DATALIB_DAG_STEP": "unified_grid_and_search_index/grid_index",
            "DATALIB_DAG_INPUTS": "\n".join(f"{g}/render_markdown" for g in groups),
            "DATALIB_MODELS_NO_FETCH": "1",
        }
        subprocess.run([step], env=env, check=True)
        subprocess.run([prompts_bin, str(root), model, out_prompts], check=True)
        # One file, no write-ahead log beside it.
        with sqlite3.connect(db) as conn:
            conn.execute("PRAGMA journal_mode=DELETE")
        shutil.copyfile(db, out_grid)
    print(f"[build_search_index] {len(groups)} sources indexed", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
