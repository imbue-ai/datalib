"""The sync state machine under a storm: a person pounding the buttons
while processes die, then one plain sync, compared with a sync from
nothing.

Each case prepares the TNG workspace (`run_sync_pipeline.prepare`),
builds its config through `datalib-http` the way the wizard does, and
plays a seeded schedule of events against the server: the Manage
screen's verbs, and SIGKILLs of the steps it runs. Then it resumes every
pause, syncs everything to completion and snapshots every store; resets
every source, syncs again and snapshots again. The two must agree. A
step that resumed by skipping a page, fetched one twice, or trusted a
half-written store lands different rows, and the diff names them.

The seed is printed first. `TNG_FUZZ_SEED=<n>` replays one case;
`TNG_FUZZ_EVENTS` sets how many events it plays. Plan:
`docs/dev/plans/http_driven_e2e.md` § 2.

A red case is a lead, not a test: build the state it names by hand in
the owning crate's tests, fast and deterministic, before fixing it.

Args: the doltlite shell, then `run_sync_pipeline.py`'s own arguments
(its data root is replaced per case).
"""

from __future__ import annotations

import hashlib
import json
import os
import random
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from run_sync_pipeline import prepare
from sync_drivers import HttpDriver

# CI's schedules. A new one comes from changing this list, on purpose;
# `TNG_FUZZ_SEED` explores.
SEEDS = [1, 2, 3]
EVENTS = int(os.environ.get("TNG_FUZZ_EVENTS", "40"))
# How long a person waits between clicks. The gap is the fuzz input, not
# a wait for anything: nothing here is ordered, so no sleep orders it.
GAPS = [0.0, 0.0, 0.05, 0.2, 0.5, 1.0]
SETTLE_DEADLINE_SECS = 5 * 60
# A paused step or a stopped request can leave nothing to start; then
# the kill is skipped rather than waited for.
VICTIM_DEADLINE_SECS = 3
# After a step is found: at once, mid-write, or near its end.
KILL_DELAYS = [0.0, 0.02, 0.1, 0.3]

# Tables that record the runs rather than the data: a store that has
# been through a storm has had more of them than one synced once.
# `source_cursors` is the index's note of its last pass per source, with
# that pass's document count.
RUN_HISTORY_TABLES = {"sync_runs", "_datalib_meta", "sqlite_sequence", "source_cursors"}
# What datalib measures of its own stores on disk — the storage page, its
# grid rows and the measurements behind them. A reset keeps a store's
# history, so the file is bigger after one than before.
SELF_MEASUREMENT_TABLES = {"source_measurements"}
SELF_MEASUREMENT_PROVIDER = "datalib"
SELF_MEASUREMENT_DOCS = "/_datalib/"
# Columns that hold churn by construction.
VOLATILE_COLUMNS = {"volatile_payload"}
# A `*_bookkeeping` row counts the attempts at a record across runs.
RUN_COUNT_COLUMNS = {"attempt_count"}
_STAMP = re.compile(r"\d{4}-\d\d-\d\d[T ]\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:?\d\d)?")
_HASH = re.compile(r"\b[0-9a-f]{40,64}\b")
# A version-7 uuid is minted from the clock: a run's or a request's id.
# The entity ids datalib mints are version 8, and stay.
_UUID7 = re.compile(
    r"\b[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}\b"
)


def mask(value: object) -> object:
    if isinstance(value, str):
        value = _STAMP.sub("<stamp>", value)
        value = _HASH.sub("<hash>", value)
        return _UUID7.sub("<uuid7>", value)
    return value


@dataclass
class Case:
    seed: int
    driver: HttpDriver
    ingest_ids: list[str]
    render_ids: list[str]
    config_text: str
    rng: random.Random
    log: list[str] = field(default_factory=list)
    paused: set[str] = field(default_factory=set)

    def note(self, line: str) -> None:
        self.log.append(line)
        print(f"[fuzz {self.seed}] {line}", flush=True)


