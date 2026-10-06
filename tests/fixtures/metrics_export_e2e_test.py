"""`GET /metrics`, end to end, read by Prometheus's own parser.

The Rust tests check the lines `datalib-http` writes against the lines
we expect. That leaves the claim the endpoint exists for untested: that a
scraper accepts them. So this runs a step through the real `datalib-dag`,
serves the root with the real `datalib-http`, and parses what `/metrics`
answers with `prometheus_client` — the parser the Prometheus project
maintains — checking that each series arrives typed and labelled as the
naming rule says (`docs/dev/step_protocol.md`).
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

from prometheus_client.parser import (  # pyright: ignore[reportMissingImports]
    text_string_to_metric_families,
)

NOW = "2369-04-15T00:00:00+00:00"
DEADLINE_SECS = 60.0

# A counter with a label, a gauge, and the progress sugar the runner turns
# into `done_total` and `queued`.
STEP_SH = """
set -e
echo '{"event":"progress_length","step":"me","total":4}'
echo '{"event":"progress_inc","step":"me","delta":4}'
echo '{"event":"metric","step":"me","name":"rows_upserted_total","labels":{"table":"t"},"value":12}'
echo '{"event":"metric","step":"me","name":"items","value":7}'
mkdir -p "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
echo hi > "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP/x.txt"
printf '{"event":"outcome","outputs":[{"path":"%s","version":"v1"}]}\\n' \
    "$DATALIB_DAG_STEP"
"""

CONFIG = """
[[steps]]
id = "fake/raw"
command = "sh {script}"
"""


def wait_for_file(path: Path, proc: subprocess.Popen[bytes], log: Path) -> str:
    deadline = time.monotonic() + DEADLINE_SECS
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise AssertionError(
                f"datalib-http exited {proc.returncode}:\n{log.read_text()}"
            )
        if path.is_file() and (text := path.read_text()):
            return text
        time.sleep(0.05)
    raise AssertionError(f"datalib-http never wrote {path}:\n{log.read_text()}")


class MetricsExport(unittest.TestCase):
    def setUp(self) -> None:
        if len(sys.argv) < 3:
            self.fail("usage: metrics_export_e2e_test.py <datalib-dag> <datalib-http>")
        dag, http = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
        self.root = Path(tempfile.mkdtemp())
        script = self.root / "step.sh"
        script.write_text(STEP_SH)
        (self.root / "config.toml").write_text(CONFIG.format(script=script))

        run = subprocess.run(
            [str(dag), str(self.root / "config.toml"), "--now", NOW],
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
        self.assertEqual(run.returncode, 0, f"the run failed:\n{run.stderr}")

        url_file = self.root.parent / f"{self.root.name}.url"
        log = self.root.parent / f"{self.root.name}.server.log"
        self.server_log = log.open("w")
        self.server = subprocess.Popen(
            [str(http), "--no-open", "--url-file", str(url_file), str(self.root)],
            env={**os.environ, "DATALIB_BIND": "127.0.0.1:0"},
            stdin=subprocess.DEVNULL,
            stdout=self.server_log,
            stderr=subprocess.STDOUT,
        )
        url = wait_for_file(url_file, self.server, log)
        self.origin = url.split("?")[0].rsplit("/", 1)[0]
        self.token = wait_for_file(
            self.root / "system" / "api-token", self.server, log
        ).strip()

    def tearDown(self) -> None:
        self.server.terminate()
        self.server.wait(timeout=30)
        self.server_log.close()

    def scrape(self) -> tuple[str, str]:
        req = urllib.request.Request(self.origin + "/metrics")
        req.add_header("Authorization", f"Bearer {self.token}")
        deadline = time.monotonic() + DEADLINE_SECS
        while True:
            try:
                with urllib.request.urlopen(req, timeout=30) as resp:
                    return resp.headers["Content-Type"], resp.read().decode()
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)

    def test_a_scrape_parses_with_every_series_typed_by_its_name(self) -> None:
        content_type, text = self.scrape()
        self.assertTrue(
            content_type.startswith("text/plain; version=0.0.4"), content_type
        )

        # Raises on anything a scraper would refuse.
        families = {f.name: f for f in text_string_to_metric_families(text)}

        def sample(family: str, sample_name: str, **labels: str) -> float:
            for s in families[family].samples:
                if s.name == sample_name and all(
                    s.labels.get(k) == v for k, v in labels.items()
                ):
                    return s.value
            self.fail(f"no {sample_name}{labels} in:\n{text}")

        # The parser names a counter's family without `_total`.
        rows = "datalib_step_rows_upserted"
        self.assertEqual(families[rows].type, "counter", text)
        self.assertEqual(
            sample(
                rows, "datalib_step_rows_upserted_total", step="fake/raw", table="t"
            ),
            12,
        )
        self.assertEqual(families["datalib_step_done"].type, "counter", text)
        self.assertEqual(
            sample("datalib_step_done", "datalib_step_done_total", step="fake/raw"), 4
        )

        self.assertEqual(families["datalib_step_items"].type, "gauge", text)
        self.assertEqual(
            sample("datalib_step_items", "datalib_step_items", step="fake/raw"), 7
        )
        # The runner empties a finished step's queue.
        self.assertEqual(
            sample("datalib_step_queued", "datalib_step_queued", step="fake/raw"), 0
        )

        # One state per step reads 1.
        states = [
            s
            for s in families["datalib_step_state"].samples
            if s.labels["step"] == "fake/raw" and s.value == 1
        ]
        self.assertEqual(len(states), 1, [s.labels for s in states])
        self.assertGreater(
            sample(
                "datalib_step_last_success_timestamp_seconds",
                "datalib_step_last_success_timestamp_seconds",
                step="fake/raw",
            ),
            0,
        )
        self.assertGreater(sample("datalib_root_bytes", "datalib_root_bytes"), 0)


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
