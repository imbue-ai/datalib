#!/usr/bin/env python3
"""The qmd model pins exist twice; make them agree.

`third-party/qmd_models/BUILD.bazel` fetches the build's copies of the
three GGUFs (repo, revision, file, sha256), and
`datalib/backend/runtime/src/qmd.rs` carries the same table for the
shipped binaries, which provision and verify the files at run time.
Bazel cannot read a Rust constant and Rust cannot read Starlark, so the
table is written down in both — and a bump that moves one of them
leaves the fixture embedding with one model while every user's sync
verifies against another. Compare the two as sets of rows.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

BUILD = Path("third-party/qmd_models/BUILD.bazel")
RUST = Path("datalib/backend/runtime/src/qmd.rs")

Row = tuple[str, str, str, str]

# One 5-tuple of the Starlark `MODELS` list: (short, repo, rev, file, sha).
BUILD_ROW = re.compile(
    r'\(\s*"[^"]+",\s*"(?P<repo>[^"]+)",\s*"(?P<rev>[^"]+)",\s*"(?P<file>[^"]+)",\s*"(?P<sha>[^"]+)",?\s*\)',
    re.DOTALL,
)
# One `PinnedModel { repo: "...", revision: "...", file: "...", sha256: "..." }`.
RUST_ROW = re.compile(
    r'PinnedModel\s*\{\s*repo:\s*"(?P<repo>[^"]+)",\s*revision:\s*"(?P<rev>[^"]+)",'
    r'\s*file:\s*"(?P<file>[^"]+)",\s*sha256:\s*"(?P<sha>[^"]+)",?\s*\}',
    re.DOTALL,
)


def _rows(text: str, rx: re.Pattern[str]) -> set[Row]:
    return {(m["repo"], m["rev"], m["file"], m["sha"]) for m in rx.finditer(text)}


def main() -> int:
    root = Path.cwd()
    if not (root / "MODULE.bazel").is_file():
        root = Path(__file__).resolve().parent.parent
    build = _rows((root / BUILD).read_text(), BUILD_ROW)
    rust = _rows((root / RUST).read_text(), RUST_ROW)
    if not build or not rust:
        print(
            f"ERROR: parsed {len(build)} rows from {BUILD} and {len(rust)} from {RUST}; "
            "a table changed shape and is no longer being checked.",
            file=sys.stderr,
        )
        return 1
    if build == rust:
        print(f"OK: {len(rust)} qmd model pins agree between {BUILD} and {RUST}.")
        return 0
    for row in sorted(build - rust):
        print(f"only in {BUILD}: {row}", file=sys.stderr)
    for row in sorted(rust - build):
        print(f"only in {RUST}: {row}", file=sys.stderr)
    print(
        "\nThe build fetches one set of models and the shipped binaries verify "
        "against another. Change both files in the same commit.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