# ── the events ──────────────────────────────────────────────────────
# Each returns the status it saw and the statuses it accepts.


def sync_everything(c: Case):
    return c.driver.request("POST", "/api/requests", {"roots": []})[0], {200}


def sync_a_few(c: Case):
    roots = c.rng.sample(c.ingest_ids, c.rng.randint(1, 3))
    return c.driver.request("POST", "/api/requests", {"roots": roots})[0], {200}


def stop_a_request(c: Case):
    open_ids = [r["id"] for r in c.driver.requests() if r["state"] == "open"]
    if not open_ids:
        return None, set()
    rid = c.rng.choice(open_ids)
    return c.driver.request("POST", f"/api/requests/{rid}/stop")[0], {204}


def pause_a_step(c: Case):
    step = c.rng.choice(c.ingest_ids + c.render_ids)
    c.paused.add(step)
    return c.driver.request("POST", f"/api/steps/{q(step)}/pause")[0], {204}


def resume_a_step(c: Case):
    if not c.paused:
        return None, set()
    step = c.rng.choice(sorted(c.paused))
    c.paused.discard(step)
    return c.driver.request("POST", f"/api/steps/{q(step)}/resume")[0], {204}


def reset_some(c: Case):
    picked = c.rng.sample(c.ingest_ids, c.rng.randint(1, 2))
    targets = [f"{s}+blobs" for s in picked]
    # Refused while a sync runs, by design.
    return c.driver.request("POST", "/api/reset", {"targets": targets})[0], {204, 409}


def save_config_unchanged(c: Case):
    status, answer = c.driver.request("PUT", "/api/config", {"text": c.config_text})
    return (status if answer and answer.get("ok") else -1), {200}


def victim(c: Case) -> int | None:
    """A running step, starting a sync to have one if none is running,
    killed a seeded moment after it is found. The fixture syncs in
    seconds, so a moment picked blind mostly finds nothing to kill."""
    victims = children(c.driver.proc.pid)
    if not victims:
        roots = c.rng.sample(c.ingest_ids, c.rng.randint(1, 3))
        c.driver.request("POST", "/api/requests", {"roots": roots})
        deadline = time.monotonic() + VICTIM_DEADLINE_SECS
        while not victims and time.monotonic() < deadline:
            time.sleep(0.01)
            victims = children(c.driver.proc.pid)
    if not victims:
        return None
    time.sleep(c.rng.choice(KILL_DELAYS))
    return c.rng.choice(victims)


def kill_a_step(c: Case):
    pid = victim(c)
    if pid is None:
        return None, set()
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        return None, set()
    return 0, {0}


def kill_a_step_group(c: Case):
    """The step and everything it started. Never a group the server is
    in: that would take the server, and this test, with it."""
    pid = victim(c)
    if pid is None:
        return None, set()
    try:
        group = os.getpgid(pid)
        if group == os.getpgid(c.driver.proc.pid):
            return None, set()
        os.killpg(group, signal.SIGKILL)
    except ProcessLookupError:
        return None, set()
    return 0, {0}


EVENTS_BY_WEIGHT = [
    (sync_everything, 3),
    (sync_a_few, 4),
    (stop_a_request, 3),
    (pause_a_step, 2),
    (resume_a_step, 2),
    (reset_some, 1),
    (save_config_unchanged, 1),
    (kill_a_step, 2),
    (kill_a_step_group, 1),
]

# `closed` is an outcome a newer build wrote; never this one.
REQUEST_STATES = {"open", "done", "failed", "stopped"}


def q(step_id: str) -> str:
    """A step id as one path segment: its slash encoded, as the UI does."""
    return urllib.parse.quote(step_id, safe="")


def children(pid: int) -> list[int]:
    # pgrep exits 1 when there is no child, which is an answer.
    out = subprocess.run(
        ["pgrep", "-P", str(pid)], capture_output=True, text=True, check=False
    )
    return [int(p) for p in out.stdout.split()]


# ── the oracle ──────────────────────────────────────────────────────


