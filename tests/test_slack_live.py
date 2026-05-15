"""Live Slack sync integration test (HTTP-driven).

Boots the real `frankweiler_http_bin` against a fresh hermetic
`data_root` and a synthesized one-source config that points at a small
Slack channel. Drives the sync via the same HTTP API the Vue UI calls
(`POST /api/sync/jobs`), polls until terminal, then asserts the
downloader actually produced files and (when ingest also runs) that the
grid endpoint returns rows.

Non-hermetic by design: hits real Slack (and Dolt + Python worker on the
host). Tagged `manual` so it's excluded from `bazelisk test //...`. Run
explicitly:

    bazelisk test //tests:slack_live_test

Prerequisites:
  * `latchkey` on PATH with creds set for the `slack` service
  * `dolt` on PATH
  * `python3` on PATH (the Rust backend spawns `python3 -m worker`)
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

SLACK_CHANNEL = "thad-testing-channel"
SOURCE_NAME = "slack-live"
JOB_TIMEOUT_S = 300.0  # 5 min upper bound for download+ingest end-to-end
HEALTH_TIMEOUT_S = 30.0


# ---------------------------------------------------------------------------
# Runfiles helpers — locate the Rust binary and the Python `worker` package.
# ---------------------------------------------------------------------------


def _runfiles_root() -> Path:
    """Bazel sets RUNFILES_DIR when invoking a py_test; outside Bazel we
    fall back to the workspace so the test can also be run with raw
    pytest for quick iteration."""
    rd = os.environ.get("RUNFILES_DIR")
    if rd:
        return Path(rd) / "_main"
    return Path(__file__).resolve().parents[1]


def _backend_binary() -> Path:
    root = _runfiles_root()
    p = root / "frankweiler/backend/http/frankweiler_http_bin"
    if not p.exists():
        # Outside Bazel — the user is responsible for `bazel build` first.
        pytest.skip(f"frankweiler_http_bin not at {p}; run via bazel")
    return p


def _python_path_for_worker() -> str:
    """The Rust backend spawns `python3 -m worker` with the inherited env.
    Bare host python doesn't have `worker` installed, so we prepend the
    location where the bazel runfiles put it."""
    root = _runfiles_root()
    src = root / "src"
    if not (src / "worker" / "__init__.py").exists():
        # Dev fallback: workspace root has src/worker/
        ws = Path(__file__).resolve().parents[1] / "src"
        if (ws / "worker" / "__init__.py").exists():
            src = ws
    return str(src)


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


# ---------------------------------------------------------------------------
# HTTP helpers — keep them stdlib-only so the test has no extra deps.
# ---------------------------------------------------------------------------


def _http(
    method: str,
    url: str,
    body: dict | None = None,
    timeout: float = 10.0,
) -> tuple[int, bytes]:
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def _get_json(url: str, timeout: float = 10.0):
    code, body = _http("GET", url, timeout=timeout)
    assert code == 200, f"GET {url} → {code}: {body!r}"
    return json.loads(body)


def _post_json(url: str, body: dict, timeout: float = 10.0):
    code, raw = _http("POST", url, body=body, timeout=timeout)
    assert code in (200, 201), f"POST {url} → {code}: {raw!r}"
    return json.loads(raw) if raw else None


# ---------------------------------------------------------------------------
# The test itself.
# ---------------------------------------------------------------------------


def _stream_log_to_stderr(path: Path, stop: threading.Event, label: str) -> None:
    """Tail `path` and forward new bytes to test stderr so the user sees
    backend / worker log output interleaved with pytest output. Best-effort
    — file may not exist yet at startup."""
    last = 0
    while not stop.is_set():
        try:
            if path.exists():
                with open(path, "rb") as f:
                    f.seek(last)
                    chunk = f.read()
                    if chunk:
                        sys.stderr.write(f"[{label}] {chunk.decode(errors='replace')}")
                        sys.stderr.flush()
                        last = f.tell()
        except OSError:
            pass
        stop.wait(0.5)


@pytest.fixture
def data_root(tmp_path_factory) -> Path:
    p = tmp_path_factory.mktemp("slack-live-root")
    return p


def _write_config(data_root: Path, dolt_port: int) -> Path:
    cfg = data_root / "config.yaml"
    cfg.write_text(
        f"""\
data_root: {data_root}
dolt:
  port: {dolt_port}
sources:
  - name: {SOURCE_NAME}
    type: slack_api
    sync:
      channels: ["{SLACK_CHANNEL}"]
      refresh_window_days: 0
      media: true
