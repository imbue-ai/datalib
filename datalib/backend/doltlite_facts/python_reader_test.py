"""imbue-datalib-reader, unpacked from a wheel around the library a release ships,
against stores the pinned CLI wrote.

A py_test, so it runs under rules_python's interpreter: python-build-standalone,
the build `uv python install` fetches, which compiles SQLite in. That is the
Python the reader exists for, and the test asserts it is the one it got."""

import hashlib
import importlib
import importlib.util
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
import zipfile
from pathlib import Path

CLI = Path(sys.argv[1]).resolve()
WHEEL = Path(sys.argv[2]).resolve()


def wheel_dir() -> Path:
    spec = importlib.util.find_spec("doltlite")
    assert spec is not None and spec.origin is not None, "the doltlite wheel is a dep"
    return Path(spec.origin).resolve().parent.parent


READER_DIR = tempfile.TemporaryDirectory()
with zipfile.ZipFile(WHEEL) as wheel_file:
    wheel_file.extractall(READER_DIR.name)
sys.path.insert(0, READER_DIR.name)
reader = importlib.import_module("datalib_reader")


def cli(store: Path, statement: str) -> str:
    done = subprocess.run(
        [str(CLI), str(store), statement], check=True, capture_output=True, text=True
    )
    return done.stdout.strip()


def seal(store: Path, value: str) -> str:
    cli(
        store,
        f"INSERT INTO t VALUES ('{value}');"
        f"SELECT dolt_commit('-A', '-m', 'added {value}');",
    )
    return cli(store, "SELECT dolt_hashof('HEAD');")


def run_python(
    tmp: Path, *args: str, **extra_env: str
) -> subprocess.CompletedProcess[str]:
    env = {"PATH": os.environ.get("PATH", ""), "TMPDIR": str(tmp)}
    env["PYTHONPATH"] = os.pathsep.join([str(wheel_dir()), READER_DIR.name])
    env.update(extra_env)
    return subprocess.run(
        [sys.executable, *args], check=False, capture_output=True, text=True, env=env
    )


