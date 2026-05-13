"""`python -m worker` entrypoint.

In production the Rust backend supervises this process the same way it
supervises `dolt sql-server` (Phase E); we connect to that running
Dolt instance through a pymysql pool.
"""

from __future__ import annotations

import logging
import signal
import threading
from pathlib import Path

import pymysql
import typer

from ingest.config import load_config
from ingest.dolt_service import DOLT_REPO_DIRNAME

from .loop import LoopConfig, run_loop

log = logging.getLogger(__name__)


def _make_pymysql_conn_factory(config_path: Path | None):
    cfg = load_config(config_path)
    dolt = cfg.dolt

    def _factory():
        # Worker stays connected to a single Dolt repo; the per-call
        # `cursor()` interface lines up with PyMySQL's Connection.
        return pymysql.connect(
            host=dolt.host,
            port=dolt.port,
            user=dolt.user,
            database=DOLT_REPO_DIRNAME,
            autocommit=False,
        )

    return _factory


def main(
    config: Path | None = typer.Option(
        None, "--config", help="Path to config.yaml (default: project default)."
    ),
    poll_interval_s: float = typer.Option(2.0, "--poll-interval-s"),
    global_cap: int = typer.Option(3, "--global-cap"),
    per_provider_cap: int = typer.Option(1, "--per-provider-cap"),
) -> None:
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
    )
    stop = threading.Event()

    def _on_signal(signum, _frame):
        log.info("received signal %d, stopping", signum)
        stop.set()

    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGINT, _on_signal)

    run_loop(
        _make_pymysql_conn_factory(config),
        config_path=config,
        loop_cfg=LoopConfig(
            poll_interval_s=poll_interval_s,
            global_cap=global_cap,
            per_provider_cap=per_provider_cap,
        ),
        stop=stop,
    )


if __name__ == "__main__":
    typer.run(main)
