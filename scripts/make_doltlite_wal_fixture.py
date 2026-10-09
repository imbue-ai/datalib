"""Write `wal.sqlite` and `wal.sqlite-wal` into
`datalib/backend/doltlite_facts/wal_fixture/`: a stock-SQLite file in WAL
mode whose first row is in the main file and whose next two are only in the
WAL, as qmd's index is between checkpoints. `doltlite_facts_test` reads them
through doltlite, which cannot write a WAL itself.

Run with any Python whose `sqlite3` is stock SQLite:
`python3 scripts/make_doltlite_wal_fixture.py`.
"""

import os
import shutil
import sqlite3
import tempfile

OUT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "datalib/backend/doltlite_facts/wal_fixture",
)


def main() -> None:
    work = tempfile.mkdtemp()
    db = os.path.join(work, "wal.sqlite")
    conn = sqlite3.connect(db, isolation_level=None)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA wal_autocheckpoint=0")
    conn.execute("CREATE TABLE crew (name TEXT)")
    conn.execute("INSERT INTO crew VALUES ('Picard')")
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.execute("INSERT INTO crew VALUES ('Riker')")
    conn.execute("INSERT INTO crew VALUES ('Data')")
    # Copied while the connection is open: closing it would checkpoint the
    # WAL into the main file and delete it.
    for name in ("wal.sqlite", "wal.sqlite-wal"):
        shutil.copy(os.path.join(work, name), os.path.join(OUT, name))
    conn.close()
    shutil.rmtree(work)


if __name__ == "__main__":
    main()
