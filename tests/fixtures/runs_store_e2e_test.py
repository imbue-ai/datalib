"""The run store, end to end, read by an engine that is not ours.

Everything else about the store is tested from inside Rust, against the
doltlite-linked SQLite that every binary in this tree carries. That
leaves the actual claim untested: that a sync run from a terminal
writes a file *any* tool can watch.

So this runs the real `datalib-dag` binary and reads what it wrote with
Python's stdlib `sqlite3` — a wholly separate engine, in a separate
process, that knows nothing about doltlite. If the store were ever
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

# Pinned so the run's clock and id are predictable: `datalib-dag` uses
# `--now` verbatim as the start time and `--run-id` verbatim as the run
# id, which is what lets a reader tell whether the store's newest run
# is the one it is looking at.
NOW = "2369-04-15T00:00:00+00:00"
RUN_ID = "0193c7e0-0000-7000-8000-000000000001"

# A step that reports progress both ways a step can — the sugar (a total
# up front, then increments) and a metric with a label — logs a line on
# stderr, and then claims its output.
STEP_SH = """
set -e
echo '{"event":"progress_length","step":"me","total":4}'
echo '{"event":"progress_message","step":"me","msg":"conversations.list"}'
echo '{"event":"progress_inc","step":"me","delta":1}'
echo '{"event":"progress_inc","step":"me","delta":3}'
echo '{"event":"metric","step":"me","name":"rows_upserted","labels":{"table":"t"},"value":12}'
echo 'a plain line on stderr' >&2
echo 'a plain line on stdout'
echo '{"timestamp":"2369-04-15T00:00:01Z","level":"WARN","target":"fake::fetch","threadName":"main","fields":{"message":"slow","attempts":3}}' >&2
test "$DATALIB_DAG_RUN_ID" = "%(run_id)s" || { echo "run id not passed: $DATALIB_DAG_RUN_ID" >&2; exit 1; }
test "$DATALIB_DAG_ATTEMPT" = "1" || { echo "attempt not passed: $DATALIB_DAG_ATTEMPT" >&2; exit 1; }
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


class RunStoreEndToEnd(unittest.TestCase):
    def setUp(self) -> None:
        if len(sys.argv) < 2:
            self.fail("usage: runs_store_e2e_test.py <datalib-dag>")
        self.dag = Path(sys.argv[1]).resolve()
        self.root = Path(tempfile.mkdtemp())

        script = self.root / "step.sh"
        script.write_text(STEP_SH.replace("%(run_id)s", RUN_ID))
        (self.root / "config.toml").write_text(CONFIG.format(script=script))

        proc = subprocess.run(
            [
                str(self.dag),
                str(self.root / "config.toml"),
                "--now",
                NOW,
                "--run-id",
                RUN_ID,
            ],
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"the run failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}",
        )
        self.store = self.root / "system" / "runs.sqlite"

    def test_the_runner_leaves_a_store_stock_sqlite_can_open(self) -> None:
        self.assertTrue(self.store.exists(), f"no run store at {self.store}")
        # The format claim, checked before we try to open it: a CTLD file
        # would fail below with a confusing "file is not a database".
        self.assertEqual(self.store.read_bytes()[:15], b"SQLite format 3")

        con = sqlite3.connect(f"file:{self.store}?mode=ro", uri=True)
        try:
            runs = con.execute(
                "SELECT run_id, started_at, finished_at FROM runs"
            ).fetchall()
            steps = {
                r[0]: r
                for r in con.execute(
                    "SELECT step, run_id, state, attempt, msg FROM step_runs"
                )
            }
            metrics = {
                (r[0], r[1], r[2]): r[3]
                for r in con.execute(
                    "SELECT step, name, labels, value FROM metrics WHERE run_id = ?",
                    (RUN_ID,),
                )
            }
            log = con.execute(
                "SELECT step, attempt, stream, level, thread, ts, msg, fields "
                "FROM log WHERE run_id = ? ORDER BY seq",
                (RUN_ID,),
            ).fetchall()
            version = con.execute("PRAGMA user_version").fetchone()[0]
        finally:
            con.close()

        self.assertEqual([(r[0], r[1]) for r in runs], [(RUN_ID, NOW)])
        self.assertIsNotNone(runs[0][2], "a finished run has a finish time")
        self.assertGreater(version, 0, "the store names its schema version")
        self.assertEqual(
            sorted(steps),
            ["fake/raw", "fake/rendered_md"],
            "every step in the plan gets a row",
        )
        for step, (_, run_id, state, attempt, msg) in steps.items():
            with self.subTest(step=step):
                self.assertEqual(run_id, RUN_ID)
                self.assertEqual(state, "succeeded")
                self.assertEqual(attempt, 1)
                self.assertEqual(msg, "conversations.list")
                # 1 + 3, accumulated by the runner: the wire carries
                # increments, the store carries a position — and what
                # the total leaves.
                self.assertEqual(metrics[(step, "done", "")], 4)
                self.assertEqual(metrics[(step, "queued", "")], 0)
                self.assertEqual(metrics[(step, "rows_upserted", "table=t")], 12)
                lines = {
                    (msg, stream, level, thread, fields)
                    for (s, _, stream, level, thread, _, msg, fields) in log
                    if s == step
                }
                # Both pipes are captured, each line saying which it
                # came from; a structured line is unwrapped into columns.
                self.assertIn(
                    ("a plain line on stderr", "stderr", "info", None, None), lines
                )
                self.assertIn(
                    ("a plain line on stdout", "stdout", "info", None, None), lines
                )
                self.assertIn(
                    ("slow", "stderr", "warn", "main", '{"attempts":3}'), lines
                )
                slow = next(r for r in log if r[0] == step and r[6] == "slow")
                self.assertEqual(slow[1], 1, "the line names its attempt")
                self.assertEqual(
                    slow[5], "2369-04-15T00:00:01Z", "a line's own clock is kept"
                )

    def test_the_store_leaves_no_doltlite_lock_sidecar(self) -> None:
        # A `.<name>-lock` file is doltlite's tell. Its absence is how we
        # know the runner did not quietly claim the path for the
        # prolly-tree engine.
        self.assertFalse(
            (self.root / "system" / ".runs.sqlite-lock").exists(),
            "a lock sidecar means doltlite claimed the store after all",
        )


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
