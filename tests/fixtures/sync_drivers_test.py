"""Both sync drivers stop at once, naming the floor, when the loop holds
their steps for disk space.

Guards the hang where `:ingested_tng` polled a request the loop would
never run, because the machine had less free space than the default
`[disk_space]` floor: the loop waits for space a build never frees.
A floor no disk can meet holds every step on any machine; a floor of
zero is the control, the same step syncing to its end.
"""

from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from sync_drivers import DISK_HOLD_PREFIX, CliDriver, HttpDriver

NOW = "2369-04-15T00:00:00+00:00"
# Long enough for a loaded runner to start a server and tick once; a
# hold the drivers miss runs this out and fails on the deadline's words.
DEADLINE_SECS = 30.0

STEP_SH = """
set -e
mkdir -p "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
printf '{"event":"outcome","outputs":[{"path":"%s","version":"v1"}]}\\n' \\
    "$DATALIB_DAG_STEP"
"""

CONFIG = """[disk_space]
pause_below_bytes = "{floor}"

[[steps]]
id = "fake/raw"
command = "sh {script}"
"""

NO_DISK_HAS_THIS = "1000000 TB"


class DiskHold(unittest.TestCase):
    def setUp(self) -> None:
        if len(sys.argv) < 3:
            self.fail("usage: sync_drivers_test.py <datalib-dag> <datalib-http>")
        self.dag = Path(sys.argv[1]).resolve()
        self.http = Path(sys.argv[2]).resolve()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / "root"
        self.root.mkdir()
        self.script = Path(tmp.name) / "step.sh"
        self.script.write_text(STEP_SH)

    def config(self, floor: str) -> str:
        return CONFIG.format(floor=floor, script=self.script)

    def cli(self) -> CliDriver:
        return CliDriver(self.dag, self.root, self.root, NOW, dict(os.environ))

    def http_driver(self) -> HttpDriver:
        driver = HttpDriver(self.http, self.root, self.root, NOW, dict(os.environ))
        self.addCleanup(driver.close)
        return driver

    def assert_fails_on_the_floor(self, driver: CliDriver | HttpDriver) -> None:
        driver.build_config([self.config(NO_DISK_HAS_THIS)])
        started = time.monotonic()
        with self.assertRaises(SystemExit) as caught:
            driver.sync(["fake/raw"], deadline_secs=DEADLINE_SECS)
        message = str(caught.exception)
        self.assertIn(DISK_HOLD_PREFIX, message)
        self.assertIn("[disk_space]", message)
        self.assertLess(time.monotonic() - started, DEADLINE_SECS / 2, message)

    def test_cli_fails_when_the_floor_holds_its_steps(self) -> None:
        self.assert_fails_on_the_floor(self.cli())

    def test_http_fails_when_the_floor_holds_its_steps(self) -> None:
        self.assert_fails_on_the_floor(self.http_driver())

    def test_cli_syncs_under_a_floor_of_zero(self) -> None:
        driver = self.cli()
        driver.build_config([self.config("0 B")])
        driver.sync(["fake/raw"], deadline_secs=DEADLINE_SECS)
        self.assertTrue((self.root / "fake" / "raw").is_dir())

    def test_http_syncs_under_a_floor_of_zero(self) -> None:
        driver = self.http_driver()
        driver.build_config([self.config("0 B")])
        driver.sync(["fake/raw"], deadline_secs=DEADLINE_SECS)
        self.assertTrue((self.root / "fake" / "raw").is_dir())


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
