#!/usr/bin/env python3
"""What docs/dev/doltlite.md § "Reading a store from Python" tells a Python
program to do, checked with the pinned PyPI wheel against a store the pinned
CLI wrote. It runs under the host's python3, which the BUILD file explains."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CLI = Path(os.environ["DOLTLITE_BIN"]).resolve()
WHEEL_ROOT = next(
    Path(f).resolve().parent.parent
    for f in os.environ["DOLTLITE_WHEEL_FILES"].split()
    if f.endswith("doltlite/__init__.py")
)
sys.path.insert(0, str(WHEEL_ROOT))

# isort: off
# doltlite first: it must load before sqlite3 does.
import doltlite  # pyright: ignore[reportMissingImports]
import sqlite3
# isort: on

# What the package's bootstrap leaves in the environment; a child process
# that inherits it skips its own bootstrap.
BOOTSTRAP_ENV = ("_DOLTLITE_BOOTSTRAPPED", "DYLD_INSERT_LIBRARIES")


def fresh_env(**extra: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in BOOTSTRAP_ENV}
    env["PYTHONPATH"] = str(WHEEL_ROOT)
    env.update(extra)
    return env


def cli(*args: str) -> str:
    done = subprocess.run(
        [str(CLI), *args], check=True, capture_output=True, text=True, env=fresh_env()
    )
    return done.stdout.strip()


def ids(conn: sqlite3.Connection) -> list[str]:
    return [row[0] for row in conn.execute("SELECT id FROM t ORDER BY id")]


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class TheWheel(unittest.TestCase):
    def test_is_the_doltlite_the_cli_was_built_from(self) -> None:
        # "DoltLite 0.51.3 (SQLite 3.54.0, 64-bit)"
        _, version, _, sqlite_version = cli("--version").split()[:4]
        self.assertEqual(doltlite.__version__, version)
        self.assertEqual(sqlite3.sqlite_version, sqlite_version.rstrip(","))

    def test_puts_the_dolt_surface_under_the_standard_sqlite3_module(self) -> None:
        conn = sqlite3.connect(":memory:")
        branches = conn.execute("SELECT name FROM dolt_branches").fetchall()
        self.assertEqual(branches, [("main",)])


class AStoreTheCliWrote(unittest.TestCase):
    tmp: tempfile.TemporaryDirectory[str]
    store: Path
    first: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        cls.store = Path(cls.tmp.name) / "entities.doltlite_db"
        db = str(cls.store)
        cli(
            db,
            "CREATE TABLE t(id TEXT PRIMARY KEY, v INTEGER);"
            "INSERT INTO t VALUES ('a', 1);"
            "SELECT dolt_commit('-A', '-m', 'one');",
        )
        cls.first = cli(db, "SELECT dolt_hashof('HEAD');")
        cli(db, "INSERT INTO t VALUES ('b', 2);SELECT dolt_commit('-A', '-m', 'two');")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.tmp.cleanup()

    def open_main(self) -> sqlite3.Connection:
        return sqlite3.connect(f"file:{self.store}?mode=ro", uri=True)

    def open_first(self, flags: str = "?mode=ro") -> sqlite3.Connection:
        return sqlite3.connect(f"file:{self.store}@{self.first}{flags}", uri=True)

    def test_a_read_only_open_reads_main(self) -> None:
        self.assertEqual(ids(self.open_main()), ["a", "b"])

    def test_a_read_only_open_refuses_writes(self) -> None:
        with self.assertRaisesRegex(sqlite3.OperationalError, "readonly"):
            self.open_main().execute("INSERT INTO t VALUES ('c', 3)")

    def test_a_detached_open_reads_that_commit(self) -> None:
        conn = self.open_first()
        self.assertEqual(ids(conn), ["a"])
        self.assertIsNone(conn.execute("SELECT active_branch()").fetchone()[0])

    def test_a_detached_open_reads_the_history(self) -> None:
        messages = [
            m for (m,) in self.open_main().execute("SELECT message FROM dolt_log")
        ]
        self.assertEqual(messages[:2], ["two", "one"])

    def test_a_detached_open_refuses_writes_without_mode_ro(self) -> None:
        with self.assertRaisesRegex(sqlite3.OperationalError, "readonly"):
            self.open_first(flags="").execute("INSERT INTO t VALUES ('c', 3)")

    def test_reading_leaves_the_store_byte_identical(self) -> None:
        before = digest(self.store), sorted(os.listdir(self.tmp.name))
        for conn in (self.open_main(), self.open_first()):
            conn.execute("SELECT * FROM t WHERE id = ?", ("a",)).fetchall()
            conn.execute("SELECT count(*) FROM dolt_log").fetchall()
            conn.close()
        self.assertEqual(
            (digest(self.store), sorted(os.listdir(self.tmp.name))), before
        )


class StartingPython(unittest.TestCase):
    def run_python(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, *args],
            check=False,
            capture_output=True,
            text=True,
            env=fresh_env(**env),
        )

    def test_a_child_process_inherits_what_the_bootstrap_leaves(self) -> None:
        expected = ["_DOLTLITE_BOOTSTRAPPED"]
        if sys.platform == "darwin":
            expected.append("DYLD_INSERT_LIBRARIES")
        self.assertEqual([k for k in BOOTSTRAP_ENV if k in os.environ], expected)

    def test_a_child_python_with_only_the_marker_cannot_read_a_store(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "entities.doltlite_db"
            cli(str(store), "CREATE TABLE t(id TEXT PRIMARY KEY);")
            script = Path(tmp) / "child.py"
            script.write_text(
                "import doltlite\n"
                "import sqlite3\n"
                f"sqlite3.connect({str(store)!r}).execute('SELECT id FROM t')\n"
            )
            done = self.run_python(str(script), _DOLTLITE_BOOTSTRAPPED="1")
            self.assertNotEqual(done.returncode, 0, "it read the store")
            self.assertIn("file is not a database", done.stderr)

    def test_the_direct_reader_works_beside_a_shared_library_sqlite3(self) -> None:
        """This host Python loads SQLite as a shared library, so the two engines
        could collide here as they cannot under python_reader_test's. The reader
        must read and the sqlite3 module stay stock, whichever loaded first and
        whichever library it found: the wheel's, or the one a release ships."""
        doc = Path(os.environ["DOLTLITE_DOC"]).read_text()
        reader = next(
            b
            for b in re.findall(r"```python\n(.*?)```", doc, flags=re.DOTALL)
            if b.startswith("# doltlite_reader.py")
        )
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "entities.doltlite_db")
            cli(store, "CREATE TABLE t(id TEXT PRIMARY KEY);")
            (Path(tmp) / "doltlite_reader.py").write_text(reader)
            tree = Path(tmp) / "datalib"
            (tree / "lib").mkdir(parents=True)
            shutil.copy(CLI, tree / "datalib-doltlite")
            shipped = Path(os.environ["DOLTLITE_SHIPPED_LIBRARY"]).resolve()
            shutil.copy(shipped, tree / "lib" / shipped.name)
            use_both = (
                "print(doltlite_reader._lib._name)\n"
                f"with doltlite_reader.Store({store!r}) as s:\n"
                "    assert s.query('SELECT count(*) FROM t') == [(0,)]\n"
                "try:\n"
                f"    sqlite3.connect({store!r}).execute('SELECT id FROM t')\n"
                "except sqlite3.DatabaseError as e:\n"
                "    assert 'file is not a database' in str(e), e\n"
                "else:\n"
                "    raise SystemExit('the sqlite3 module read a store')\n"
            )
            for name, imports in (
                ("sqlite3_first.py", "import sqlite3\nimport doltlite_reader\n"),
                ("reader_first.py", "import doltlite_reader\nimport sqlite3\n"),
            ):
                script = Path(tmp) / name
                script.write_text(imports + use_both)
                for path, library in (
                    (os.environ["PATH"], WHEEL_ROOT / "doltlite" / "_lib"),
                    (str(tree), tree / "lib"),
                ):
                    done = self.run_python(str(script), PATH=path)
                    self.assertEqual(done.returncode, 0, f"{name}: {done.stderr}")
                    self.assertEqual(
                        Path(done.stdout.split("\n")[0]).parent.resolve(),
                        library.resolve(),
                    )

    def test_python_c_fails_on_macos_only(self) -> None:
        done = self.run_python("-c", "import doltlite")
        if sys.platform == "darwin":
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("DoltliteLoadError", done.stderr)
        else:
            self.assertEqual(done.returncode, 0, done.stderr)

    @unittest.skipUnless(sys.platform == "darwin", "the shim is macOS's")
    def test_macos_copies_the_library_on_every_start(self) -> None:
        """dolthub/doltlite-python#2. When this fails, upstream reuses the
        copy: drop the caveat from doltlite.md."""
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "one.py"
            script.write_text("import doltlite\nimport sqlite3\n")
            shims = Path(tmp) / "shims"
            shims.mkdir()
            for _ in range(2):
                done = self.run_python(str(script), TMPDIR=str(shims))
                self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(len(list(shims.glob("doltlite-shim-*"))), 2)


if __name__ == "__main__":
    print(f"host python: {sys.executable} {sys.version.split()[0]}", file=sys.stderr)
    unittest.main()
