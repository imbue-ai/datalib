"""Two ways to run the TNG fixture's syncs: `datalib-dag` from a
terminal, or `datalib-http` driven over the API the way the web client
drives it. Both take the same four calls, so `run_sync_pipeline.py`
reads the same whichever door it goes through.

The HTTP driver builds the root the way a person does in the app — the
starter config from `POST /api/config/init`, then one `PUT /api/config`
per source added — and syncs with `POST /api/requests`, waiting on
`GET /api/requests` for the request to close.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

# A whole fixture sync is a minute or two on a warm laptop; this is for
# a loaded CI runner, and a hang still fails with the request it was on.
SYNC_DEADLINE_SECS = 30 * 60
STARTUP_DEADLINE_SECS = 60
POLL_SECS = 0.2


class CliDriver:
    """`datalib-dag <config> --binary-dir … --now …`, one invocation per
    call. The config lives beside the root as `dag.toml`."""

    def __init__(
        self, dag_bin: Path, workspace: Path, bindir: Path, now: str, env: dict
    ):
        self.workspace = workspace
        self.config = workspace / "dag.toml"
        self.argv = [
            str(dag_bin),
            str(self.config),
            "--binary-dir",
            str(bindir),
            "--now",
            now,
        ]
        self.env = env

    def build_config(self, texts: list[str]) -> None:
        self.config.write_text(
            f"data_root = {json.dumps(str(self.workspace))}\n\n" + texts[-1]
        )

    def sync(self, roots: list[str] | None = None) -> None:
        extra = ["--sync", ",".join(roots)] if roots else []
        _run([*self.argv, *extra], self.env)

    def reset(self, targets: list[str]) -> None:
        _run([*self.argv, "--reset", ",".join(targets)], self.env)

    def close(self) -> None:
        pass


class HttpDriver:
    """A `datalib-http` on the root for the life of the driver."""

    def __init__(
        self, http_bin: Path, workspace: Path, bindir: Path, now: str, env: dict
    ):
        self.workspace = workspace
        url_file = workspace.parent / f"{workspace.name}.url"
        url_file.unlink(missing_ok=True)
        self.log_path = workspace.parent / f"{workspace.name}.server.log"
        # Appended to: a server started again on the same root keeps
        # the dead one's lines.
        self._log = self.log_path.open("a")
        argv = [
            str(http_bin),
            "--no-open",
            "--url-file",
            str(url_file),
            "--now",
            now,
            str(workspace),
        ]
        print("[sync_drivers] $", " ".join(argv), flush=True)
        self.proc = subprocess.Popen(
            argv,
            env={
                **env,
                "DATALIB_BIND": "127.0.0.1:0",
                # Where the loop's steps find `datalib-step`.
                "DATALIB_BINARY_DIR": str(bindir),
            },
            stdin=subprocess.DEVNULL,
            stdout=self._log,
            stderr=subprocess.STDOUT,
        )
        url = self._wait_for_file(url_file, "its url file")
        self.origin = url.split("?")[0].rsplit("/", 1)[0]
        # The url file lands before the token file; the token is what
        # every call needs.
        self.token = self._wait_for_file(
            workspace / "system" / "api-token", "its api token"
        ).strip()
        self._wait_until_serving()

    # ── the four calls ──────────────────────────────────────────────

    def build_config(self, texts: list[str]) -> None:
        """The starter config, then each text in turn, as a person adding
        sources one at a time saves after each."""
        init = self.call("POST", "/api/config/init")
        if not init["created"]:
            # A workspace shared across runs already has its config; the
            # starter is only ever written once.
            print(f"[sync_drivers] config already there: {init['path']}", flush=True)
        for text in texts:
            verdict = self.call("PUT", "/api/config", {"text": text})
            if not verdict["ok"]:
                raise SystemExit(
                    f"PUT /api/config refused the config:\n{json.dumps(verdict['diagnostics'], indent=2)}\n---\n{text}"
                )

    def sync(self, roots: list[str] | None = None) -> None:
        request = self.call("POST", "/api/requests", {"roots": roots or []})
        print(
            f"[sync_drivers] request {request['id']} → {request['roots']}", flush=True
        )
        closed = self.wait_closed(request["id"])
        if closed["state"] != "done":
            raise SystemExit(
                f"request {request['id']} ended {closed['state']}"
                f" (failed step: {closed['failed_step']}); server log: {self.log_path}\n"
                + self.failure_log(closed["failed_step"])
                + "\n--- server log, last lines ---\n"
                + "\n".join(self.log_path.read_text().splitlines()[-60:])
            )

    def reset(self, targets: list[str]) -> None:
        self.call("POST", "/api/reset", {"targets": targets})

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self._log.close()

    # ── observation ─────────────────────────────────────────────────

    def requests(self) -> list[dict]:
        return self.call("GET", "/api/requests")

    def wait_closed(
        self, request_id: str, deadline_secs: float = SYNC_DEADLINE_SECS
    ) -> dict:
        deadline = time.monotonic() + deadline_secs
        while True:
            self._check_alive()
            row = next((r for r in self.requests() if r["id"] == request_id), None)
            if row is None:
                raise SystemExit(f"request {request_id} is not in GET /api/requests")
            if row["state"] != "open":
                return row
            if time.monotonic() > deadline:
                raise SystemExit(
                    f"request {request_id} still open after {deadline_secs}s; server log: {self.log_path}"
                )
            time.sleep(POLL_SECS)

    def failure_log(self, step: str | None) -> str:
        """The failed step's warnings and errors, from the run store's log."""
        if not step:
            return ""
        lines = self.call("GET", f"/api/log?step={urllib.parse.quote(step)}")
        return "\n".join(
            f"  {line.get('level')}: {line.get('msg') or line.get('line')}"
            for line in lines[-40:]
        )

    def call(self, method: str, path: str, body: object | None = None) -> Any:
        status, answer = self.request(method, path, body)
        if status >= 400:
            raise SystemExit(f"{method} {path} → {status}: {answer}")
        return answer

    def request(
        self, method: str, path: str, body: object | None = None
    ) -> tuple[int, Any]:
        """The status and the parsed answer, whatever the status."""
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.origin + path, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                status, text = resp.status, resp.read().decode()
        except urllib.error.HTTPError as e:
            status, text = e.code, e.read().decode(errors="replace")
        try:
            return status, json.loads(text) if text else None
        except json.JSONDecodeError:
            return status, text

    # ── startup ─────────────────────────────────────────────────────

    def _check_alive(self) -> None:
        code = self.proc.poll()
        if code is not None:
            raise SystemExit(
                f"datalib-http exited {code}; log: {self.log_path}\n{self.log_path.read_text()[-4000:]}"
            )

    def _wait_for_file(self, path: Path, what: str) -> str:
        deadline = time.monotonic() + STARTUP_DEADLINE_SECS
        while True:
            self._check_alive()
            if path.is_file() and (text := path.read_text()):
                return text
            if time.monotonic() > deadline:
                raise SystemExit(
                    f"datalib-http never wrote {what}; log: {self.log_path}"
                )
            time.sleep(0.05)

    def _wait_until_serving(self) -> None:
        # The url file is written before the router serves; a health
        # answer says it does.
        deadline = time.monotonic() + STARTUP_DEADLINE_SECS
        while True:
            self._check_alive()
            try:
                self.call("GET", "/api/health")
                return
            except (SystemExit, OSError):
                if time.monotonic() > deadline:
                    raise
            time.sleep(0.05)


def _run(argv: list[str], env: dict) -> None:
    print("[sync_drivers] $", " ".join(argv), flush=True)
    subprocess.run(argv, check=True, env=env)


def point_playback(link: Path, tree: Path) -> None:
    """Aim the playback path every step reads at `tree`. A running server
    fixed its steps' environment when it started, so a new capture
    arrives the way a changed upstream does: behind the same address."""
    tmp = link.with_name(link.name + ".tmp")
    tmp.unlink(missing_ok=True)
    tmp.symlink_to(tree)
    os.replace(tmp, link)
