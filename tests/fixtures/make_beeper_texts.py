#!/usr/bin/env python3
"""Lay out the TNG Beeper fixture as a Beeper Texts data directory.

Run as a Bazel genrule
(`//datalib/backend/etl/providers/beeper:tng_beeper_texts`), and imported
by `run_sync_pipeline.py` for the fixture sync. The two
databases are **plain SQLite files**, which is why this is Python and not
Rust (see `make_apple_photos_library.py`).

    make_beeper_texts.py <fixture dir> <target dir>

`<fixture dir>` is `datalib/backend/etl/providers/beeper/tests/fixtures/
beeper_tng`: `index_db.sql`, `local_signal_megabridge.sql` and `media/`.
"""

import shutil
import sqlite3
import sys
from pathlib import Path


def load_sql(sql_path: Path, db_path: Path) -> None:
    db_path.parent.mkdir(parents=True, exist_ok=True)
    if db_path.exists():
        db_path.unlink()
    conn = sqlite3.connect(str(db_path))
    try:
        conn.executescript(sql_path.read_text())
        conn.commit()
    finally:
        conn.close()


def lay_out(fixture: Path, target: Path) -> None:
    load_sql(fixture / "index_db.sql", target / "index.db")
    load_sql(
        fixture / "local_signal_megabridge.sql",
        target / "local-signal" / "megabridge.db",
    )
    shutil.copytree(fixture / "media", target / "media", dirs_exist_ok=True)


def main() -> None:
    lay_out(Path(sys.argv[1]), Path(sys.argv[2]))


if __name__ == "__main__":
    main()
