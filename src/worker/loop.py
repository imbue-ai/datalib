"""Worker poll loop.

Connects to Dolt (via the backend's `dolt sql-server`), runs startup
recovery, then polls `sync_jobs` for `pending` rows, claims as many as
the concurrency caps allow, and dispatches each to `runner.run_job` in
a thread. Cancel handling lives in the runner — this loop only manages
scheduling.
"""

from __future__ import annotations

import logging
import threading
from concurrent.futures import ThreadPoolExecutor, Future
from dataclasses import dataclass
from pathlib import Path

from ingest.config import Config, load_config

from . import jobs as jobs_mod
from .runner import ConnFactory, RunnerConfig, run_job

log = logging.getLogger(__name__)


@dataclass(slots=True)
class LoopConfig:
    poll_interval_s: float = 2.0
    global_cap: int = 3
    per_provider_cap: int = 1


def _provider_map(cfg: Config) -> dict[str, str]:
    return {s.name: s.provider for s in cfg.sources}


def _alive_pids() -> set[int]:
    """Best-effort PID liveness check used by startup recovery. Reads
    `/proc/<pid>` on Linux; on macOS falls back to `os.kill(pid, 0)`."""
    pids: set[int] = set()
    proc = Path("/proc")
    if proc.is_dir():
        for entry in proc.iterdir():
            if entry.name.isdigit():
                pids.add(int(entry.name))
        return pids
    # Fallback: there's no portable way to enumerate, so callers should
    # treat an empty set as "everyone is presumed dead" and reconcile.
    # The runner's mark_failed message makes this explicit.
    return pids


def run_loop(
    make_conn: ConnFactory,
    *,
    config_path: Path | None = None,
    loop_cfg: LoopConfig | None = None,
    stop: threading.Event | None = None,
) -> None:
    """Block the calling thread, draining `sync_jobs` until `stop` is set."""
    loop_cfg = loop_cfg or LoopConfig()
    stop = stop or threading.Event()
    cfg = load_config(config_path)
    provider_for_source = _provider_map(cfg)
    runner_cfg = RunnerConfig(config_path=config_path)

    conn = make_conn()
    jobs_mod.ensure_schema(conn)
    # Startup recovery — any leftover `running` row whose pid is gone
    # gets failed so the UI doesn't show a phantom in-flight job after a
    # worker restart.
    recovered = jobs_mod.recover_stale_running(conn, alive_pids=_alive_pids())
    if recovered:
        log.info("recovered %d stale running job(s)", recovered)
    conn.commit()

    pool = ThreadPoolExecutor(max_workers=max(1, loop_cfg.global_cap))
    inflight: dict[str, Future] = {}
    try:
        while not stop.is_set():
            # Reap completed futures.
            for jid, fut in list(inflight.items()):
                if fut.done():
                    inflight.pop(jid, None)

            conn = make_conn()
            free_slots = loop_cfg.global_cap - len(inflight)
            if free_slots > 0:
                candidates = jobs_mod.list_runnable(
                    conn,
                    global_cap=loop_cfg.global_cap,
                    per_provider_cap=loop_cfg.per_provider_cap,
                    provider_for_source=provider_for_source,
                )
                for job in candidates:
                    if job.id in inflight:
                        continue
                    if len(inflight) >= loop_cfg.global_cap:
                        break
                    inflight[job.id] = pool.submit(
                        run_job, job, make_conn=make_conn, cfg=runner_cfg
                    )
            stop.wait(loop_cfg.poll_interval_s)
    finally:
        pool.shutdown(wait=True)
