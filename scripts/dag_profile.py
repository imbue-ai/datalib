#!/usr/bin/env python3
"""Profile a `datalib-dag` NDJSON event stream: per-step durations + stalls.

The runner writes one JSON object per line to stderr, each stamped with a
wall-clock `ts` (see `datalib/backend/dag/src/events.rs`). Capture it and
feed it here:

    datalib-dag pipeline.yaml 2> run.ndjson
    scripts/dag_profile.py run.ndjson

Two questions, both of which are painful to answer by reading the raw stream:

1. **Where did the time go?** Per-step wall clock from `step_start` to
   `step_finish`, sorted slowest first, with each step's share of the run.

2. **Did anything stall?** The longest silences — spans where a step emitted
   nothing. A step doing work emits progress; a step sitting at zero CPU on a
   hung request emits nothing, and looks identical to a fast step from the
   outside. This is what makes the difference visible.

Motivating case: a manual-e2e golden run took 1069s and the obvious suspect
(the source with by far the most output files) turned out to be one of the
fastest parts. ~840s of it was a single provider step stalling on a degraded
upstream, emitting nothing the whole time. Nothing in the stream said so —
there were no timestamps at all — so answering it meant `ps` and `du`
forensics against a live process. Hence this script, and the `ts` field.

See issue #136 for where this is going. The interesting idea there: rank log
lines by the silence that FOLLOWED them and use that as a bounded excerpt for
a failure report, with a few lines of preceding context — complementary to
"last N lines", which tells you how a run ended rather than where it wedged.
It is self-limiting by construction (gaps >= X number at most T/X), and the
budget scales with elapsed time rather than log volume, so a chatty healthy
step and a silent stuck one get equal treatment. A healthy run emits nothing.

Known blind spot, also tracked there: a stall that logs. A retry loop
printing "attempt 3/50" every 20s shows no gap while making no progress, so
this wants pairing with progress-flatline detection.
"""

from __future__ import annotations

import itertools
import json
import sys
from collections import defaultdict
from datetime import datetime

# A silence longer than this is worth showing. Chosen to be well above
# normal inter-event spacing (progress events fire many times a second)
# without burying a genuine stall in noise.
DEFAULT_STALL_SECONDS = 10.0
# How many rows each section prints.
TOP_N = 20


def parse_ts(s: str) -> datetime | None:
    try:
        return datetime.fromisoformat(s)
    except (ValueError, TypeError):
        return None


def load(path: str) -> list[tuple[datetime, dict]]:
    """Every stamped line, in file order. Unparseable lines are skipped.

    The stream is interleaved with whatever a step wrote to stderr, so
    non-JSON lines are expected and not an error.
    """
    out: list[tuple[datetime, dict]] = []
    with open(path, errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            ts = parse_ts(obj.get("ts", ""))
            if ts is not None and "event" in obj:
                out.append((ts, obj))
    return out


def fmt(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:6.1f}s"
    return f"{seconds / 60:6.1f}m"


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    path = argv[1]
    stall_threshold = float(argv[2]) if len(argv) > 2 else DEFAULT_STALL_SECONDS

    events = load(path)
    if not events:
        print(
            f"no stamped events in {path}.\n"
            "Older runs predate the `ts` field — re-run to get a profileable "
            "stream.",
            file=sys.stderr,
        )
        return 1

    run_start, run_end = events[0][0], events[-1][0]
    total = (run_end - run_start).total_seconds()
    print(f"run: {run_start.isoformat()} → {run_end.isoformat()}  ({fmt(total)})")
    print(f"events: {len(events)}\n")

    # ── per-step durations ────────────────────────────────────────────
    started: dict[str, datetime] = {}
    spans: list[tuple[float, str, str]] = []
    for ts, e in events:
        step = e.get("step")
        if not step:
            continue
        if e["event"] == "step_start":
            started[step] = ts
        elif e["event"] == "step_finish":
            t0 = started.pop(step, None)
            if t0 is not None:
                spans.append(((ts - t0).total_seconds(), step, e.get("status", "?")))
    for step, t0 in started.items():
        # Still running when the stream ended — a crash or a kill.
        spans.append(((run_end - t0).total_seconds(), step, "UNFINISHED"))

    if spans:
        print(f"── slowest steps ({min(len(spans), TOP_N)} of {len(spans)}) ──")
        for dur, step, status in sorted(spans, reverse=True)[:TOP_N]:
            share = f"{100 * dur / total:4.1f}%" if total > 0 else "   - "
            print(f"  {fmt(dur)}  {share}  {step:<34} {status}")
        print()

    # ── silences ──────────────────────────────────────────────────────
    # Gap between consecutive events anywhere in the stream. Attributed to
    # the step that was last heard from, which is the one to suspect.
    gaps: list[tuple[float, datetime, str, str]] = []
    for (t0, e0), (t1, _e1) in itertools.pairwise(events):
        gap = (t1 - t0).total_seconds()
        if gap >= stall_threshold:
            gaps.append((gap, t0, e0.get("step") or "-", e0["event"]))

    print(f"── silences ≥ {stall_threshold:g}s ({len(gaps)}) ──")
    if not gaps:
        print("  none — the stream never went quiet for that long.")
    else:
        for gap, at, step, after in sorted(gaps, reverse=True)[:TOP_N]:
            print(
                f"  {fmt(gap)}  after {at.strftime('%H:%M:%S')}  {step:<34} (last: {after})"
            )
    print()

    # ── event mix, as a cheap sanity check on the stream itself ───────
    kinds: dict[str, int] = defaultdict(int)
    for _ts, e in events:
        kinds[e["event"]] += 1
    print("── event mix ──")
    for kind, n in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f"  {n:7d}  {kind}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
