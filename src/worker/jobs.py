"""CRUD for the `sync_jobs` table — the producer-consumer queue between
the backend (which inserts `pending` rows when the UI clicks Sync) and
the worker (which drains them).

The shape of a job lifecycle:

    pending  --claim--> running  --finish_*--> done | failed
                            \\
                             --observe_cancel--> canceled

Schema source of truth is `schemas/sync_jobs.schema.json`; this module
just talks to it. All writes use `cur.execute(sql, tuple)` with `%s`
placeholders so the same SQL works under PyMySQL (production / Dolt)
and the sqlite3-shim used by the unit tests.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Protocol

from ingest.generated_sync_jobs import COLUMNS, DDL

_COLUMNS = COLUMNS["sync_jobs"]


class _ConnLike(Protocol):
    def cursor(self) -> Any: ...


def _now() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def ensure_schema(conn: _ConnLike) -> None:
    """Create the `sync_jobs` table if it doesn't exist. Idempotent."""
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)


@dataclass(slots=True)
class SyncJob:
    id: str
    source_name: str | None
    kind: str
    parent_job_id: str | None
    state: str
    created_at: str
    started_at: str | None
    finished_at: str | None
    error: str | None
    pid: int | None
    progress_pct: float | None
    progress_msg: str | None


def _row_to_job(row: tuple) -> SyncJob:
    return SyncJob(*row)


def enqueue(
    conn: _ConnLike,
    *,
    kind: str,
    source_name: str | None,
    parent_job_id: str | None = None,
    job_id: str | None = None,
) -> SyncJob:
    """Insert a fresh `pending` job. Returns the row as written."""
    job = SyncJob(
        id=job_id or str(uuid.uuid4()),
        source_name=source_name,
        kind=kind,
        parent_job_id=parent_job_id,
        state="pending",
        created_at=_now(),
        started_at=None,
        finished_at=None,
        error=None,
        pid=None,
        progress_pct=None,
        progress_msg=None,
    )
    columns_sql = ", ".join(_COLUMNS)
    placeholders = ", ".join(["%s"] * len(_COLUMNS))
    with conn.cursor() as cur:
        cur.execute(
            f"INSERT INTO sync_jobs ({columns_sql}) VALUES ({placeholders})",
            (
                job.id,
                job.source_name,
                job.kind,
                job.parent_job_id,
                job.state,
                job.created_at,
                job.started_at,
                job.finished_at,
                job.error,
                job.pid,
                job.progress_pct,
                job.progress_msg,
            ),
        )
    return job


def get(conn: _ConnLike, job_id: str) -> SyncJob | None:
    columns_sql = ", ".join(_COLUMNS)
    with conn.cursor() as cur:
        cur.execute(f"SELECT {columns_sql} FROM sync_jobs WHERE id = %s", (job_id,))
        row = cur.fetchone()
    return _row_to_job(row) if row else None


def list_runnable(
    conn: _ConnLike,
    *,
    global_cap: int,
    per_provider_cap: int,
    provider_for_source: dict[str, str],
) -> list[SyncJob]:
    """Return the prefix of pending jobs the worker may claim right now,
    after subtracting currently-running jobs from the caps. Order is
    `created_at ASC` so older requests run first.

    `provider_for_source` maps `sources[].name` -> `sources[].provider`
    from the resolved config — needed because the per-provider cap groups
    by provider, but jobs only carry `source_name`."""
    columns_sql = ", ".join(_COLUMNS)
    with conn.cursor() as cur:
        cur.execute(
            f"SELECT {columns_sql} FROM sync_jobs "
            f"WHERE state IN ('pending', 'running') "
            f"ORDER BY created_at ASC, id ASC"
        )
        rows = [_row_to_job(r) for r in cur.fetchall()]

    running = [j for j in rows if j.state == "running"]
    pending = [j for j in rows if j.state == "pending"]

    running_provider_count: dict[str, int] = {}
    for j in running:
        prov = provider_for_source.get(j.source_name or "", "")
        if prov:
            running_provider_count[prov] = running_provider_count.get(prov, 0) + 1

    runnable: list[SyncJob] = []
    global_used = len(running)
    for j in pending:
        if global_used >= global_cap:
            break
        prov = provider_for_source.get(j.source_name or "", "")
        if prov:
            if running_provider_count.get(prov, 0) >= per_provider_cap:
                continue
            running_provider_count[prov] = running_provider_count.get(prov, 0) + 1
        runnable.append(j)
        global_used += 1
    return runnable


def mark_running(conn: _ConnLike, job_id: str, *, pid: int | None) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET state = 'running', started_at = %s, pid = %s "
            "WHERE id = %s AND state = 'pending'",
            (_now(), pid, job_id),
        )


def update_progress(
    conn: _ConnLike, job_id: str, *, pct: float | None, msg: str | None
) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET progress_pct = %s, progress_msg = %s WHERE id = %s",
            (pct, msg, job_id),
        )


def mark_done(conn: _ConnLike, job_id: str) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET state = 'done', finished_at = %s, pid = NULL "
            "WHERE id = %s",
            (_now(), job_id),
        )


def mark_failed(conn: _ConnLike, job_id: str, *, error: str) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET state = 'failed', finished_at = %s, "
            "pid = NULL, error = %s WHERE id = %s",
            (_now(), error, job_id),
        )


def mark_canceled(conn: _ConnLike, job_id: str, *, error: str | None = None) -> None:
    """Worker side: transition a job that was flipped to `canceled` (or that
    the worker preempted itself) into its terminal state with a finish
    timestamp. Idempotent — running this on an already-final row is a
    no-op."""
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET state = 'canceled', finished_at = %s, "
            "pid = NULL, error = COALESCE(error, %s) "
            "WHERE id = %s AND state IN ('pending', 'running', 'canceled')",
            (_now(), error, job_id),
        )


def request_cancel(conn: _ConnLike, job_id: str) -> None:
    """API side: flag a pending/running job for cancellation. The worker
    notices the state change on its next poll and SIGTERMs its child."""
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE sync_jobs SET state = 'canceled' "
            "WHERE id = %s AND state IN ('pending', 'running')",
            (job_id,),
        )


def is_canceled(conn: _ConnLike, job_id: str) -> bool:
    with conn.cursor() as cur:
        cur.execute("SELECT state FROM sync_jobs WHERE id = %s", (job_id,))
        row = cur.fetchone()
    return bool(row) and row[0] == "canceled"


def recover_stale_running(conn: _ConnLike, *, alive_pids: set[int]) -> int:
    """Worker startup recovery: any row in `running` whose `pid` is not in
    `alive_pids` is flipped to `failed` with a recovery note. Returns the
    number of rows touched."""
    columns_sql = ", ".join(_COLUMNS)
    with conn.cursor() as cur:
        cur.execute(f"SELECT {columns_sql} FROM sync_jobs WHERE state = 'running'")
        running = [_row_to_job(r) for r in cur.fetchall()]
    touched = 0
    for j in running:
        if j.pid is None or j.pid not in alive_pids:
            mark_failed(conn, j.id, error="worker restarted; pid not alive")
            touched += 1
    return touched
