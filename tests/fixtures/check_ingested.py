#!/usr/bin/env python3
"""Refuse an `:ingested_tng` whose index is missing a source.

A sync can end without error and still leave a source out of the index,
and a bad fixture, once cached, fails every test downstream of it for a
reason unrelated to them. So before the genrule succeeds, every source
the fixture declares must have rows in `grid_rows` and a row in
`source_cursors`, and every source with a `render_time_series` step
must have time series in the time-series index.

Args (positional):
    1: the doltlite shell (stock sqlite3 cannot open the index)
    2: the grid index store, `backend_index.doltlite_db`
    3: `config_body.toml`, the fixture's sources as groups
    4: the time-series index store, `time_series.doltlite_db`
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path


def declared_sources(config_body: str) -> list[str]:
    """Every group that is a source: one with a `type`, diff groups too.

    Read by hand, not with `tomllib`: a genrule runs the host's python3,
    which can predate it. `run_sync_pipeline.py` writes every table as
    a header line, then its `key = value` lines."""
    sources = []
    for table in re.split(r"^\[", config_body, flags=re.MULTILINE):
        if not table.startswith("[groups]]"):
            continue
        keys = dict(re.findall(r'^(\w+) = "([^"]*)"$', table, flags=re.MULTILINE))
        if "type" in keys:
            sources.append(keys["id"])
    return sources


def time_series_sources(config_body: str) -> list[str]:
    """Every group with a `render_time_series` step, read as
    `declared_sources` reads groups."""
    out = []
    for table in re.split(r"^\[", config_body, flags=re.MULTILINE):
        if not table.startswith("[steps]]"):
            continue
        keys = dict(re.findall(r'^(\w+) = "([^"]*)"$', table, flags=re.MULTILINE))
        if keys.get("function") == "render_time_series":
            out.append(keys["group"])
    return out


def missing(declared: list[str], rows: dict[str, int], cursors: set[str]) -> list[str]:
    out = []
    for source in declared:
        lacks = []
        if not rows.get(source):
            lacks.append("no grid_rows")
        if source not in cursors:
            lacks.append("no source_cursors row")
        if lacks:
            out.append(f"{source}: {', '.join(lacks)}")
    return out


def _query(doltlite: Path, store: Path, sql: str) -> list[list[str]]:
    out = subprocess.run(
        [str(doltlite), "-readonly", str(store), sql],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [line.split("|") for line in out.splitlines() if line]


def main(argv: list[str]) -> int:
    doltlite, store, config_body = Path(argv[1]), Path(argv[2]), Path(argv[3])
    declared = declared_sources(config_body.read_text())
    rows = {
        source: int(n)
        for source, n in _query(
            doltlite, store, "SELECT source_id, count(*) FROM grid_rows GROUP BY 1"
        )
    }
    cursors = {
        r[0] for r in _query(doltlite, store, "SELECT source_id FROM source_cursors")
    }
    wrong = missing(declared, rows, cursors)
    series_store = Path(argv[4])
    with_series = time_series_sources(config_body.read_text())
    counted = (
        {
            source: int(n)
            for source, n in _query(
                doltlite,
                series_store,
                "SELECT source_id, count(*) FROM time_series GROUP BY 1",
            )
        }
        if series_store.exists()
        else {}
    )
    wrong += [f"{s}: no time series in the index" for s in with_series if not counted.get(s)]
    if wrong:
        print(
            f"{store.name} is missing {len(wrong)} of the {len(declared)} sources "
            f"{config_body.name} declares:\n  " + "\n  ".join(wrong),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
