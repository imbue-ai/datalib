#!/usr/bin/env python3
"""Syncs one TNG source on its own: its ingest, its render, and the grid
index over it alone, through `datalib-dag`. The driver behind
`tng_source` (`tng_source.bzl`): a source synced this way is checked
without waiting on, or adding to, the `:ingested_tng` fan-in.

Usage:
    sync_one_source.py --dag <datalib-dag> --step <datalib-step>
        --now <iso stamp> --workspace <dir> --id <group id> --type <type>
        --ingest-params '<TOML inline table>' [--input <name>=<path> ...]
        --index-out <grid index file>

`--ingest-params` is the ingest step's `params`, as TOML; each `@<name>@`
in it is replaced by the absolute path `--input <name>=<path>` gives,
quoted. The source must leave at least one row in the index, or the
build fails rather than cache an empty fixture.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import sys
from pathlib import Path

# A py_test's interpreter runs with safe-path on, which leaves this
# script's own directory off `sys.path`.
sys.path.insert(0, str(Path(__file__).parent))
from sync_drivers import PROGRAM_AND_LOCKS, CliDriver


def config_text(source_id: str, type_str: str, ingest_params: str) -> str:
    run = f"datalib-step run {{}} {type_str}"
    return f"""{PROGRAM_AND_LOCKS}

[[groups]]
id = "{source_id}"
type = "{type_str}"

[[steps]]
id = "{source_id}/ingest"
command = "{run.format("ingest")}"
params = {ingest_params}

[[steps]]
id = "{source_id}/render_markdown"
command = "{run.format("render_markdown")}"
inputs = ["{source_id}/ingest"]
reads = "pinned"

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "datalib-step run unified_grid_and_search_index"
reads = "pinned"
locks = ["index"]
inputs = ["*/render_markdown"]
"""


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--dag", type=Path, required=True)
    p.add_argument("--step", type=Path, required=True)
    p.add_argument("--now", required=True)
    p.add_argument("--workspace", type=Path, required=True)
    p.add_argument("--id", required=True)
    p.add_argument("--type", required=True)
    p.add_argument("--ingest-params", required=True)
    p.add_argument("--input", action="append", default=[])
    p.add_argument("--index-out", type=Path, required=True)
    args = p.parse_args()

    params = args.ingest_params
    for given in args.input:
        name, path = given.split("=", 1)
        params = params.replace(f"@{name}@", json.dumps(str(Path(path).resolve())))

    workspace = args.workspace.resolve()
    shutil.rmtree(workspace, ignore_errors=True)
    workspace.mkdir(parents=True)
    bindir = workspace / "bindir"
    bindir.mkdir()
    (bindir / "datalib-step").symlink_to(args.step.resolve())
    env = {
        **os.environ,
        # The host's fingerprint cache is keyed by absolute path, and
        # every path here is a sandbox gone by the next run.
        "DATALIB_CACHE_DIR": str(workspace / "fingerprint_cache"),
        "DATALIB_MODELS_NO_FETCH": "1",
    }
    driver = CliDriver(args.dag.resolve(), workspace, bindir, args.now, env)
    driver.build_config([config_text(args.id, args.type, params)])
    driver.sync()

    index = (
        workspace / "unified_grid_and_search_index" / "unified_grid_and_search_index"
    ) / "db.sqlite"
    # A copy through SQLite's backup, so whatever the index left in its
    # write-ahead log comes along, in one file a test opens read-only.
    args.index_out.unlink(missing_ok=True)
    with sqlite3.connect(f"file:{index}?mode=ro", uri=True) as src:
        (rows,) = src.execute(
            "SELECT COUNT(*) FROM grid_rows WHERE source_id = ?", (args.id,)
        ).fetchone()
        if rows == 0:
            print(f"[sync_one_source] {args.id} left no rows in the index", flush=True)
            return 1
        out = sqlite3.connect(args.index_out)
        src.backup(out)
        # The copy keeps the index's journal mode, and a write-ahead log
        # cannot be read where its directory is read-only.
        out.execute("PRAGMA journal_mode=DELETE")
        out.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
