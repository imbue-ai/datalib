"""Unit tests for `src/worker/`.

Uses a pymysql-shape sqlite3 shim (same idea as test_orphan_cleanup) so
we can exercise the full job-lifecycle SQL without spinning up Dolt.
"""

from __future__ import annotations

import sqlite3
from contextlib import contextmanager

from worker import download_runs as dl_runs
from worker import jobs as jobs_mod
from worker.runner import RunnerConfig, run_job


class _Cursor:
    def __init__(self, raw: sqlite3.Cursor) -> None:
        self._raw = raw

    def execute(self, sql: str, params: tuple = ()) -> None:
        self._raw.execute(sql.replace("%s", "?"), params)

    def executemany(self, sql: str, seq) -> None:
        self._raw.executemany(sql.replace("%s", "?"), seq)

    def fetchall(self):
        return self._raw.fetchall()

    def fetchone(self):
        return self._raw.fetchone()


class _Conn:
    def __init__(self, db: sqlite3.Connection) -> None:
        self.db = db

    @contextmanager
    def cursor(self):
        cur = self.db.cursor()
        try:
            yield _Cursor(cur)
        finally:
            cur.close()

    def commit(self) -> None:
        self.db.commit()


class _FakeProc:
    """Minimal stand-in for `_Subproc` so the download runner can drive
    state transitions without spawning a real child."""

    def __init__(self, exit_after_polls: int = 1, rc: int = 0) -> None:
        self._remaining = exit_after_polls
        self._rc = rc
        self.terminated = False
        self.pid = 99999

    def poll(self):
        if self._remaining <= 0:
            return self._rc
        self._remaining -= 1
        return None

    def terminate(self) -> None:
        self.terminated = True
        self._remaining = 0

    def wait(self, timeout=None):
        return self._rc


def _shared_factory(db: sqlite3.Connection):
    conn = _Conn(db)

    def factory():
        return conn

    return factory


def _setup():
    db = sqlite3.connect(":memory:")
    factory = _shared_factory(db)
    conn = factory()
    jobs_mod.ensure_schema(conn)  # type: ignore[arg-type]
    dl_runs.ensure_schema(conn)  # type: ignore[arg-type]
    conn.commit()
    return db, factory


def test_enqueue_and_get() -> None:
    db, factory = _setup()
    job = jobs_mod.enqueue(factory(), kind="download", source_name="slack-imbue")
    got = jobs_mod.get(factory(), job.id)
    assert got is not None
    assert got.state == "pending"
    assert got.kind == "download"
    assert got.source_name == "slack-imbue"


def test_list_runnable_respects_global_cap() -> None:
    db, factory = _setup()
    j1 = jobs_mod.enqueue(factory(), kind="download", source_name="a")
    j2 = jobs_mod.enqueue(factory(), kind="download", source_name="b")
    j3 = jobs_mod.enqueue(factory(), kind="download", source_name="c")
    runnable = jobs_mod.list_runnable(
        factory(),
        global_cap=2,
        per_provider_cap=99,
        provider_for_source={"a": "p1", "b": "p2", "c": "p3"},
    )
    assert len(runnable) == 2
    ids = {j.id for j in runnable}
    assert ids.issubset({j1.id, j2.id, j3.id})


def test_list_runnable_respects_per_provider_cap() -> None:
    db, factory = _setup()
    j1 = jobs_mod.enqueue(factory(), kind="download", source_name="a1")
    j2 = jobs_mod.enqueue(factory(), kind="download", source_name="a2")
    j3 = jobs_mod.enqueue(factory(), kind="download", source_name="b1")
    runnable = jobs_mod.list_runnable(
        factory(),
        global_cap=99,
        per_provider_cap=1,
        provider_for_source={"a1": "anth", "a2": "anth", "b1": "slack"},
    )
    ids = {j.id for j in runnable}
    # Provider `anth` allows only one of a1/a2; provider `slack` allows b1.
    # Either a1 or a2 may win (same created_at), tiebroken by id ASC.
    assert len(ids & {j1.id, j2.id}) == 1
    assert j3.id in ids


def test_request_cancel_flips_state() -> None:
    db, factory = _setup()
    job = jobs_mod.enqueue(factory(), kind="download", source_name="x")
    jobs_mod.mark_running(factory(), job.id, pid=123)
    jobs_mod.request_cancel(factory(), job.id)
    assert jobs_mod.is_canceled(factory(), job.id)


def test_recover_stale_running_marks_dead_pids_failed() -> None:
    db, factory = _setup()
    j1 = jobs_mod.enqueue(factory(), kind="download", source_name="x")
    jobs_mod.mark_running(factory(), j1.id, pid=11)
    j2 = jobs_mod.enqueue(factory(), kind="download", source_name="y")
    jobs_mod.mark_running(factory(), j2.id, pid=22)

    touched = jobs_mod.recover_stale_running(factory(), alive_pids={22})
    assert touched == 1
    assert jobs_mod.get(factory(), j1.id).state == "failed"  # type: ignore[union-attr]
    assert jobs_mod.get(factory(), j2.id).state == "running"  # type: ignore[union-attr]


def test_run_ingest_marks_done_and_records_progress() -> None:
    db, factory = _setup()
    job = jobs_mod.enqueue(factory(), kind="ingest", source_name=None)

    state = run_job(
        job,
        make_conn=factory,
        cfg=RunnerConfig(),
        ingest_fn=lambda: 42,
    )
    assert state == "done"
    got = jobs_mod.get(factory(), job.id)
    assert got is not None
    assert got.state == "done"
    assert got.finished_at is not None
    assert got.progress_pct == 1.0
    assert got.progress_msg is not None and "42 rows" in got.progress_msg


def test_run_ingest_marks_failed_on_exception() -> None:
    db, factory = _setup()
    job = jobs_mod.enqueue(factory(), kind="ingest", source_name=None)

    def _boom() -> int:
        raise RuntimeError("kaboom")

    state = run_job(job, make_conn=factory, cfg=RunnerConfig(), ingest_fn=_boom)
    assert state == "failed"
    got = jobs_mod.get(factory(), job.id)
    assert got is not None
    assert got.state == "failed"
    assert got.error is not None and "kaboom" in got.error


def test_download_runs_lifecycle() -> None:
    db, factory = _setup()
    rid = dl_runs.insert_started(
        factory(),
        source_name="slack-imbue",
        raw_path="raw/slack-imbue/2026-05-13T14-22-05-07-00",
    )
    dl_runs.mark_finished(factory(), rid)
    pending = dl_runs.pending_ingest(factory())
    assert len(pending) == 1
    assert pending[0][2] == "raw/slack-imbue/2026-05-13T14-22-05-07-00"
    dl_runs.mark_ingested(factory(), rid, doc_uuids=["d-1", "d-2"])
    assert dl_runs.pending_ingest(factory()) == []
