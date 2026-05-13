"""CRUD for the `download_runs` table.

Each `download` sync_job creates one `download_runs` row at start
(`started_at`, `raw_path`) and updates it on completion (`finished_at`).
The ingest pipeline later stamps `ingested_at` + `doc_uuids_touched`
when it consumes that raw subdir.

Downloader scripts themselves don't touch this table — the worker owns
the writes — so the downloaders stay project-agnostic.
"""

from __future__ import annotations

import json
import uuid
from datetime import datetime
from typing import Any, Iterable, Protocol

from ingest.generated_download_runs import COLUMNS, DDL

_COLUMNS = COLUMNS["download_runs"]


class _ConnLike(Protocol):
    def cursor(self) -> Any: ...


def _now() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def ensure_schema(conn: _ConnLike) -> None:
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)


def insert_started(
    conn: _ConnLike,
    *,
    source_name: str,
    raw_path: str,
    kind: str = "delta",
    run_id: str | None = None,
) -> str:
    """Insert a new `download_runs` row at run start. Returns the row id."""
    rid = run_id or str(uuid.uuid4())
    columns_sql = ", ".join(_COLUMNS)
    placeholders = ", ".join(["%s"] * len(_COLUMNS))
    with conn.cursor() as cur:
        cur.execute(
            f"INSERT INTO download_runs ({columns_sql}) VALUES ({placeholders})",
            (rid, source_name, raw_path, kind, _now(), None, None, None),
        )
    return rid


def mark_finished(conn: _ConnLike, run_id: str) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE download_runs SET finished_at = %s WHERE id = %s",
            (_now(), run_id),
        )


def mark_ingested(conn: _ConnLike, run_id: str, *, doc_uuids: Iterable[str]) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE download_runs SET ingested_at = %s, doc_uuids_touched = %s "
            "WHERE id = %s",
            (_now(), json.dumps(sorted(doc_uuids)), run_id),
        )


def pending_ingest(conn: _ConnLike, source_name: str | None = None) -> list[tuple]:
    """Return rows whose downloader finished but ingest hasn't claimed them
    yet. Tuples are `(id, source_name, raw_path, kind)`."""
    sql = (
        "SELECT id, source_name, raw_path, kind FROM download_runs "
        "WHERE finished_at IS NOT NULL AND ingested_at IS NULL"
    )
    params: tuple = ()
    if source_name is not None:
        sql += " AND source_name = %s"
        params = (source_name,)
    sql += " ORDER BY started_at ASC"
    with conn.cursor() as cur:
        cur.execute(sql, params)
        return list(cur.fetchall())
