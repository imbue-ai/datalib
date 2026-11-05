"""The custom step example in docs/dev/step_protocol.md, run as it is
written under the real runner. Agents copy it, and it is the only place
most of them learn what DATALIB_DAG_CHANGED_INPUTS and
DATALIB_DAG_FAILED_INPUTS hold; nothing ran it, so it could drift from
the runner unnoticed.

Two producer steps feed it, each a real doltlite store whose version is
its commit, as a download's is. The example opens that commit with the
reader in docs/dev/doltlite.md and logs the commit's message, the version
the producer was told to write. Three syncs: the first names every input
as changed, the second only the producer that moved, and the third, where
one producer failed, makes the example warn that it is reading a partial
input.
"""

import importlib.util
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
DOLTLITE_DOC = Path(sys.argv[3]).resolve()
CLI = Path(sys.argv[4]).resolve()

PRODUCER = textwrap.dedent(
    """\
    import json, os, pathlib, subprocess, sys
    control, cli = pathlib.Path(sys.argv[1]), sys.argv[2]
    name = os.environ["DATALIB_DAG_STEP"].split("/")[0]
    if (control / f"fail_{name}").exists():
        sys.exit(1)
    version = (control / f"version_{name}").read_text().strip()
    out = pathlib.Path(os.environ["DATALIB_DAG_DATA_ROOT"]) / os.environ["DATALIB_DAG_STEP"]
    out.mkdir(parents=True, exist_ok=True)
    def sql(statement):
        done = subprocess.run([cli, str(out / "entities.doltlite_db"), statement],
                              check=True, capture_output=True, text=True)
        return done.stdout.strip()
    sql("CREATE TABLE IF NOT EXISTS seen(version TEXT PRIMARY KEY)")
    if not sql(f"SELECT version FROM seen WHERE version = '{version}'"):
        sql(f"INSERT INTO seen VALUES ('{version}');"
            f"SELECT dolt_commit('-A', '-m', '{version}');")
    print(json.dumps({"event": "outcome",
                      "outputs": [{"path": os.environ["DATALIB_DAG_STEP"],
                                   "version": sql("SELECT dolt_hashof('main')")}]}),
          flush=True)
    """
)


def block_from_doc(doc: Path, first_line: str) -> str:
    blocks = re.findall(r"```python\n(.*?)```", doc.read_text(), flags=re.DOTALL)
    found = [b for b in blocks if b.startswith(first_line)]
    assert len(found) == 1, (
        f"expected one {first_line!r} block in {doc}, found {len(found)}"
    )
    return found[0]


def wheel_dir() -> Path:
    spec = importlib.util.find_spec("doltlite")
    assert spec is not None and spec.origin is not None, "the doltlite wheel is a dep"
    return Path(spec.origin).resolve().parent.parent


class StepProtocolExample(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        base = Path(self.dir.name)
        self.root = base / "root"
        self.control = base / "control"
        self.root.mkdir()
        self.control.mkdir()
        (base / "producer.py").write_text(PRODUCER)
        (base / "example.py").write_text(block_from_doc(DOC, "#!/usr/bin/env python3"))
        (base / "doltlite_reader.py").write_text(
            block_from_doc(DOLTLITE_DOC, "# doltlite_reader.py")
        )
        py = sys.executable
        producer = f"{py} {base / 'producer.py'} {self.control} {CLI}"
        # A step's environment is an allowlist, so the wheel goes on the command.
        example = f"/usr/bin/env PYTHONPATH={wheel_dir()} {py} {base / 'example.py'}"
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
                command = "{example}"
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
        self.assertIn(
            ("info", "a/make moved: a1"), first, "a first run names every input"
        )
        self.assertIn(
            ("info", "b/make moved: b1"), first, "a first run names every input"
        )

        self.set_version("a", "a2")
        second = self.sync()
        self.assertIn(("info", "a/make moved: a2"), second)
        self.assertFalse(
            any(msg.startswith("b/make moved") for _, msg in second), "b did not move"
        )

        (self.control / "fail_a").write_text("")
        self.set_version("b", "b2")
        third = self.sync()
        self.assertIn(("info", "b/make moved: b2"), third)
        self.assertTrue(
            any(
                level == "warn" and msg.startswith("a/make did not finish its last run")
                for level, msg in third
            ),
            f"the example warns about the failed input: {json.dumps(third)}",
        )


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
