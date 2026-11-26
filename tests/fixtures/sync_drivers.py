"""Two ways to run the TNG fixture's syncs: `datalib-pipeline` from a
terminal, or `datalib-http` driven over the API the way the web client
drives it. Both take the same four calls, so `run_sync_pipeline.py`
reads the same whichever door it goes through.

The HTTP driver builds the root the way a person does in the app — the
starter config from `POST /api/config/init`, then one `PUT /api/config`
per source added — and syncs with `POST /api/requests`, waiting on
`GET /api/requests/{id}` for each request it opened to close.

Either driver stops the moment the loop holds a step for disk space: a
held step starts again only once space is freed, and nothing in a build
frees any, so waiting would only run out the deadline.
"""

from __future__ import annotations

import json
import os
import signal
import sqlite3
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterable
from pathlib import Path
from typing import Any, NamedTuple

# A whole fixture sync is a minute or two on a warm laptop; this is for
# a loaded CI runner. One deadline per `sync()`, however many requests it
# opens; past it, the error names each step still unfinished.
SYNC_DEADLINE_SECS = 30 * 60
STARTUP_DEADLINE_SECS = 60
POLL_SECS = 0.2

# The program and the locks a datalib config declares, above every group:
# the runner gives `datalib-step` no verbs and no locks of its own.
PROGRAM_AND_LOCKS = """[[programs]]
command = "datalib-step"
verbs = ["reset", "prepare", "compact", "collect", "check"]

[[locks]]
name = "index"
slots = 2

[[locks]]
name = "embedder"
slots = 1"""

# The `WaitKind` of a step the loop holds for the config's `[disk_space]`
# floor (`datalib/backend/dag/src/supervisor/tick.rs`).
DISK_SPACE = "disk_space"


class StepNow(NamedTuple):
    """One step as the loop's record has it now."""

    step: str
    state: str | None
    waiting_for: str | None
    detail: str | None

    def describe(self) -> str:
        return f"{self.step}: {self.state}" + (
            f" ({self.detail})" if self.detail else ""
        )


def held_for_disk(steps: Iterable[StepNow]) -> StepNow | None:
    return next((s for s in steps if s.waiting_for == DISK_SPACE), None)


def disk_hold_error(held: StepNow, root: Path) -> SystemExit:
    return SystemExit(
        "the sync cannot finish: the loop holds its steps for want of disk space\n"
        f"  data root: {root}\n"
        f"  {held.step}: {held.detail}\n"
        "Free space on that disk, or lower the config's [disk_space] floor "
        "(`pause_below_bytes`)."
    )


class CliDriver:
    """`datalib-pipeline <verb> <config> … --binary-dir … --now …`, one
    invocation per call. The config lives beside the root as `dag.toml`."""

    def __init__(
        self, dag_bin: Path, workspace: Path, bindir: Path, now: str, env: dict
    ):
        self.workspace = workspace
        self.config = workspace / "dag.toml"
        self.dag_bin = str(dag_bin)
        self.flags = ["--binary-dir", str(bindir), "--now", now]
        self.env = env

    def build_config(self, texts: list[str]) -> None:
        self.config.write_text(
            f"data_root = {json.dumps(str(self.workspace))}\n\n" + texts[-1]
        )

    def _argv(self, verb: str, steps: list[str]) -> list[str]:
        return [self.dag_bin, verb, str(self.config), *steps, *self.flags]

    def sync(
        self, roots: list[str] | None = None, deadline_secs: float = SYNC_DEADLINE_SECS
    ) -> None:
        """Runs `datalib-pipeline sync` to its end, reading the loop's record
        in `system/supervisor.sqlite` while it runs: a one-shot run keeps
        waiting while the disk holds its steps, and says so only in its
        log."""
        argv = self._argv("sync", roots or [])
        print("[sync_drivers] $", " ".join(argv), flush=True)
        proc = subprocess.Popen(argv, env=self.env)
        deadline = time.monotonic() + deadline_secs
        try:
            while (code := proc.poll()) is None:
                steps = self._recorded_steps()
                if held := held_for_disk(steps):
                    raise disk_hold_error(held, self.workspace)
                if time.monotonic() > deadline:
                    raise SystemExit(
                        f"datalib-pipeline still running after {deadline_secs:.0f}s; "
                        "steps not finished, as its record has them:\n"
                        + _unfinished(steps)
                    )
                time.sleep(POLL_SECS)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        if code != 0:
            raise subprocess.CalledProcessError(code, argv)

    def reset(self, targets: list[str]) -> None:
        _run(self._argv("reset", targets), self.env)

    def close(self) -> None:
        pass

    def _recorded_steps(self) -> list[StepNow]:
        """Every step's state in the loop's record; none before the loop
        has made the record, or while a write holds it."""
        store = self.workspace / "system" / "supervisor.sqlite"
        if not store.is_file():
            return []
        try:
            con = sqlite3.connect(f"file:{store}?mode=ro", uri=True, timeout=1)
            try:
                rows = con.execute(
                    "SELECT step, state, waiting_for, state_detail FROM steps "
                    "ORDER BY step"
                ).fetchall()
            finally:
                con.close()
        except sqlite3.OperationalError as e:
            # The store is made, then its tables; a write holds it briefly.
            # Anything else (a missing column) would turn the check off.
            if "locked" in str(e) or "no such table" in str(e):
                return []
            raise
        return [StepNow(*row) for row in rows]


