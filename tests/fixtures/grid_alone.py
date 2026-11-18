"""Drops the search index's tables from a grid index file, in place.

A sync's `unified_grid_and_search_index` writes both the grid and the search index's keyword
tables into one file. The fixture keeps the grid alone as
`backend_index.sqlite`, so a test can stand for a root no keyword pass has
run on (`copy_grid_index` in the search fixture), and
`build_search_index.py` runs the keyword pass over it again.

Usage: grid_alone.py <db.sqlite>
"""

import sqlite3
import sys


def main() -> int:
    (db,) = sys.argv[1:]
    with sqlite3.connect(db) as conn:
        tables = [
            name
            for (name,) in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' "
                "AND name LIKE 'search\\_%' ESCAPE '\\' ORDER BY sql LIKE 'CREATE VIRTUAL%' DESC"
            )
        ]
        for name in tables:
            # A virtual table takes its shadow tables with it.
            conn.execute(f'DROP TABLE IF EXISTS "{name}"')
        conn.execute("PRAGMA journal_mode=DELETE")
    return 0


if __name__ == "__main__":
    sys.exit(main())
