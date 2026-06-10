"""End-to-end pipeline test.

Replaces the previous `run_pipeline_test.sh` shell wrapper. Drives
the same fixture-backed sync pipeline three times against a single
data root and asserts on the resume-cursor behavior:

  Run 1: fresh data root — full ingest happens; the
         `signal_snapshot_already_ingested` event is NOT emitted.
  Run 2: same data root, no flags — signal's `ingested_backups`
         cursor (Blake3 over the snapshot's three on-disk files)
         must short-circuit the second extract: the
         `signal_snapshot_already_ingested` event IS emitted.
  Run 3: `--reset-and-redownload` — orchestrator emits its reset
         banner, the cursor row is wiped, signal re-ingests, and
         `signal_snapshot_already_ingested` is again NOT emitted.

The pytest invokes `run_sync_pipeline.py` as a subprocess (same
contract as the prior sh_test). Per-run assertions read the
subprocess's combined stderr, which carries the orchestrator's
tracing events on a TTY-less run as pretty-printed lines.

Stdlib `unittest` rather than third-party pytest to keep the
toolchain dep graph small — one self-contained test doesn't
justify wiring pytest through pip.parse.

Why stderr inspection and not direct doltlite reads: doltlite's
on-disk format isn't sqlite-file-compatible, the doltlite CLI
isn't built by bazel, and we don't want to depend on a system
binary. The orchestrator's tracing events are an explicit,
load-bearing API surface already used by the obs stack — asserting
on them keeps the test hermetic.
"""

from __future__ import annotations

import os
import subprocess
import sys
import unittest
from pathlib import Path


# Bazel runfiles layout: under bzlmod, the workspace dir is `_main`.
_BAZEL_WORKSPACE_DIR = "_main"

# Tracing event name emitted by signal extract when the
# `ingested_backups` cursor short-circuits a fetch. Source of truth:
# providers/signal/src/extract/mod.rs. Presence or absence of this
# event across the three runs is the load-bearing signal.
EV_SIGNAL_ALREADY_INGESTED = "signal_snapshot_already_ingested"


def _argv():
    """sys.argv layout, matching `args = [...]` in BUILD.bazel:

    [0]: test script (set by py_test)
    [1]: run_sync_pipeline.py path
    [2]: frankweiler_sync_bin path
    [3]: signal_make_fixture path
    [4]: --now stamp
    [5..]: fixture paths
    """
    return sys.argv[1:]


class IngestedTngPipelineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        argv = _argv()
        cls.driver_script = argv[0]
        cls.sync_bin = argv[1]
        cls.signal_bin = argv[2]
        cls.now = argv[3]
        cls.fixture_paths = argv[4:]

        cls.workspace = Path(os.environ["TEST_TMPDIR"]) / "sync_workspace"
        cls.workspace.mkdir(parents=True, exist_ok=True)

        runfiles_root = os.environ.get("TEST_SRCDIR")
        if runfiles_root:
            cls.cwd = Path(runfiles_root) / _BAZEL_WORKSPACE_DIR
        else:
            cls.cwd = Path.cwd()

    def _run_pipeline(self, *, reset: bool) -> subprocess.CompletedProcess:
        env = {**os.environ}
        if reset:
            env["INGESTED_TNG_RESET"] = "1"
        else:
            env.pop("INGESTED_TNG_RESET", None)
        argv = [
            sys.executable,
            self.driver_script,
            self.sync_bin,
            self.signal_bin,
            self.now,
            str(self.workspace),
            *self.fixture_paths,
        ]
        result = subprocess.run(
            argv,
            check=True,
            cwd=str(self.cwd),
            env=env,
            capture_output=True,
            text=True,
        )
        # Print the captured streams so a test failure leaves the
        # orchestrator's output in the test's own log for debugging.
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        return result

    def test_pipeline_resume_and_reset(self) -> None:
        # --- Run 1: fresh workspace. No cursor hit yet.
        run1 = self._run_pipeline(reset=False)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run1.stderr,
            "run 1 is a fresh ingest — signal must NOT report already_ingested",
        )

        # --- Run 2: same data root, no flags. Signal's
        # ingested_backups cursor MUST short-circuit the second
        # extract. That's the load-bearing assertion of this test.
        run2 = self._run_pipeline(reset=False)
        self.assertIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run2.stderr,
            "run 2 must hit signal's ingested_backups cursor and emit "
            f"the {EV_SIGNAL_ALREADY_INGESTED!r} event",
        )

        # --- Run 3: --reset-and-redownload. The flag wipes signal's
        # ingested_backups row before fetch, so the cursor MUST NOT
        # short-circuit; we should see signal re-ingest from scratch
        # instead. (If --reset-and-redownload were silently dropped,
        # the cursor row would still be present and this run would
        # behave like run 2 — this assertion catches that case.)
        run3 = self._run_pipeline(reset=True)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run3.stderr,
            "after --reset-and-redownload wipes ingested_backups, "
            "signal must NOT report already_ingested on run 3",
        )


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
