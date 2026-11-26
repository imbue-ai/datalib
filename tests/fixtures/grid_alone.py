"""Empties the search index's tables in a grid index file, in place.

A sync's `unified_grid_and_search_index` writes both the grid and the search
index's keyword tables into one file, and the tables are part of the grid
index's shape. The fixture keeps the grid alone as `backend_index.sqlite`,
its keyword tables empty, so a test can stand for a root no keyword pass has
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
                "AND name LIKE 'search\\_%' ESCAPE '\\' "
                "AND name NOT LIKE 'search\\_fts\\_%' ESCAPE '\\'"
            )
        ]
        for name in tables:
            # A contentless FTS table deletes its postings with its rows.
            conn.execute(f'DELETE FROM "{name}"')
        # Out of the DELETEs' transaction, or the journal mode does not move.
        conn.commit()
        conn.execute("PRAGMA journal_mode=DELETE")
    return 0


if __name__ == "__main__":
    sys.exit(main())
