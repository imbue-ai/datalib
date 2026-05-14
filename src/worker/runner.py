"""Execute a single `sync_jobs` row to completion.

This is split out from `loop.py` so unit tests can drive a job through
its state transitions without spinning up the polling loop. The runner
takes a callable `make_conn` rather than a live connection because Dolt
sessions are per-connection (working set isolation) — long-running jobs
re-acquire as needed.
"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Protocol

from ingest.run_source import TYPE_TO_MODULE, resolve, sync_to_argv

from . import download_runs as dl_runs
from . import jobs as jobs_mod

log = logging.getLogger(__name__)


class _ConnLike(Protocol):
    def cursor(self) -> Any: ...

    def commit(self) -> None: ...


ConnFactory = Callable[[], _ConnLike]


@dataclass(slots=True)
class RunnerConfig:
    """Knobs the runner reads at job start. Keeping these on a dataclass
    makes them easy to override in tests."""

    config_path: Path | None = None
    cancel_poll_interval_s: float = 0.5


class _Subproc:
    """Indirection over `subprocess.Popen` so tests can inject a fake
    child without touching the OS."""

    def __init__(self, popen: subprocess.Popen) -> None:
        self._popen = popen

    @property
    def pid(self) -> int:
        return self._popen.pid

    def poll(self) -> int | None:
        return self._popen.poll()

    def terminate(self) -> None:
        try:
            self._popen.terminate()
        except ProcessLookupError:
            pass

    def wait(self, timeout: float | None = None) -> int:
        return self._popen.wait(timeout=timeout)


SpawnFn = Callable[[list[str], Path], _Subproc]


def _real_spawn(argv: list[str], _cwd: Path) -> _Subproc:
    return _Subproc(subprocess.Popen(argv))


def run_job(
    job: jobs_mod.SyncJob,
    *,
    make_conn: ConnFactory,
    cfg: RunnerConfig | None = None,
    spawn: SpawnFn = _real_spawn,
    ingest_fn: Callable[[], int] | None = None,
) -> str:
    """Drive `job` to a terminal state. Returns the final state string.

    `ingest_fn` lets tests stub out the in-process ingest invocation for
    `ingest`/`render` jobs. In production it's None and the runner pulls
    the real `ingest.ingest.ingest()` lazily.
    """
    cfg = cfg or RunnerConfig()
    try:
        if job.kind == "download":
            return _run_download(job, make_conn, cfg, spawn)
        if job.kind in ("ingest", "render"):
            return _run_ingest(job, make_conn, cfg, ingest_fn)
        if job.kind == "all":
            return _run_all(job, make_conn)
        raise ValueError(f"unknown job kind: {job.kind!r}")
    except Exception as e:  # noqa: BLE001
        log.exception("job %s failed", job.id)
        conn = make_conn()
        jobs_mod.mark_failed(conn, job.id, error=str(e))
        conn.commit()
        return "failed"


def _run_download(
    job: jobs_mod.SyncJob,
    make_conn: ConnFactory,
    cfg: RunnerConfig,
    spawn: SpawnFn,
) -> str:
    if job.source_name is None:
        raise ValueError("download job requires source_name")

    src, out_dir = resolve(job.source_name, cfg.config_path)
    module = TYPE_TO_MODULE[src.type]
    argv = [sys.executable, "-m", module, *sync_to_argv(src, out_dir)]
    proc = spawn(argv, out_dir)
    raw_rel = str(out_dir)

    conn = make_conn()
    jobs_mod.mark_running(conn, job.id, pid=proc.pid)
    jobs_mod.update_progress(conn, job.id, pct=0.0, msg=f"starting {src.type}")
    run_id = dl_runs.insert_started(conn, source_name=job.source_name, raw_path=raw_rel)
    conn.commit()

    while True:
        rc = proc.poll()
        if rc is not None:
            break
        conn = make_conn()
        if jobs_mod.is_canceled(conn, job.id):
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass
            jobs_mod.mark_canceled(
                conn, job.id, error="canceled by user; child SIGTERM'd"
            )
            conn.commit()
            return "canceled"
        time.sleep(cfg.cancel_poll_interval_s)

    conn = make_conn()
    if rc == 0:
        dl_runs.mark_finished(conn, run_id)
        jobs_mod.update_progress(conn, job.id, pct=1.0, msg="download complete")
        jobs_mod.mark_done(conn, job.id)
        conn.commit()
        return "done"
    jobs_mod.mark_failed(conn, job.id, error=f"downloader exited rc={rc}")
    conn.commit()
    return "failed"


def _run_ingest(
    job: jobs_mod.SyncJob,
    make_conn: ConnFactory,
    cfg: RunnerConfig,
    ingest_fn: Callable[[], int] | None,
) -> str:
    conn = make_conn()
    jobs_mod.mark_running(conn, job.id, pid=os.getpid())
    jobs_mod.update_progress(conn, job.id, pct=0.0, msg="ingest starting")
    conn.commit()

    if ingest_fn is None:
        from ingest.config import load_config
        from ingest.ingest import ingest as do_ingest

        def _real_ingest() -> int:
            cfgobj = load_config(cfg.config_path)
            summary = do_ingest(cfgobj)
            return summary.grid_rows

        ingest_fn = _real_ingest

    # In v0, ingest is non-preemptible — cancel only takes effect at the
    # job boundary (the plan's open question defaults to this).
    rows = ingest_fn()

    conn = make_conn()
    jobs_mod.update_progress(
        conn, job.id, pct=1.0, msg=f"ingest complete ({rows} rows)"
    )
    jobs_mod.mark_done(conn, job.id)
    conn.commit()
    return "done"


def _run_all(job: jobs_mod.SyncJob, make_conn: ConnFactory) -> str:
    """Parent marker job. The supervising backend route inserts the
    per-source download children + a final ingest job in one transaction
    (Phase E `POST /api/sync/jobs/all` handler); this runner waits for
    them to settle. The Phase F UI groups by `parent_job_id` for rollup."""
    conn = make_conn()
    jobs_mod.mark_running(conn, job.id, pid=os.getpid())
    while True:
        with conn.cursor() as cur:
            cur.execute(
                "SELECT state, COUNT(*) FROM sync_jobs WHERE parent_job_id = %s "
                "GROUP BY state",
                (job.id,),
            )
            buckets = dict(cur.fetchall())
        non_terminal = buckets.get("pending", 0) + buckets.get("running", 0)
        if non_terminal == 0:
            if buckets.get("failed", 0) > 0:
                jobs_mod.mark_failed(
                    conn, job.id, error="one or more child jobs failed"
                )
                conn.commit()
                return "failed"
            if buckets.get("canceled", 0) > 0:
                jobs_mod.mark_canceled(conn, job.id, error="children canceled")
                conn.commit()
                return "canceled"
            jobs_mod.mark_done(conn, job.id)
            conn.commit()
            return "done"
        if jobs_mod.is_canceled(conn, job.id):
            with conn.cursor() as cur:
                cur.execute(
                    "UPDATE sync_jobs SET state = 'canceled' "
                    "WHERE parent_job_id = %s "
                    "AND state IN ('pending', 'running')",
                    (job.id,),
                )
            conn.commit()
            return "canceled"
        time.sleep(1.0)