def snapshot(doltlite: str, workspace: Path) -> dict[str, object]:
    """Every store's content, masked, plus every document's hash."""
    shot: dict[str, object] = {}
    for db in sorted(workspace.glob("*/*/*.doltlite_db")):
        rel = str(db.relative_to(workspace))
        tables = query(
            doltlite, db, "SELECT name FROM sqlite_master WHERE type='table'"
        )
        for t in sorted(r["name"] for r in tables):
            if t in RUN_HISTORY_TABLES | SELF_MEASUREMENT_TABLES or t.startswith(
                "sqlite_"
            ):
                continue
            dropped = VOLATILE_COLUMNS | (
                RUN_COUNT_COLUMNS if t.endswith("_bookkeeping") else set()
            )
            rows = [
                {k: mask(v) for k, v in row.items() if k not in dropped}
                for row in query(doltlite, db, f'SELECT * FROM "{t}"')
                if row.get("provider") != SELF_MEASUREMENT_PROVIDER
            ]
            shot[f"{rel}:{t}"] = sorted(json.dumps(r, sort_keys=True) for r in rows)
    for md in sorted(workspace.glob("*/render_markdown/**/*.md")):
        if SELF_MEASUREMENT_DOCS in str(md):
            continue
        shot[str(md.relative_to(workspace))] = hashlib.sha256(
            mask(md.read_text()).encode()  # type: ignore[union-attr]
        ).hexdigest()
    return shot


