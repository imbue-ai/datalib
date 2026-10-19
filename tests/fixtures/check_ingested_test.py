"""`check_ingested.py` against the built fixture, and against copies of
it with a source taken out the way a bad build left it."""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from check_ingested import declared_sources


class CheckIngestedTest(unittest.TestCase):
    doltlite: Path
    store: Path
    config_body: Path

    def check(self, store: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(Path(__file__).parent / "check_ingested.py"),
                str(self.doltlite),
                str(store),
                str(self.config_body),
            ],
            capture_output=True,
            text=True,
            check=False,
        )

    def without(self, sql: str) -> Path:
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        copy = tmp / self.store.name
        shutil.copy(self.store, copy)
        copy.chmod(0o644)
        subprocess.run([str(self.doltlite), str(copy), sql], check=True)
        return copy

    def test_the_built_fixture_has_every_source(self):
        done = self.check(self.store)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_source_the_sync_never_ran_fails_the_build(self):
        """2026-10-19: a build exited 0 with `tng_calendar` absent from
        the index, and four tests downstream failed on the cached fixture."""
        store = self.without(
            "DELETE FROM grid_rows WHERE source_id = 'tng_calendar';"
            "DELETE FROM source_cursors WHERE source_id = 'tng_calendar';"
        )
        done = self.check(store)
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("tng_calendar: no grid_rows, no source_cursors row", done.stderr)

    def test_a_source_without_its_cursor_fails_the_build(self):
        store = self.without("DELETE FROM source_cursors WHERE source_id = 'codex';")
        done = self.check(store)
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("codex: no source_cursors row", done.stderr)
        self.assertNotIn("codex: no grid_rows", done.stderr)

    def test_every_typed_group_is_a_source_and_the_index_is_not(self):
        sources = declared_sources(self.config_body.read_text())
        self.assertIn("tng_calendar", sources)
        self.assertIn("slack-diff", sources)
        self.assertNotIn("unified_document_index", sources)


if __name__ == "__main__":
    CheckIngestedTest.doltlite = Path(sys.argv[1]).resolve()
    CheckIngestedTest.store = Path(sys.argv[2]).resolve()
    CheckIngestedTest.config_body = Path(sys.argv[3]).resolve()
    unittest.main(argv=sys.argv[:1])
