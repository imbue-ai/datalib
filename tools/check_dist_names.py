#!/usr/bin/env python3
"""The shipped-binary list exists twice; make them agree.

`//datalib/backend:bin` renames each Bazel output to its public
`datalib-<name>` form via `replace_prefixes`, and `release.yml`'s stage
step performs the *same* rename by hand when it builds the release
tarball. Both files say "keep this in sync with the other" in a comment,
and nothing checked it.

That drifted the first time it was exercised: `datalib-dirtree-diff` was
added to `:dist` and to `replace_prefixes` but not to `release.yml`, so
`bazel build //datalib/backend:bin` produced it locally while a tagged
release would have shipped a tarball without it. Nothing would have
failed — the release just quietly lacks a binary, which is the kind of
gap you find from a bug report months later.

This compares the two lists directly.

One wrinkle: not every shipped binary is built under `datalib/backend/`.
The doltlite shell is third-party C at `//third-party/doltlite:doltlite`,
which `:bin` reaches by listing a second entry in `root_paths`. So the
Bazel-side key is `doltlite/doltlite` where release.yml writes the full
`third-party/doltlite/doltlite`. We read `root_paths` out of the same
BUILD file and strip whichever one matches, rather than hardcoding a
prefix that only holds for the Rust binaries.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

BUILD = Path("datalib/backend/BUILD.bazel")
RELEASE = Path(".github/workflows/release.yml")

# `"dag/datalib_dag_bin": "datalib-dag",` inside the replace_prefixes map.
BAZEL_RENAME = re.compile(r'"(?P<out>[^"]+)":\s*"(?P<public>[^"]+)"')
# `root_paths = [` … `"datalib/backend",` … `]` on the same rule.
ROOT_PATH = re.compile(r'"(?P<path>[^"]+)"')
# `cp bazel-bin/datalib/backend/dag/datalib_dag_bin "${stage}/datalib-dag"`
RELEASE_COPY = re.compile(
    r'cp\s+bazel-bin/(?P<out>\S+)\s+"\$\{stage\}/(?P<public>[^"]+)"'
)


def _block(text: str, opener: str) -> str:
    """The text between `opener` and its closing bracket."""
    start = text.index(opener)
    end = text.index("]" if opener.endswith("[") else "}", start)
    return text[start:end]


def bazel_names(root: Path) -> dict[str, str]:
    text = (root / BUILD).read_text()
    return {
        m.group("public"): m.group("out")
        for m in BAZEL_RENAME.finditer(_block(text, "replace_prefixes = {"))
    }


def bazel_root_paths(root: Path) -> list[str]:
    text = (root / BUILD).read_text()
    return [m.group("path") for m in ROOT_PATH.finditer(_block(text, "root_paths = ["))]


def release_names(root: Path, root_paths: list[str]) -> dict[str, str]:
    """Public name -> bazel-bin path with the matching root_path stripped.

    Stripped so the value is directly comparable to the Bazel-side
    `replace_prefixes` key, which is always relative to a root_path.
    Longest match first, so a root that is a prefix of another can't win
    by ordering.
    """
    text = (root / RELEASE).read_text()
    names: dict[str, str] = {}
    for m in RELEASE_COPY.finditer(text):
        out = m.group("out")
        for prefix in sorted(root_paths, key=len, reverse=True):
            if out.startswith(prefix + "/"):
                out = out[len(prefix) + 1 :]
                break
        names[m.group("public")] = out
    return names


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    bazel = bazel_names(root)
    release = release_names(root, bazel_root_paths(root))

    problems: list[str] = []
    for public in sorted(set(bazel) - set(release)):
        problems.append(
            f"  {public!r} is renamed by //datalib/backend:bin but never "
            f"staged by release.yml — a tagged release would omit it"
        )
    for public in sorted(set(release) - set(bazel)):
        problems.append(
            f"  {public!r} is staged by release.yml but not renamed by "
            f"//datalib/backend:bin"
        )
    for public in sorted(set(bazel) & set(release)):
        if bazel[public] != release[public]:
            problems.append(
                f"  {public!r} comes from {bazel[public]!r} in "
                f"//datalib/backend:bin but {release[public]!r} in release.yml"
            )

    if problems:
        print(
            "ERROR: the shipped-binary lists disagree.\n\n".join([""] + problems)
            + "\n\n"
            f"Fix by editing both {BUILD} (the `replace_prefixes` map on\n"
            f"`:bin`, plus the `:dist` filegroup above it) and {RELEASE}\n"
            '(the `cp bazel-bin/... "${stage}/..."` block).',
            file=sys.stderr,
        )
        return 1

    print(f"OK: {len(bazel)} shipped binaries, named identically in both places.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