def query(doltlite: str, db: Path, sql: str) -> list[dict]:
    out = subprocess.run(
        [doltlite, "-readonly", "-json", str(db), sql],
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(out.stdout) if out.stdout.strip() else []


def differences(before: dict, after: dict, limit: int = 60) -> list[str]:
    """Per table, the rows only one side has, paired by `id` where the
    table has one so a changed row reads as its changed columns."""
    lines: list[str] = []
    for key in sorted(set(before) | set(after)):
        a, b = before.get(key), after.get(key)
        if a == b:
            continue
        if not (isinstance(a, list) and isinstance(b, list)):
            lines.append(
                f"{key}: only {'after' if a is None else 'before'}"
                if a is None or b is None
                else f"{key}: content differs"
            )
            continue
        lines.append(f"{key}: {len(a)} rows vs {len(b)}")
        only_a = [json.loads(r) for r in sorted(set(a) - set(b))]
        only_b = [json.loads(r) for r in sorted(set(b) - set(a))]
        by_id_b = {r.get("id"): r for r in only_b if "id" in r}
        for ra in only_a:
            rb = by_id_b.pop(ra.get("id"), None) if "id" in ra else None
            if rb is None:
                lines.append(f"  - {json.dumps(ra)[:300]}")
            else:
                changed = {
                    k: (ra.get(k), rb.get(k))
                    for k in sorted(set(ra) | set(rb))
                    if ra.get(k) != rb.get(k)
                }
                lines.append(f"  ~ id={ra['id']}: {json.dumps(changed)[:300]}")
        for rb in only_b:
            if "id" not in rb or rb["id"] in by_id_b:
                lines.append(f"  + {json.dumps(rb)[:300]}")
        if len(lines) >= limit:
            lines.append("  …")
            break
    return lines


class FuzzTest(unittest.TestCase):
    doltlite: str
    pipeline_args: list[str]

    def run_case(self, seed: int, events: int) -> None:
        with tempfile.TemporaryDirectory(prefix=f"tng-fuzz-{seed}-") as tmp:
            argv = ["run_sync_pipeline.py", *self.pipeline_args]
            argv[6] = str(Path(tmp) / "ws")
            fx = prepare(argv)
            assert fx.http_bin, "the fuzzer drives a server: pass datalib-http"
            driver = HttpDriver(fx.http_bin, fx.workspace, fx.bindir, fx.now, fx.env)
            try:
                config_text = fx.config_texts({})[0]
                driver.build_config([config_text])
                case = Case(
                    seed=seed,
                    driver=driver,
                    ingest_ids=fx.ingest_ids,
                    render_ids=[f"{s}/render_markdown" for s in fx.sources],
                    config_text=config_text,
                    rng=random.Random(seed),
                )
                self.storm(case, events)
                self.settle(case)
                fuzzed = snapshot(self.doltlite, fx.workspace)

                targets = [f"{s}+blobs" for s in fx.ingest_ids] + case.render_ids
                case.note(f"reset {len(targets)} steps, then sync from nothing")
                driver.reset(targets)
                driver.sync()
                # A reset opens a sync of its own, of what reads the steps
                # it emptied.
                wait_all_closed(driver)
                clean = snapshot(self.doltlite, fx.workspace)
            finally:
                driver.close()
            diff = differences(fuzzed, clean)
            self.assertEqual(
                diff,
                [],
                f"seed {seed}: after the storm and a full sync, the stores differ from a sync "
                f"from nothing\nevents:\n  "
                + "\n  ".join(case.log)
                + "\ndifferences (fuzzed vs clean):\n"
                + "\n".join(diff),
            )

    def storm(self, c: Case, events: int) -> None:
        kinds = [e for e, _ in EVENTS_BY_WEIGHT]
        weights = [w for _, w in EVENTS_BY_WEIGHT]
        for i in range(events):
            event = c.rng.choices(kinds, weights)[0]
            status, accepted = event(c)
            c.note(f"{i:3} {event.__name__}: {status}")
            if status is not None:
                self.assertIn(
                    status,
                    accepted,
                    f"seed {c.seed}: {event.__name__} → {status}\n" + "\n".join(c.log),
                )
            self.assertIsNone(c.driver.proc.poll(), f"seed {c.seed}: the server died")
            states = {r["id"]: r["state"] for r in c.driver.requests()}
            odd = {rid: st for rid, st in states.items() if st not in REQUEST_STATES}
            self.assertEqual(odd, {}, f"seed {c.seed}: requests in no known state")
            time.sleep(c.rng.choice(GAPS))

    def settle(self, c: Case) -> None:
        """Lift every pause, let what is open finish, and sync everything."""
        for step in c.ingest_ids + c.render_ids:
            c.driver.call("POST", f"/api/steps/{q(step)}/resume")
        wait_all_closed(c.driver)
        c.note("settled; sync everything")
        c.driver.sync()
        wait_all_closed(c.driver)


def wait_all_closed(driver: HttpDriver) -> None:
    for r in driver.requests():
        if r["state"] == "open":
            driver.wait_closed(r["id"], SETTLE_DEADLINE_SECS)


def _test_for(seed: int, events: int):
    def test(self: FuzzTest) -> None:
        print(f"[fuzz] seed {seed}, {events} events", flush=True)
        self.run_case(seed, events)

    return test


def _cases() -> list[tuple[int, int]]:
    if "TNG_FUZZ_SEED" in os.environ:
        seed = int(os.environ["TNG_FUZZ_SEED"])
        return [(seed, EVENTS if seed else 0)]
    # Seed 0 plays nothing: the oracle alone, a sync against a reset and a
    # sync, so a red here is the comparison and not the storm.
    return [(0, 0)] + [(s, EVENTS) for s in SEEDS]


def _this_shard(cases: list[tuple[int, int]]) -> list[tuple[int, int]]:
    """Bazel runs one shard per case; say so, or it fails the target."""
    if status := os.environ.get("TEST_SHARD_STATUS_FILE"):
        Path(status).touch()
    index = int(os.environ.get("TEST_SHARD_INDEX", "0"))
    total = int(os.environ.get("TEST_TOTAL_SHARDS", "1"))
    return cases[index::total]


for _seed, _events in _this_shard(_cases()):
    setattr(FuzzTest, f"test_seed_{_seed}", _test_for(_seed, _events))


if __name__ == "__main__":
    FuzzTest.doltlite = str(Path(sys.argv[1]).resolve())
    FuzzTest.pipeline_args = sys.argv[2:]
    unittest.main(argv=sys.argv[:1])