class HttpDriver:
    """A `datalib-http` on the root for the life of the driver."""

    def __init__(
        self,
        http_bin: Path,
        pipeline_bin: Path,
        workspace: Path,
        bindir: Path,
        now: str,
        env: dict,
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
                # The runner the server keeps as a child.
                "DATALIB_PIPELINE_BIN": str(pipeline_bin),
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

    def sync(
        self, roots: list[str] | None = None, deadline_secs: float = SYNC_DEADLINE_SECS
    ) -> None:
        """One request per source the roots belong to; waits for each."""
        opened = self.call("POST", "/api/requests", {"roots": roots or []})
        for request in opened:
            print(
                f"[sync_drivers] request {request['id']} → {request['roots']}",
                flush=True,
            )
        deadline = time.monotonic() + deadline_secs
        for request in opened:
            closed = self.wait_closed(
                request["id"], max(0.0, deadline - time.monotonic())
            )
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
        """The request once closed. Fails sooner when the loop holds one of
        its steps for disk space, and past the deadline names the steps
        it was still waiting on."""
        deadline = time.monotonic() + deadline_secs
        while True:
            self._check_alive()
            status, row = self.request("GET", f"/api/requests/{request_id}")
            if status == 404:
                raise SystemExit(f"request {request_id} is not in GET /api/requests")
            if status >= 400:
                raise SystemExit(f"GET /api/requests/{request_id} → {status}: {row}")
            if row["state"] != "open":
                return row
            steps = [
                StepNow(s["id"], s["state"], s["waiting_for"], s["state_detail"])
                for s in row["steps"]
            ]
            if held := held_for_disk(steps):
                raise disk_hold_error(held, self.workspace)
            if time.monotonic() > deadline:
                unfinished = [
                    st
                    for st, s in zip(steps, row["steps"], strict=True)
                    if s["phase"] != "done"
                ]
                raise SystemExit(
                    f"request {request_id} still open after {deadline_secs:.0f}s; "
                    f"steps it was still waiting on:\n{_unfinished(unfinished)}\n"
                    f"server log: {self.log_path}"
                )
            time.sleep(POLL_SECS)

    def failure_log(self, step: str | None) -> str:
        """The failed step's newest log lines, from the run store's log."""
        if not step:
            return ""
        query = urllib.parse.urlencode({"q": f"step:{step}", "limit": 40})
        status, lines = self.request("GET", f"/api/log?{query}")
        if status >= 400:
            return f"  (GET /api/log?{query} → {status}: {lines})"
        return "\n".join(f"  {line['level']}: {line['msg']}" for line in lines)

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


def _unfinished(steps: Iterable[StepNow]) -> str:
    lines = [
        f"  {s.describe()}"
        for s in steps
        if s.state not in ("idle", "fresh", "stale", "off")
    ]
    return "\n".join(lines) or "  (none: every step reads finished)"


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
