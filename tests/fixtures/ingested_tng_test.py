"""End-to-end pipeline test.

Drives the fixture-backed sync pipeline three times against a single
data root and asserts on what actually landed in the doltlite stores:

  Run 1: fresh data root — full ingest. Every expected provider must
         appear in the grid index, and signal's resume cursor must be
         recorded.
  Run 2: same data root, no flags — a steady-state re-run. Row counts
         must be IDENTICAL to run 1 (re-running must not duplicate or
         drop rows), signal's cursor must be untouched, and the
         `signal_snapshot_already_ingested` event must be emitted.
  Run 3: `--reset-and-redownload` — the cursor row is wiped and signal
         re-ingests, so the event must NOT be emitted; the store must
         then converge back to exactly the same contents.

The pytest invokes `run_sync_pipeline.py` as a subprocess (same
contract as the prior sh_test).

Stdlib `unittest` rather than third-party pytest to keep the
toolchain dep graph small — one self-contained test doesn't
justify wiring pytest through pip.parse.

Reading the stores directly: `//third-party/doltlite:doltlite` is a
bazel-built sqlite3-shell CLI linked against the same amalgamation the
pipeline uses (doltlite's on-disk format is not sqlite-file-compatible,
so stock sqlite3 cannot open these). It arrives through `data`, so this
stays hermetic — no system binary. Before it existed this test could
only grep the orchestrator's tracing events out of stderr, which meant
"the pipeline re-downloaded everything on run 2" was indistinguishable
from a pass. The stderr assertions are kept: they cover the one
transition (cursor hit / miss) that leaves no trace in the final state,
since run 3 wipes the cursor and then re-creates it.
"""

from __future__ import annotations

import os
import subprocess
import sys
import unittest
from pathlib import Path


# Bazel runfiles layout: under bzlmod, the workspace dir is `_main`.
_BAZEL_WORKSPACE_DIR = "_main"

# Tracing event name emitted by signal download when the
# `ingested_backups` cursor short-circuits a fetch. Source of truth:
# providers/signal/src/download/mod.rs.
EV_SIGNAL_ALREADY_INGESTED = "signal_snapshot_already_ingested"

# Providers that must appear in `grid_rows` after a full fixture run.
#
# These are `grid_rows.provider` values, which are provider *types* and
# so don't always match the DAG step names (chatgpt-api reports
# `openai`, the carddav source reports `contacts`, the mbox source
# reports `jmap`).
#
# Keep this exhaustive over the sources run_sync_pipeline.py
# configures. The first draft had to omit `gitlab`, which turned out to
# be a dead fixture rather than an intended exclusion — its records
# spelled the project path `project_path` while every consumer had
# moved to `project_full_path`, so it silently produced nothing for
# three months. A source that stops producing rows should fail here,
# not disappear quietly.
EXPECTED_PROVIDERS = frozenset(
    {
        "anthropic",
        "beeper",
        "contacts",
        "github",
        "gitlab",
        "google_takeout",
        "jmap",
        "linkedin",
        "notion",
        "openai",
        # The only file-backed source in this fixture that renders.
        # fsindex and lightroom scan trees too but produce no rows;
        # `pdf` converts what it scans, so it must show up here.
        "pdf",
        "signal",
        "slack",
        "sms_backup_restore",
        "whatsapp",
        # Render-only in the fixture: its raw store is seeded by
        # `yolink-make-fixture` rather than downloaded, so there is no
        # `yolink.download` step. Rows here prove the render step ran
        # over that store — see run_sync_pipeline.py's RENDER_ONLY.
        "yolink",
    }
)


def _argv():
    """sys.argv layout, matching `args = [...]` in BUILD.bazel:

    [0]: run_sync_pipeline.py path
    [1]: datalib_dag path
    [2]: datalib_step path
    [3]: signal_make_fixture path
    [4]: whatsapp_make_fixture path
    [5]: doltlite CLI path
    [6]: --now stamp
    [7..]: fixture paths, forwarded verbatim as run_sync_pipeline.py's
           args 7..22 (the last two being yolink-make-fixture + its
           spec; see that script's docstring for why they're appended)
    """
    return sys.argv[1:]


class IngestedTngPipelineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        argv = _argv()
        cls.driver_script = argv[0]
        cls.dag_bin = argv[1]
        cls.step_bin = argv[2]
        cls.signal_bin = argv[3]
        cls.whatsapp_bin = argv[4]
        cls.doltlite_bin = argv[5]
        cls.now = argv[6]
        cls.fixture_paths = argv[7:]

        cls.workspace = Path(os.environ["TEST_TMPDIR"]) / "sync_workspace"
        cls.workspace.mkdir(parents=True, exist_ok=True)

        runfiles_root = os.environ.get("TEST_SRCDIR")
        if runfiles_root:
            cls.cwd = Path(runfiles_root) / _BAZEL_WORKSPACE_DIR
        else:
            cls.cwd = Path.cwd()

    # ── doltlite store access ───────────────────────────────────────

    @property
    def _index_db(self) -> Path:
        """The grid index the `grid_index` fan-in step writes."""
        return self.workspace / "system" / "backend_index" / "db.doltlite_db"

    @property
    def _signal_entities_db(self) -> Path:
        """Signal's raw entity store, which holds `ingested_backups`."""
        return self.workspace / "signal" / "raw" / "entities.doltlite_db"

    def _query(self, db: Path, sql: str) -> list[str]:
        """Run one SQL statement, returning stripped non-empty lines."""
        self.assertTrue(db.is_file(), f"expected a doltlite store at {db}")
        result = subprocess.run(
            [str(Path(self.cwd) / self.doltlite_bin), str(db), sql],
            check=True,
            capture_output=True,
            text=True,
        )
        return [ln.strip() for ln in result.stdout.splitlines() if ln.strip()]

    def _scalar(self, db: Path, sql: str) -> str:
        rows = self._query(db, sql)
        self.assertEqual(len(rows), 1, f"expected one row from {sql!r}, got {rows}")
        return rows[0]

    def _count(self, db: Path, table: str) -> int:
        return int(self._scalar(db, f"SELECT COUNT(*) FROM {table};"))

    def _index_shape(self) -> dict[str, int]:
        """The index contents that a re-run must leave unchanged."""
        return {
            "grid_rows": self._count(self._index_db, "grid_rows"),
            "markdowns": self._count(self._index_db, "markdowns"),
        }

    def _providers(self) -> frozenset[str]:
        return frozenset(
            self._query(self._index_db, "SELECT DISTINCT provider FROM grid_rows;")
        )

    def _pdf_shape(self) -> dict[str, int]:
        """The `pdf` source's contribution, by grid_rows kind.

        Pinned rather than merely non-empty because this source is the
        one whose output feeds the qmd index by *page*: silent growth
        here shows up as a slower fixture build for everyone, and a
        silent drop to zero would mean PDFs stopped being searchable
        without any test going red.
        """
        rows = self._query(
            self._index_db,
            "SELECT kind, COUNT(*) FROM grid_rows WHERE provider = 'pdf' "
            "GROUP BY kind ORDER BY kind;",
        )
        out: dict[str, int] = {}
        for r in rows:
            kind, n = r.rsplit("|", 1)
            out[kind] = int(n)
        return out

    def _signal_cursor(self) -> list[str]:
        """Signal's `ingested_backups` rows as `<snapshot_dir>|<blake3>`."""
        return self._query(
            self._signal_entities_db,
            "SELECT snapshot_dir, blake3 FROM ingested_backups ORDER BY snapshot_dir;",
        )

    # ── pipeline driver ────────────────────────────────────────────

    def _run_pipeline(self, *, reset: bool) -> subprocess.CompletedProcess:
        env = {**os.environ}
        if reset:
            env["INGESTED_TNG_RESET"] = "1"
        else:
            env.pop("INGESTED_TNG_RESET", None)
        argv = [
            sys.executable,
            self.driver_script,
            self.dag_bin,
            self.step_bin,
            self.signal_bin,
            self.whatsapp_bin,
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
        # --- Run 1: fresh workspace. Full ingest.
        run1 = self._run_pipeline(reset=False)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run1.stderr,
            "run 1 is a fresh ingest — signal must NOT report already_ingested",
        )

        shape1 = self._index_shape()
        self.assertGreater(
            shape1["grid_rows"], 0, "run 1 must load grid rows into the index"
        )
        self.assertGreater(
            shape1["markdowns"], 0, "run 1 must load markdowns into the index"
        )
        # Every source that renders must be represented. An exact set
        # comparison (not a subset check) is deliberate: it catches a
        # source silently dropping out of the pipeline, which is the
        # failure this test previously could not see at all.
        self.assertEqual(
            self._providers(),
            EXPECTED_PROVIDERS,
            "grid_rows providers after a full run",
        )

        # PDFs specifically: 3 renderable documents, 4 pages between
        # them (the scanned blueprint is recorded but not rendered, and
        # the corrupt file is skipped). Every page row must carry a
        # `qmd_path`, since that column is what lets a qmd hit resolve
        # back to a grid row — a page indexed without one is findable
        # by search but unreachable from the UI.
        self.assertEqual(
            self._pdf_shape(),
            {"PDF Document": 3, "PDF Page": 4},
            "pdf grid_rows shape",
        )
        self.assertEqual(
            self._scalar(
                self._index_db,
                "SELECT COUNT(*) FROM grid_rows "
                "WHERE provider = 'pdf' AND qmd_path IS NULL;",
            ),
            "0",
            "every pdf row needs a qmd_path to be reachable from search",
        )
        # The index is committed, so its version history is non-empty.
        self.assertGreater(
            int(self._scalar(self._index_db, "SELECT COUNT(*) FROM dolt_log;")),
            0,
            "grid_index must commit, leaving a dolt_log entry",
        )
        # Signal recorded exactly one snapshot in its resume cursor.
        cursor1 = self._signal_cursor()
        self.assertEqual(
            len(cursor1), 1, f"expected one ingested_backups row, got {cursor1}"
        )

        # --- Run 2: same data root, no flags. Signal's
        # ingested_backups cursor MUST short-circuit the second
        # download.
        run2 = self._run_pipeline(reset=False)
        self.assertIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run2.stderr,
            "run 2 must hit signal's ingested_backups cursor and emit "
            f"the {EV_SIGNAL_ALREADY_INGESTED!r} event",
        )
        # The load-bearing state assertion: a steady-state re-run is
        # idempotent. Re-rendering and re-loading the same documents
        # must not duplicate rows (upserts keyed correctly) or drop
        # them (a cursor short-circuit skipping too much).
        self.assertEqual(
            self._index_shape(), shape1, "run 2 must leave the index unchanged"
        )
        self.assertEqual(self._providers(), EXPECTED_PROVIDERS, "run 2 providers")
        self.assertEqual(
            self._signal_cursor(), cursor1, "run 2 must not disturb signal's cursor"
        )

        # --- Run 3: --reset-and-redownload. The flag wipes signal's
        # ingested_backups row before fetch, so the cursor MUST NOT
        # short-circuit. (If --reset-and-redownload were silently
        # dropped, this run would behave like run 2.)
        run3 = self._run_pipeline(reset=True)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run3.stderr,
            "after --reset-and-redownload wipes ingested_backups, "
            "signal must NOT report already_ingested on run 3",
        )
        # A reset re-downloads from scratch and must converge to the
        # same store, not to a duplicated or partial one.
        self.assertEqual(
            self._index_shape(),
            shape1,
            "run 3 (--reset-and-redownload) must converge to the same index",
        )
        self.assertEqual(self._providers(), EXPECTED_PROVIDERS, "run 3 providers")
        # The cursor is wiped mid-run, so by the end it must be back —
        # same snapshot, same fingerprint.
        self.assertEqual(
            self._signal_cursor(),
            cursor1,
            "run 3 must re-record the same signal snapshot fingerprint",
        )


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
