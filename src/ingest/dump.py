"""Deterministic SQL dump of the Dolt database.

Goal: emit a byte-stable text artifact suitable as a Bazel genrule output.
The dump is *not* a Dolt-flavored backup (no dolt history, no commit hashes
\u2014 those are intentionally non-deterministic and aren't useful as a
fixture); it's a portable `CREATE TABLE` + sorted `INSERT INTO` script.

Portability target: anything that speaks the SQL subset shared by Dolt,
MySQL, and SQLite. The DDL we emit is the same text that
`generated_grid_rows.DDL` declares \u2014 not the engine-specific output of
`SHOW CREATE TABLE` \u2014 so the dump can be loaded into in-memory SQLite
for fast hermetic tests as well as into a fresh Dolt for end-to-end
fidelity. See `ingest.sqlite_load` for the SQLite loader.

Only `grid_rows` is dumped: per-provider tables no longer exist; the
ingest pipeline now flows entirely through in-memory parsed dataclasses
and writes only the union projection to SQL.

Determinism rules:
  * Rows are ordered by primary key.
  * Each row is one INSERT, columns listed explicitly.
"""

from __future__ import annotations

import json
import re
import textwrap
from pathlib import Path
from typing import Any

from pymysql.connections import Connection

from ingest.generated_documents import DDL as _DOCUMENTS_DDL
from ingest.generated_grid_rows import DDL as _GRID_ROWS_DDL


def _portable_ddl_for(table: str) -> str:
    """Return the portable CREATE TABLE for `table`. Strips the
    `IF NOT EXISTS` clause (we emit `DROP TABLE IF EXISTS` first, so a
    plain CREATE is what we want for a fresh load) and normalizes
    whitespace so the output is stable regardless of how the source
    string was formatted."""
    for stmt in (*_GRID_ROWS_DDL, *_DOCUMENTS_DDL):
        normalized = textwrap.dedent(stmt).strip()
        if re.search(
            rf"\bCREATE TABLE\b\s+(IF\s+NOT\s+EXISTS\s+)?{re.escape(table)}\b",
            normalized,
        ):
            return re.sub(
                r"\bCREATE TABLE\s+IF\s+NOT\s+EXISTS\s+",
                "CREATE TABLE ",
                normalized,
            )
    raise KeyError(f"no portable DDL declared for table {table!r}")


_TABLES = ("grid_rows", "documents")


def _quote_ident(name: str) -> str:
    return "`" + name.replace("`", "``") + "`"


def _quote_value(v: Any) -> str:
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, (bytes, bytearray)):
        return "0x" + bytes(v).hex()
    s: str
    if isinstance(v, str):
        # If it parses as JSON (object/array), canonicalize so logically equal
        # payloads serialize identically across runs and Dolt versions.
        stripped = v.lstrip()
        if stripped[:1] in ("{", "["):
            try:
                parsed = json.loads(v)
                s = json.dumps(
                    parsed, sort_keys=True, ensure_ascii=False, separators=(",", ":")
                )
            except json.JSONDecodeError:
                s = v
        else:
            s = v
    else:
        s = json.dumps(v, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    return (
        "'"
        + s.replace("\\", "\\\\")
        .replace("'", "''")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        + "'"
    )


def _table_exists(conn: Connection, name: str) -> bool:
    with conn.cursor() as cur:
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        cur.execute(
            "SELECT COUNT(*) FROM information_schema.tables "
            "WHERE table_schema = %s AND table_name = %s",
            (db, name),
        )
        (n,) = cur.fetchone()  # type: ignore[misc]
        return bool(n)


def _columns(conn: Connection, table: str) -> list[str]:
    with conn.cursor() as cur:
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        cur.execute(
            "SELECT column_name FROM information_schema.columns "
            "WHERE table_schema = %s AND table_name = %s "
            "ORDER BY ordinal_position",
            (db, table),
        )
        return [r[0] for r in cur.fetchall()]


def _primary_key(conn: Connection, table: str) -> list[str]:
    with conn.cursor() as cur:
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        cur.execute(
            "SELECT column_name FROM information_schema.key_column_usage "
            "WHERE table_schema = %s AND table_name = %s AND constraint_name = 'PRIMARY' "
            "ORDER BY ordinal_position",
            (db, table),
        )
        return [r[0] for r in cur.fetchall()]


def _create_table(conn: Connection, table: str) -> str:
    # We deliberately do *not* use `SHOW CREATE TABLE` here \u2014 Dolt/MySQL
    # emits engine-specific clauses (`ENGINE=InnoDB`,
    # `COLLATE=utf8mb4_0900_bin`, charset annotations) that SQLite rejects.
    # The DDL declared in `providers/<p>/schema.py` is the portable subset
    # the project commits to.
    del conn  # unused; kept for symmetry with the older signature
    return _portable_ddl_for(table)


def dump_sql(conn: Connection, out_path: Path) -> None:
    """Write a deterministic CREATE+INSERT script to `out_path`."""
    lines: list[str] = []
    lines.append("-- Generated by ingest.dump.dump_sql")
    lines.append(
        "-- This file is byte-stable for a given input + schema; do not hand-edit."
    )
    lines.append("")

    for table in _TABLES:
        if not _table_exists(conn, table):
            continue
        cols = _columns(conn, table)
        pk = _primary_key(conn, table) or cols  # fallback: order by all columns
        lines.append(f"-- {table}")
        lines.append(f"DROP TABLE IF EXISTS {_quote_ident(table)};")
        lines.append(_create_table(conn, table) + ";")
        lines.append("")

        with conn.cursor() as cur:
            order_by = ", ".join(_quote_ident(c) for c in pk)
            select_cols = ", ".join(_quote_ident(c) for c in cols)
            cur.execute(
                f"SELECT {select_cols} FROM {_quote_ident(table)} ORDER BY {order_by}"
            )
            rows = cur.fetchall()
        if rows:
            col_list = ", ".join(_quote_ident(c) for c in cols)
            for row in rows:
                vals = ", ".join(_quote_value(v) for v in row)
                lines.append(
                    f"INSERT INTO {_quote_ident(table)} ({col_list}) VALUES ({vals});"
                )
            lines.append("")

    out_path.write_text("\n".join(lines).rstrip() + "\n")
