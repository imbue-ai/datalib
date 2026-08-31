"""The progress bus, end to end, read by an engine that is not ours.

Everything else about the bus is tested from inside Rust, against the
doltlite-linked SQLite that every binary in this tree carries. That
leaves the actual claim untested: that a sync run from a terminal
writes a file *any* tool can watch.

So this runs the real `datalib-dag` binary and reads what it wrote with
Python's stdlib `sqlite3` — a wholly separate engine, in a separate
process, that knows nothing about doltlite. If the bus were ever
written in doltlite's own CTLD format, stdlib sqlite3 could not open it
at all and this test would say so.
"""

from __future__ import annotations

import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Pinned so the run id is predictable: `datalib-dag` uses `--now`
# verbatim as the run id, which is what lets a reader tell whether the
# bus describes the run it is looking at.
NOW = "2369-04-15T00:00:00+00:00"

# A step that reports progress the way a real downloader does — a total
# up front, then increments — and then claims its output.
STEP_SH = """
set -e
echo '{"event":"progress_length","step":"me","total":4}'
echo '{"event":"progress_message","step":"me","msg":"conversations.list"}'
echo '{"event":"progress_inc","step":"me","delta":1}'
echo '{"event":"progress_inc","step":"me","delta":3}'
mkdir -p "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
echo hi > "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP/x.txt"
printf '{"event":"outcome","outputs":[{"path":"%s","version":"v1"}]}\\n' \
    "$DATALIB_DAG_STEP"
"""

CONFIG = """
[[steps]]
id = "fake/raw"
command = "sh {script}"

[[steps]]
id = "fake/rendered_md"
command = "sh {script}"
inputs = ["fake/raw"]
"""


class ProgressBusEndToEnd(unittest.TestCase):
    def setUp(self) -> None:
        if len(sys.argv) < 2:
            self.fail("usage: progress_bus_e2e_test.py <datalib-dag>")
        self.dag = Path(sys.argv[1]).resolve()
        self.root = Path(tempfile.mkdtemp())

        script = self.root / "step.sh"
        script.write_text(STEP_SH)
        (self.root / "config.toml").write_text(CONFIG.format(script=script))

        proc = subprocess.run(
            [str(self.dag), str(self.root / "config.toml"), "--now", NOW],
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"the run failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}",
        )
        self.bus = self.root / "system" / "progress.sqlite"

    def test_the_runner_leaves_a_bus_stock_sqlite_can_open(self) -> None:
        self.assertTrue(self.bus.exists(), f"no progress bus at {self.bus}")
        # The format claim, checked before we try to open it: a CTLD file
        # would fail below with a confusing "file is not a database".
        self.assertEqual(self.bus.read_bytes()[:15], b"SQLite format 3")

        con = sqlite3.connect(f"file:{self.bus}?mode=ro", uri=True)
        try:
            rows = dict(
                (r[0], r)
                for r in con.execute(
                    "SELECT step, run_id, state, done, total, msg FROM step_progress"
                )
            )
        finally:
            con.close()

        self.assertEqual(
            sorted(rows),
            ["fake/raw", "fake/rendered_md"],
            "every step in the plan gets a row",
        )
        for step, (_, run_id, state, done, total, msg) in rows.items():
            with self.subTest(step=step):
                # The run id is what a reader matches against the run it
                # is displaying, so a stale bus cannot paint bars onto
                # the wrong run.
                self.assertEqual(run_id, NOW)
                self.assertEqual(state, "succeeded")
                # 1 + 3, accumulated by the runner: the wire carries
                # increments, the bus carries a position.
                self.assertEqual(done, 4)
                self.assertEqual(total, 4)
                self.assertEqual(msg, "conversations.list")

    def test_the_bus_leaves_no_doltlite_lock_sidecar(self) -> None:
        # A `.<name>-lock` file is doltlite's tell. Its absence is how we
        # know the runner did not quietly claim the path for the
        # prolly-tree engine.
        self.assertFalse(
            (self.root / "system" / ".progress.sqlite-lock").exists(),
            "a lock sidecar means doltlite claimed the bus after all",
        )


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
