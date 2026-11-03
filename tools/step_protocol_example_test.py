"""The custom step example in docs/dev/step_protocol.md, run as it is
written under the real runner. Agents copy it, and it is the only place
most of them learn what DATALIB_DAG_CHANGED_INPUTS and
DATALIB_DAG_FAILED_INPUTS hold; nothing ran it, so it could drift from
the runner unnoticed.

Two producer steps feed it. Three syncs: the first names every input as
changed, the second only the producer that moved, and the third, where
one producer failed, makes the example warn that it is reading a
partial input.
"""

import json
import re
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

DAG = Path(sys.argv[1]).resolve()
DOC = Path(sys.argv[2]).resolve()

PRODUCER = textwrap.dedent(
    """\
    import json, os, pathlib, sys
    control = pathlib.Path(sys.argv[1])
    name = os.environ["DATALIB_DAG_STEP"].split("/")[0]
    if (control / f"fail_{name}").exists():
        sys.exit(1)
    version = (control / f"version_{name}").read_text().strip()
    out = pathlib.Path(os.environ["DATALIB_DAG_DATA_ROOT"]) / os.environ["DATALIB_DAG_STEP"]
    out.mkdir(parents=True, exist_ok=True)
    (out / "data.txt").write_text(version)
    print(json.dumps({"event": "outcome",
                      "outputs": [{"path": os.environ["DATALIB_DAG_STEP"], "version": version}]}),
          flush=True)
    """
)


def example_from_doc() -> str:
    blocks = re.findall(r"```python\n(.*?)```", DOC.read_text(), flags=re.DOTALL)
    steps = [b for b in blocks if b.startswith("#!/usr/bin/env python3")]
    assert len(steps) == 1, f"expected one step example in {DOC}, found {len(steps)}"
    return steps[0]


class StepProtocolExample(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        base = Path(self.dir.name)
        self.root = base / "root"
        self.control = base / "control"
        self.root.mkdir()
        self.control.mkdir()
        (base / "producer.py").write_text(PRODUCER)
        (base / "example.py").write_text(example_from_doc())
        py = sys.executable
        producer = f"{py} {base / 'producer.py'} {self.control}"
        self.config = self.root / "config.toml"
        self.config.write_text(
            textwrap.dedent(
                f"""\
                [[groups]]
                id = "a"
                [[groups]]
                id = "b"
                [[groups]]
                id = "report"

                [[steps]]
                group = "a"
                function = "make"
                command = "{producer}"

                [[steps]]
                group = "b"
                function = "make"
                command = "{producer}"

                [[steps]]
                group = "report"
                function = "build"
                command = "{py} {base / "example.py"}"
                inputs = ["a/make", "b/make"]
                """
            )
        )
        self.seen = 0

    def tearDown(self) -> None:
        self.dir.cleanup()

    def set_version(self, name: str, version: str) -> None:
        (self.control / f"version_{name}").write_text(version)

    def sync(self) -> list[tuple[str, str]]:
        """Run every step once; return the example's new log lines."""
        run = subprocess.run(
            [str(DAG), str(self.config), "--sync", "a/make,b/make"],
            check=False,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertIn(run.returncode, (0, 2), run.stderr)
        db = sqlite3.connect(
            f"file:{self.root / 'system/runs/runs.sqlite'}?mode=ro", uri=True
        )
        try:
            rows = db.execute(
                "SELECT seq, level, msg FROM log WHERE step = 'report/build' AND seq > ? "
                "ORDER BY seq",
                (self.seen,),
            ).fetchall()
        finally:
            db.close()
        if rows:
            self.seen = rows[-1][0]
        return [(level, msg) for _, level, msg in rows]

    def test_the_example_reads_what_moved_and_what_failed(self) -> None:
        self.set_version("a", "a1")
        self.set_version("b", "b1")
        first = self.sync()
        self.assertIn(("info", "a/make moved"), first, "a first run names every input")
        self.assertIn(("info", "b/make moved"), first, "a first run names every input")

        self.set_version("a", "a2")
        second = self.sync()
        self.assertIn(("info", "a/make moved"), second)
        self.assertNotIn(("info", "b/make moved"), second, "b did not move")

        (self.control / "fail_a").write_text("")
        self.set_version("b", "b2")
        third = self.sync()
        self.assertIn(("info", "b/make moved"), third)
        self.assertTrue(
            any(
                level == "warn" and msg.startswith("a/make did not finish its last run")
                for level, msg in third
            ),
            f"the example warns about the failed input: {json.dumps(third)}",
        )


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