"""
    )
    return cfg


def _ensure_tools_available() -> None:
    for tool in ("latchkey", "dolt", "python3"):
        if shutil.which(tool) is None:
            pytest.skip(f"{tool} not on PATH — required for live slack test")


def _wait_for_health(http_url: str) -> None:
    deadline = time.monotonic() + HEALTH_TIMEOUT_S
    last_err: Exception | None = None
    while time.monotonic() < deadline:
        try:
            code, _ = _http("GET", f"{http_url}/api/health", timeout=2.0)
            if code == 200:
                return
        except (OSError, urllib.error.URLError) as e:
            last_err = e
        time.sleep(0.25)
    raise AssertionError(f"backend never came up: {last_err}")


def _wait_for_job_terminal(http_url: str, job_id: str, label: str) -> dict:
    """Poll /api/sync/jobs/{id} until terminal state or timeout."""
    deadline = time.monotonic() + JOB_TIMEOUT_S
    last_state = None
    while time.monotonic() < deadline:
        try:
            job = _get_json(f"{http_url}/api/sync/jobs/{job_id}")
        except AssertionError as e:
            # 404 right after enqueue is possible if the worker hasn't
            # committed yet — keep trying briefly.
            sys.stderr.write(f"[poll] {label}: {e}\n")
            time.sleep(0.5)
            continue
        state = job["state"]
        if state != last_state:
            sys.stderr.write(
                f"[poll] {label} {job_id[:8]} → {state} ({job.get('progress_msg')})\n"
            )
            last_state = state
        if state in ("done", "failed", "canceled"):
            return job
        time.sleep(1.0)
    raise AssertionError(f"{label} job {job_id} never reached terminal state")


def test_slack_live_download_and_ingest(data_root: Path) -> None:
    _ensure_tools_available()
    bin_path = _backend_binary()
    pythonpath = _python_path_for_worker()

    dolt_port = _free_port()
    http_port = _free_port()
    http_url = f"http://127.0.0.1:{http_port}"

    cfg_path = _write_config(data_root, dolt_port)
    sys.stderr.write(f"[setup] data_root={data_root}\n")
    sys.stderr.write(f"[setup] config={cfg_path}\n")
    sys.stderr.write(f"[setup] dolt port={dolt_port} http port={http_port}\n")

    env = os.environ.copy()
    env["FRANKWEILER_CONFIG"] = str(cfg_path)
    env["FRANKWEILER_BIND"] = f"127.0.0.1:{http_port}"
    env["FRANKWEILER_ROOT"] = str(data_root)
    # Make `python3 -m worker` (spawned by the Rust backend) work — the
    # host python doesn't have `worker` installed.
    existing_pp = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = f"{pythonpath}:{existing_pp}" if existing_pp else pythonpath

    backend_log = data_root / "backend.stderr.log"
    backend = subprocess.Popen(
        [str(bin_path)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=open(backend_log, "wb"),
    )
    sys.stderr.write(f"[setup] backend pid={backend.pid}\n")

    stop = threading.Event()
    threads = [
        threading.Thread(
            target=_stream_log_to_stderr,
            args=(backend_log, stop, "backend"),
            daemon=True,
        ),
        threading.Thread(
            target=_stream_log_to_stderr,
            args=(data_root / "logs" / "worker.log", stop, "worker"),
            daemon=True,
        ),
    ]
    for t in threads:
        t.start()

    try:
        _wait_for_health(http_url)
        sys.stderr.write("[setup] backend healthy\n")

        # Sanity: /api/sync/sources should surface our single source.
        sources = _get_json(f"{http_url}/api/sync/sources")
        names = [s["name"] for s in sources]
        assert SOURCE_NAME in names, f"sources missing {SOURCE_NAME}: {sources}"

        # Drive a download job directly (same path the Vue 'Sync now'
        # button takes, modulo the `all` parent wrapper).
        dl = _post_json(
            f"{http_url}/api/sync/jobs",
            {"kind": "download", "source_name": SOURCE_NAME},
        )
        assert dl is not None
        sys.stderr.write(f"[enqueue] download job id={dl['id']}\n")
        dl_final = _wait_for_job_terminal(http_url, dl["id"], "download")
        assert dl_final["state"] == "done", (
            f"download failed: state={dl_final['state']!r} error={dl_final.get('error')!r}"
        )

        # Verify on-disk output.
        raw_dir = data_root / "raw" / SOURCE_NAME
        runs = sorted(p for p in raw_dir.iterdir() if p.is_dir())
        assert runs, f"no run subdir produced under {raw_dir}"
        run_dir = runs[-1]
        produced = list(run_dir.rglob("events.jsonl"))
        assert produced, (
            f"download claimed done but no events.jsonl under {run_dir}; "
            f"contents: {[p.name for p in run_dir.iterdir()]}"
        )
        sys.stderr.write(f"[verify] {len(produced)} events.jsonl files in {run_dir}\n")

        # Now ingest and verify the grid sees rows.
        ing = _post_json(f"{http_url}/api/sync/jobs", {"kind": "ingest"})
        assert ing is not None
        sys.stderr.write(f"[enqueue] ingest job id={ing['id']}\n")
        ing_final = _wait_for_job_terminal(http_url, ing["id"], "ingest")
        assert ing_final["state"] == "done", (
            f"ingest failed: state={ing_final['state']!r} "
            f"error={ing_final.get('error')!r}"
        )

        grid = _get_json(f"{http_url}/api/grid?limit=10")
        assert grid.get("rows"), f"grid returned no rows after ingest: {grid}"
        sys.stderr.write(f"[verify] grid returned {len(grid['rows'])} rows\n")

    finally:
        stop.set()
        backend.terminate()
        try:
            backend.wait(timeout=10)
        except subprocess.TimeoutExpired:
            backend.kill()
            backend.wait()
        for t in threads:
            t.join(timeout=2.0)
        # On failure leak the data_root so the user can inspect; pytest's
        # tmp_path_factory keeps the last few runs anyway.
        sys.stderr.write(f"[teardown] data_root preserved at {data_root}\n")
