"""The TNG library syncs: what `materialize_tng_root.sh` lays down for
`dev_tng` and the Playwright suite is a library a person could press
Sync on.

Each source that reads files reads the library's own copy of them under
`inputs/`; every other source keeps what its raw store holds
(`common.download = false`), byte for byte. The first sync runs every
step, since the library has no record of having run anything, and
leaves the grid with the rows the fixture build gave it; the second
redoes nothing.
"""

from __future__ import annotations

import hashlib
import os
import sqlite3
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

# The fixture's own clock (tests/fixtures/README.md §"Conventions").
NOW = "2369-04-15T00:00:00+00:00"

MATERIALIZE: Path
BIN_DIR: Path


def outcomes_of(stdout: str) -> dict[str, str]:
    """`datalib-pipeline sync` prints one `<step> <outcome>` line per step."""
    out = {}
    for line in stdout.splitlines():
        step, sep, outcome = line.partition(" ")
        if sep:
            out[step] = outcome.strip()
    return out


class TngLibraryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name) / "library"
        cls.cache = Path(cls.tmp.name) / "fingerprint_cache"
        subprocess.run(
            [str(MATERIALIZE), str(cls.root)], check=True, stdout=subprocess.DEVNULL
        )
        cls.config = tomllib.loads((cls.root / "config.toml").read_text())
        cls.stores_before = cls.raw_stores()
        cls.rows_before = cls.grid_rows()
        cls.first = cls.sync()
        cls.stores_after_first = cls.raw_stores()
        cls.rows_after_first = cls.grid_rows()
        cls.second = cls.sync()
        cls.stores_after_second = cls.raw_stores()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.tmp.cleanup()

    @classmethod
    def sync(cls) -> dict[str, str]:
        env = {
            **os.environ,
            "PATH": f"{BIN_DIR}:{os.environ.get('PATH', '')}",
            "DATALIB_CACHE_DIR": str(cls.cache),
            "DATALIB_MODELS_NO_FETCH": "1",
        }
        proc = subprocess.run(
            [
                str(BIN_DIR / "datalib-pipeline"),
                "sync",
                str(cls.root),
                "--by",
                "tng_library_test",
                "--now",
                NOW,
            ],
            env=env,
            capture_output=True,
            check=False,
            text=True,
        )
        if proc.returncode != 0:
            raise AssertionError(
                f"sync exited {proc.returncode}:\n{proc.stdout}\n{proc.stderr[-4000:]}"
            )
        return outcomes_of(proc.stdout)

    @classmethod
    def raw_stores(cls) -> dict[str, str]:
        """Each source's raw store → the hash of its bytes."""
        return {
            str(f.relative_to(cls.root)): hashlib.sha256(f.read_bytes()).hexdigest()
            for f in sorted(cls.root.glob("*/ingest/entities.doltlite_db"))
        }

    @classmethod
    def grid_rows(cls) -> list[tuple[str, int]]:
        """How many grid rows each source has. The grid index is plain
        SQLite."""
        db = (
            cls.root
            / "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite"
        )
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            return con.execute(
                "SELECT source_id, count(*) FROM grid_rows GROUP BY 1 ORDER BY 1"
            ).fetchall()
        finally:
            con.close()

    def ingest_params(self) -> dict[str, dict]:
        return {
            s["id"].split("/")[0]: s.get("params", {})
            for s in self.config["steps"]
            if s["id"].endswith("/ingest")
        }

    def kept(self) -> list[str]:
        return sorted(
            g
            for g, p in self.ingest_params().items()
            if p.get("common", {}).get("download") is False
        )

    def test_some_sources_sync_and_the_rest_are_kept(self) -> None:
        params = self.ingest_params()
        synced = sorted(set(params) - set(self.kept()))
        self.assertTrue(self.kept(), "no source in the library is kept")
        self.assertTrue(synced, "no source in the library downloads")
        inputs = str(self.root / "inputs")
        for group in synced:
            self.assertIn(
                inputs, repr(params[group]), f"{group} reads nothing under inputs/"
            )
        groups = {g["id"] for g in self.config["groups"] if g.get("type") != "diff"}
        groups -= {"unified_grid_and_search_index", "unified_time_series_index"}
        self.assertEqual(sorted(groups), sorted(params), "a source has no download")

    def test_the_first_sync_runs_every_step(self) -> None:
        steps = {s["id"] for s in self.config["steps"]}
        self.assertEqual(sorted(steps), sorted(self.first), "a step did not run")
        failed = {s: o for s, o in self.first.items() if not o.startswith("succeeded")}
        self.assertEqual(failed, {}, "a step of the first sync did not succeed")

    def test_a_kept_source_keeps_its_store_as_it_was(self) -> None:
        kept = [f"{g}/ingest/entities.doltlite_db" for g in self.kept()]
        for store in kept:
            self.assertIn(store, self.stores_before)
            self.assertEqual(
                self.stores_before[store], self.stores_after_first[store], store
            )
            self.assertEqual(
                self.stores_before[store], self.stores_after_second[store], store
            )

    def test_the_grid_has_the_rows_the_fixture_build_gave_it(self) -> None:
        self.assertTrue(self.rows_before, "the library's grid is empty")
        self.assertEqual(self.rows_before, self.rows_after_first)

    def test_the_second_sync_redoes_nothing(self) -> None:
        redone = {
            step: outcome
            for step, outcome in self.second.items()
            if outcome != "skipped_up_to_date"
            and not (
                step.endswith("/ingest") and outcome == "succeeded, output unchanged"
            )
        }
        self.assertTrue(self.second, "the second sync named no steps")
        self.assertEqual(redone, {}, "the second sync redid work")


if __name__ == "__main__":
    MATERIALIZE = Path(sys.argv[1]).resolve()
    BIN_DIR = Path(sys.argv[2]).resolve()
    del sys.argv[1:3]
    unittest.main()