class WithAStore(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        # A data root can be named anything, and the name goes into a URI, where
        # `#` and `?` would end the path and `%41` would read as `A`.
        self.dir = Path(tmp.name) / "a b@c #1 %41?"
        self.dir.mkdir()
        self.store = self.dir / "entities.doltlite_db"
        cli(self.store, "CREATE TABLE t(id TEXT PRIMARY KEY);")
        self.first = seal(self.store, "a")
        self.second = seal(self.store, "b")


class ThisPython(WithAStore):
    def test_compiles_sqlite_in(self) -> None:
        self.assertIn("_sqlite3", sys.builtin_module_names)

    def test_the_package_cannot_take_over_the_sqlite3_module_here(self) -> None:
        """When this fails, upstream reads stores under this Python: drop the
        caveat from doltlite.md."""
        script = self.dir / "swap.py"
        script.write_text(
            "import doltlite\n"
            "import sqlite3\n"
            f"sqlite3.connect({str(self.store)!r}).execute('SELECT id FROM t')\n"
        )
        done = run_python(self.dir, str(script))
        self.assertNotEqual(done.returncode, 0, "it read the store")
        if sys.platform == "darwin":
            self.assertIn("has no attribute '__file__'", done.stderr)
        else:
            self.assertIn("file is not a database", done.stderr)


class TheReader(WithAStore):
    def test_reads_main(self) -> None:
        with reader.Store(self.store) as store:
            self.assertEqual(
                store.query("SELECT id FROM t ORDER BY id"), [("a",), ("b",)]
            )

    def test_reads_one_commit_however_far_main_moves(self) -> None:
        with reader.Store(self.store, self.first) as store:
            self.assertEqual(store.query("SELECT id FROM t"), [("a",)])
            seal(self.store, "c")
            self.assertEqual(store.query("SELECT id FROM t"), [("a",)])
            self.assertEqual(store.query("SELECT active_branch()"), [(None,)])

    def test_a_connection_kept_open_sees_a_later_seal(self) -> None:
        with reader.Store(self.store) as store:
            self.assertEqual(store.query("SELECT count(*) FROM t"), [(2,)])
            seal(self.store, "c")
            self.assertEqual(store.query("SELECT count(*) FROM t"), [(3,)])

    def test_refuses_writes_on_both_kinds_of_open(self) -> None:
        for commit in (None, self.first):
            with (
                reader.Store(self.store, commit) as store,
                self.assertRaisesRegex(OSError, "readonly"),
            ):
                store.query("INSERT INTO t VALUES ('z')")

    def test_a_missing_commit_fails_the_open(self) -> None:
        for commit in ("0" * 40, "no-such-branch"):
            with self.assertRaisesRegex(reader.UnknownCommit, "not found"):
                reader.Store(self.store, commit)

    def test_each_failure_has_its_own_class_and_is_still_an_oserror(self) -> None:
        with self.assertRaises(reader.CannotOpen) as opened:
            reader.Store(self.dir / "missing.doltlite_db")
        self.assertNotIsInstance(opened.exception, reader.UnknownCommit)
        with reader.Store(self.store) as store:
            for sql, kind in [
                ("SELEC 1", reader.QueryError),
                ("SELECT * FROM nope", reader.QueryError),
                ("SELECT nope FROM t", reader.QueryError),
                ("INSERT INTO t VALUES ('z')", reader.ReadOnly),
            ]:
                with self.assertRaises(kind, msg=sql) as raised:
                    store.query(sql)
                self.assertIsInstance(raised.exception, OSError)
                self.assertNotEqual(raised.exception.code, 0, sql)

    def test_rows_answer_by_name_and_say_their_columns(self) -> None:
        with reader.Store(self.store) as store:
            rows = store.query(
                "SELECT id, length(id) AS n, id AS id FROM t ORDER BY id"
            )
            self.assertEqual(rows.columns, ("id", "n", "id"))
            self.assertEqual(rows, [("a", 1, "a"), ("b", 1, "b")])
            first = rows[0]
            self.assertEqual((first["n"], first[0], first[-1]), (1, "a", "a"))
            self.assertEqual(first.as_dict(), {"id": "a", "n": 1})
            self.assertEqual(first.keys(), ["id", "n", "id"])
            with self.assertRaises(KeyError):
                first["nope"]
            empty = store.query("SELECT id FROM t WHERE id = 'z'")
            self.assertEqual((empty, empty.columns), ([], ("id",)))

    def test_binds_and_returns_every_type(self) -> None:
        values = (None, 7, 2.5, "é\x00z", b"\x00\xff", b"")
        with reader.Store(self.store) as store:
            self.assertEqual(store.query("SELECT ?, ?, ?, ?, ?, ?", *values), [values])

    def test_reads_history_and_diffs(self) -> None:
        with reader.Store(self.store) as store:
            messages = [m for (m,) in store.query("SELECT message FROM dolt_log")]
            self.assertEqual(messages[:2], ["added b", "added a"])
            self.assertEqual(
                store.query(
                    "SELECT diff_type, to_id FROM dolt_diff_t(?, ?)",
                    self.first,
                    self.second,
                ),
                [("added", "b")],
            )

    def test_leaves_the_store_byte_identical(self) -> None:
        def state() -> tuple[str, list[str]]:
            digest = hashlib.sha256(self.store.read_bytes()).hexdigest()
            return digest, sorted(os.listdir(self.dir))

        before = state()
        for commit in (None, self.first):
            with reader.Store(self.store, commit) as store:
                store.query("SELECT * FROM t WHERE id = ?", "a")
                store.query("SELECT count(*) FROM dolt_log")
        self.assertEqual(state(), before)

    def test_threads_each_with_a_connection(self) -> None:
        answers: list[int] = []

        def read() -> None:
            with reader.Store(self.store) as store:
                for _ in range(50):
                    answers.append(store.query("SELECT count(*) FROM t")[0][0])

        threads = [threading.Thread(target=read) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(answers, [2] * 400)

    def test_its_library_is_the_build_the_shell_is(self) -> None:
        versions = "SELECT sqlite_version() || '|' || dolt_version();"
        with reader.Store(self.store) as store:
            self.assertEqual(store.query(versions), [(cli(self.store, versions),)])

    def test_opens_a_plain_sqlite_file(self) -> None:
        plain = self.dir / "blobs.sqlite"
        with sqlite3.connect(plain) as db:
            db.execute("CREATE TABLE cas_objects(blake3 TEXT)")
            db.execute("INSERT INTO cas_objects VALUES ('x')")
        db.close()
        with reader.Store(plain) as store:
            self.assertEqual(store.query("SELECT blake3 FROM cas_objects"), [("x",)])

    def test_leaves_the_sqlite3_module_stock(self) -> None:
        db = sqlite3.connect(self.store)
        with self.assertRaisesRegex(sqlite3.DatabaseError, "file is not a database"):
            db.execute("SELECT id FROM t")
        db.close()

    def test_needs_no_restart_no_temp_file_and_no_environment(self) -> None:
        """What `import doltlite` costs on macOS, none of it paid here. `-c`
        cannot restart, so this fails if the import starts to."""
        shims = self.dir / "tmp"
        shims.mkdir()
        done = run_python(
            shims,
            "-c",
            "import os\n"
            "from datalib_reader import Store\n"
            f"print(Store({str(self.store)!r}).query('SELECT count(*) FROM t'))\n"
            "print([k for k in os.environ if 'DOLTLITE' in k or k.startswith('DYLD')])",
        )
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.split("\n")[:2], ["[(2,)]", "[]"])
        self.assertEqual(os.listdir(shims), [])


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
