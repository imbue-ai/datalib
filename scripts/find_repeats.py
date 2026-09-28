#!/usr/bin/env python3
"""Find prose that appears in more than one place in the docs.

    scripts/find_repeats.py [--min 14] [--paths docs AGENTS.md] [--top N]

Reads every tracked `*.md` outside code fences and reports each run of
at least `--min` words (lowercased, punctuation dropped) that occurs
twice or more, longest first, with every place it occurs. Comparing
words rather than lines is the point: a paragraph copied and then
re-wrapped still matches, which a line-based clone detector misses.
For code, use one: `npx jscpd@4.0.5 --min-lines 10 datalib`.

A report is a list of candidates, not a verdict: a sentence a user doc
repeats so it can stand alone may be right where it is. Vendored code,
the audit and plan records and the fixtures are skipped.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

SKIP_PREFIXES = (
    "third-party/",
    "docs/dev/audits/",
    "docs/dev/plans/",
    "tests/fixtures/",
)
MAX_OCCURRENCES = 40  # a window seen more often than this is boilerplate, not a copy

WORD = re.compile(r"[a-z0-9_]+")

Span = tuple[str, int, int]
Finding = tuple[int, list[Span]]


@dataclass
class Doc:
    path: str
    words: list[str]
    lines: list[int]  # the source line of each word


def _tracked_markdown(root: Path, paths: list[str]) -> list[str]:
    specs = [p if p.endswith(".md") else f"{p.rstrip('/')}/**/*.md" for p in paths]
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", *(specs or ["*.md"])],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return sorted(p for p in out.split("\0") if p and not p.startswith(SKIP_PREFIXES))


def _read(root: Path, path: str) -> Doc:
    words: list[str] = []
    lines: list[int] = []
    in_fence = False
    for n, line in enumerate((root / path).read_text(errors="replace").splitlines(), 1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        for word in WORD.findall(line.lower()):
            words.append(word)
            lines.append(n)
    return Doc(path, words, lines)


def find_repeats(docs: list[Doc], window: int) -> list[Finding]:
    """Every maximal run of at least `window` words that occurs more than
    once, as (length, [(path, first line, last line), …])."""
    seen: dict[tuple[str, ...], list[tuple[int, int]]] = defaultdict(list)
    for d, doc in enumerate(docs):
        for i in range(len(doc.words) - window + 1):
            seen[tuple(doc.words[i : i + window])].append((d, i))

    runs: dict[tuple[str, ...], set[tuple[int, int]]] = defaultdict(set)
    for places in seen.values():
        if not 1 < len(places) <= MAX_OCCURRENCES:
            continue
        for a in range(len(places)):
            for b in range(a + 1, len(places)):
                (da, ia), (db, ib) = places[a], places[b]
                wa, wb = docs[da].words, docs[db].words
                if ia > 0 and ib > 0 and wa[ia - 1] == wb[ib - 1]:
                    continue  # inside a longer run, which another pair starts
                n = window
                while (
                    ia + n < len(wa) and ib + n < len(wb) and wa[ia + n] == wb[ib + n]
                ):
                    n += 1
                if da == db and ia + n > ib:
                    continue  # a run overlapping itself, e.g. a repeated table row
                runs[tuple(wa[ia : ia + n])] |= {(da, ia), (db, ib)}

    report: list[Finding] = []
    for key, places in runs.items():
        n = len(key)
        spans = sorted(
            (docs[d].path, docs[d].lines[i], docs[d].lines[i + n - 1])
            for d, i in places
        )
        report.append((n, spans))
    report.sort(key=lambda r: (-r[0], r[1]))
    return _drop_nested(report)


def _drop_nested(report: list[Finding]) -> list[Finding]:
    """Drop a finding whose every span sits inside a longer finding's."""
    kept: list[Finding] = []
    covered: dict[str, list[tuple[int, int]]] = defaultdict(list)
    for n, spans in report:
        if all(any(lo <= a and b <= hi for lo, hi in covered[p]) for p, a, b in spans):
            continue
        kept.append((n, spans))
        for path, a, b in spans:
            covered[path].append((a, b))
    return kept


def main() -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").split("\n\n")[0])
    parser.add_argument(
        "--min", type=int, default=14, help="shortest run to report, in words"
    )
    parser.add_argument(
        "--paths", nargs="*", default=[], help="limit to these files or dirs"
    )
    parser.add_argument("--top", type=int, default=0, help="print at most this many")
    args = parser.parse_args()

    root = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    )
    docs = [_read(root, p) for p in _tracked_markdown(root, args.paths)]
    report = find_repeats(docs, args.min)
    for n, spans in report[: args.top or None]:
        print(f"{n} words, {len(spans)} places:")
        for path, a, b in spans:
            print(f"  {path}:{a}-{b}")
    print(f"{len(report)} repeated runs of {args.min}+ words.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
