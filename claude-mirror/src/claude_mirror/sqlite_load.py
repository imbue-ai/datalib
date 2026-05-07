"""Load a `dump.sql` (produced by `claude_mirror.dump.dump_sql`) into an
in-memory SQLite database.

Why SQLite? The dump is the canonical projection of ingested data and is
deliberately written in the SQL subset that Dolt, MySQL, *and* SQLite all
accept. Downstream tests (and demos, and exploratory queries) that just
need to read the data don't need a Dolt subprocess \u2014 they can spin up an
in-memory SQLite in microseconds, get full SQL, and stay hermetic.

What still needs Dolt: the upstream ingest pipeline (writes via
`pymysql` into a Dolt sql-server) and the `dump.sql` artifact itself.
Once the dump exists, every consumer is portable.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path


def load_dump_into_memory(dump_path: Path) -> sqlite3.Connection:
    """Return a fresh in-memory SQLite connection with the dump applied.

    The connection has `row_factory = sqlite3.Row` so callers can index
    columns by name. JSON columns come back as plain strings; use
    `json.loads` (or SQLite's `json1` `json_extract`) at the call site.
    """
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(Path(dump_path).read_text())
    return conn
