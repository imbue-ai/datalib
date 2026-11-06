#!/usr/bin/env python3
"""What check_reader_wheels.sh runs inside each image, after installing the wheel
there: the installer chose the wheel for this platform, its own library loaded, and
it reads a store with `sqlite3` stock beside it. The module's behaviour in depth
is //datalib/backend/doltlite_facts:python_reader_test's."""

from __future__ import annotations

import importlib.metadata
import os
import platform
import subprocess
import sys
import threading
from pathlib import Path

STORE = os.environ["STORE"]
FIRST_COMMIT = os.environ["FIRST_COMMIT"]
WANT_PLATFORM = os.environ["WANT_PLATFORM"]
STOCK_SQLITE = os.environ.get("STOCK_SQLITE")

BESIDE_SQLITE3 = """
import sys
order = sys.argv[1].split(",")
for name in order:
    __import__(name)
import datalib_reader, sqlite3
with datalib_reader.Store(sys.argv[2]) as store:
    assert store.query("SELECT count(*) FROM t") == [(2,)]
try:
    sqlite3.connect(sys.argv[2]).execute("SELECT id FROM t")
except sqlite3.DatabaseError as error:
    assert "file is not a database" in str(error), error
else:
    raise SystemExit("the sqlite3 module read a store")
"""

# Stock SQLite already answering in the process's global symbols. musl has no
# RTLD_DEEPBIND, so this holds there only because the library is linked
# -Bsymbolic, and without that it segfaults.
BESIDE_GLOBAL_SQLITE = """
import ctypes, os, sys
ctypes.CDLL(sys.argv[1], mode=os.RTLD_GLOBAL)
import datalib_reader
with datalib_reader.Store(sys.argv[2]) as store:
    assert store.query("SELECT count(*) FROM t") == [(2,)]
"""


def main() -> None:
    wheel_tags = importlib.metadata.distribution("imbue-datalib-reader").read_text(
        "WHEEL"
    )
    assert wheel_tags is not None
    assert f"-{WANT_PLATFORM}" in wheel_tags, f"the wrong wheel:\n{wheel_tags}"

    from datalib_reader import Store, _lib  # pyright: ignore[reportMissingImports]

    library = Path(_lib._name)
    assert library.parent.name == "datalib_reader", f"loaded {library}"

    with Store(STORE) as store:
        assert store.query("SELECT id FROM t ORDER BY id") == [("a",), ("b",)]
        messages = [m for (m,) in store.query("SELECT message FROM dolt_log")]
        assert messages[:2] == ["added b", "added a"], messages
        versions = store.query("SELECT sqlite_version(), dolt_version()")[0]
    with Store(STORE, FIRST_COMMIT) as store:
        assert store.query("SELECT id FROM t") == [("a",)]

    try:
        with Store(STORE) as store:
            store.query("INSERT INTO t VALUES ('z')")
    except OSError as error:
        assert "readonly" in str(error), error
    else:
        raise AssertionError("a write succeeded")

    answers: list[int] = []

    def read() -> None:
        with Store(STORE) as store:
            for _ in range(50):
                answers.append(store.query("SELECT count(*) FROM t")[0][0])

    threads = [threading.Thread(target=read) for _ in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert answers == [2] * 400, "a thread's read failed"

    # Each order in a process of its own, the only way the import order counts.
    for order in ("sqlite3,datalib_reader", "datalib_reader,sqlite3"):
        subprocess.run(
            [sys.executable, "-I", "-c", BESIDE_SQLITE3, order, STORE], check=True
        )
    if STOCK_SQLITE:
        subprocess.run(
            [sys.executable, "-I", "-c", BESIDE_GLOBAL_SQLITE, STOCK_SQLITE, STORE],
            check=True,
        )

    print(
        f"OK: Python {platform.python_version()} on {platform.platform()}, "
        f"{library.name} from the {WANT_PLATFORM} wheel, "
        f"SQLite {versions[0]}, doltlite {versions[1]}"
    )


if __name__ == "__main__":
    main()
