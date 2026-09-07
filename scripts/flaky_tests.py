#!/usr/bin/env python3
"""Which tests have been flaking, from CI reruns.

Hitting "re-run failed jobs" replays the *same commit*, so a commit
carrying both a failure and a success flaked — nothing about the code
changed between the two. This groups runs by commit, keeps the ones
with mixed outcomes, and reads the failed attempt's log to name the
bazel targets, so the answer is "//datalib/ui:e2e_test, 3 times" rather
than "CI was red a few times".

    scripts/flaky_tests.py [--limit 300] [--repo owner/name] [--json]

Two blind spots: it only sees flakes somebody actually re-ran (a red PR
that got an empty commit pushed at it instead is invisible), and GitHub
deletes run logs after 90 days, after which the episode still counts but
the target names are gone.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import zipfile
from collections import defaultdict
from io import BytesIO
from pathlib import Path

CACHE = Path.home() / ".cache" / "datalib-flaky-tests"
FIELDS = "databaseId,workflowName,headSha,headBranch,conclusion,attempt,createdAt,url"
TARGET = re.compile(r"(//\S+)\s+(?:FAILED|TIMEOUT|FLAKY)\b")
INVOCATION = re.compile(r"https://\S*buildbuddy\S*?/invocation/[0-9a-f-]+")


def gh(args: list[str], *, binary: bool = False) -> bytes | str | None:
    proc = subprocess.run(
        ["gh", *args], capture_output=True, text=not binary, check=False
    )
    return None if proc.returncode else proc.stdout


def failed_attempts(runs: list[dict]) -> list[tuple[dict, int]]:
    """The (run, attempt) pairs that failed on a commit that also went green."""
    by_commit: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for run in runs:
        by_commit[(run["workflowName"], run["headSha"])].append(run)

    out = []
    for group in by_commit.values():
        outcomes = {r["conclusion"] for r in group}
        for run in group:
            # A rerun holds both outcomes by itself: the attempts before
            # the last one are exactly what somebody re-ran, i.e. failures.
            if run["conclusion"] == "success" and run["attempt"] > 1:
                out += [(run, a) for a in range(1, run["attempt"])]
            elif run["conclusion"] == "failure" and "success" in outcomes:
                out.append((run, run["attempt"]))
    return out


def read_logs(repo: str, run_id: int, attempt: int) -> list[str]:
    """One log per job in the attempt; empty once GitHub has expired them."""
    cached = CACHE / f"{run_id}-{attempt}.zip"
    if not cached.exists():
        blob = gh(
            ["api", f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/logs"],
            binary=True,
        )
        if not isinstance(blob, bytes):
            return []
        CACHE.mkdir(parents=True, exist_ok=True)
        cached.write_bytes(blob)
    with zipfile.ZipFile(BytesIO(cached.read_bytes())) as zf:
        # Top-level entries are the per-job logs; subdirectories repeat
        # them step by step.
        return [
            zf.read(n).decode("utf-8", "replace")
            for n in zf.namelist()
            if "/" not in n and n.endswith(".txt")
        ]


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--limit", type=int, default=300, help="runs to scan")
    ap.add_argument("--repo", help="owner/name (default: this checkout's remote)")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args()

    scope = ["--repo", args.repo] if args.repo else []
    listing = gh(["run", "list", "--limit", str(args.limit), "--json", FIELDS, *scope])
    query = ["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]
    name = args.repo or gh(query)
    if not isinstance(listing, str) or not isinstance(name, str):
        print("ERROR: `gh` failed. Installed, authenticated, inside a repo?")
        return 2
    repo = name.strip()

    episodes = failed_attempts(json.loads(listing))
    hits: dict[str, list[dict]] = defaultdict(list)
    unnamed = 0
    for run, attempt in episodes:
        named = False
        for log in read_logs(repo, run["databaseId"], attempt):
            # Take the invocation link from the same job log as the
            # failures, or a green sibling job's link lands on the row.
            for target in sorted(set(TARGET.findall(log))):
                named = True
                hits[target].append(
                    {
                        "when": run["createdAt"][:10],
                        "commit": run["headSha"][:8],
                        "branch": run["headBranch"],
                        "run": f"{run['url']}/attempts/{attempt}",
                        "buildbuddy": (INVOCATION.findall(log) or [""])[0],
                    }
                )
        unnamed += not named

    if args.json:
        print(json.dumps(hits, indent=2))
        return 0

    print(f"Flaky tests — {repo}, last {args.limit} runs")
    print("(failed and then passed on the same commit)\n")
    for target, when in sorted(hits.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        print(f"  {len(when)}x  {target}")
        for hit in when:
            print(f"        {hit['when']}  {hit['commit']}  {hit['branch']}")
            print(f"          {hit['run']}")
            print(f"          {hit['buildbuddy']}")
        print()
    print(
        f"  {len(episodes)} flaky episode(s), {len(hits)} target(s)"
        f"{f', {unnamed} with no target name (log expired?)' if unnamed else ''}."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
