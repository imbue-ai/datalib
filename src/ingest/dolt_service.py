from __future__ import annotations

import shutil
import socket
import subprocess
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

import pymysql
from pymysql.connections import Connection

from ingest.config import Config

DOLT_REPO_DIRNAME = "dolt_repo"  # dolt exposes the dir as a database; avoid leading dot/dash


def _port_open(host: str, port: int, timeout: float = 0.3) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


class DoltService:
    """Manages the dolt sql-server lifecycle for one repo under <root>/.dolt-repo/.

    Idempotent: reuses an already-running server on the configured port
    (verified by issuing a SELECT). Otherwise spawns one.
    """

    def __init__(self, config: Config) -> None:
        self._config = config
        self._repo_dir: Path = config.root / DOLT_REPO_DIRNAME
        self._proc: subprocess.Popen[bytes] | None = None
        self._owns_server = False

    @property
    def repo_dir(self) -> Path:
        return self._repo_dir

    def __enter__(self) -> DoltService:
        self._ensure_dolt_installed()
        self._ensure_repo_initialized()
        if _port_open(self._config.dolt.host, self._config.dolt.port):
            # Assume an existing server we can attach to. Verify with a connection.
            try:
                with self._raw_connect() as c:
                    c.ping(reconnect=False)
                self._owns_server = False
                return self
            except Exception:
                pass
        self._spawn_server()
        self._owns_server = True
        return self

    def __exit__(self, *exc: object) -> None:
        if self._proc is not None and self._owns_server:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self._proc.kill()
                self._proc.wait()
        self._proc = None

    @staticmethod
    def _ensure_dolt_installed() -> None:
        if shutil.which("dolt") is None:
            raise RuntimeError(
                "`dolt` not found on PATH. Install via `brew install dolt` or see https://docs.dolthub.com/."
            )

    def _ensure_repo_initialized(self) -> None:
        self._repo_dir.mkdir(parents=True, exist_ok=True)
        if not (self._repo_dir / ".dolt").exists():
            subprocess.run(
                ["dolt", "init", "--name", "personal-mirror", "--email", "personal-mirror@local"],
                cwd=self._repo_dir,
                check=True,
                capture_output=True,
            )

    def _spawn_server(self) -> None:
        log_dir = self._repo_dir / "logs"
        log_dir.mkdir(exist_ok=True)
        log_path = log_dir / "dolt-sql-server.log"
        log_fh = log_path.open("ab", buffering=0)
        cmd = [
            "dolt",
            "sql-server",
            "--host",
            self._config.dolt.host,
            "--port",
            str(self._config.dolt.port),
            "--no-auto-commit",  # we manage dolt commits explicitly
        ]
        self._proc = subprocess.Popen(
            cmd, cwd=self._repo_dir, stdout=log_fh, stderr=subprocess.STDOUT
        )
        self._wait_for_port()

    def _wait_for_port(self, timeout: float = 30.0) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self._proc and self._proc.poll() is not None:
                raise RuntimeError(
                    f"dolt sql-server exited with code {self._proc.returncode}; check {self._repo_dir / 'logs' / 'dolt-sql-server.log'}"
                )
            if _port_open(self._config.dolt.host, self._config.dolt.port):
                # Try a real connection as a readiness probe.
                try:
                    with self._raw_connect() as c:
                        c.ping(reconnect=False)
                    return
                except Exception:
                    pass
            time.sleep(0.2)
        raise TimeoutError(
            f"dolt sql-server failed to come up on {self._config.dolt.host}:{self._config.dolt.port} within {timeout}s"
        )

    def _raw_connect(self) -> Connection:
        # The dolt repo dir name is the database name by default.
        return pymysql.connect(
            host=self._config.dolt.host,
            port=self._config.dolt.port,
            user=self._config.dolt.user,
            password="",
            database=self._database_name(),
            autocommit=True,
            charset="utf8mb4",
        )

    def _database_name(self) -> str:
        return self._repo_dir.name

    @contextmanager
    def connect(self) -> Iterator[Connection]:
        conn = self._raw_connect()
        try:
            yield conn
        finally:
            conn.close()

    def commit(self, message: str) -> str | None:
        """Stage and commit pending changes. Returns the commit hash, or None
        if there were no changes to commit."""
        with self.connect() as conn, conn.cursor() as cur:
            cur.execute("CALL DOLT_ADD('-A')")
            cur.execute("SELECT COUNT(*) FROM dolt_status")
            (changed,) = cur.fetchone()  # type: ignore[misc]
            if changed == 0:
                return None
            cur.execute("CALL DOLT_COMMIT('-m', %s)", (message,))
            row = cur.fetchone()
            return row[0] if row else None
