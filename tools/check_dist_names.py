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
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

BUILD = Path("datalib/backend/BUILD.bazel")
RELEASE = Path(".github/workflows/release.yml")

# `"dag/datalib_dag_bin": "datalib-dag",` inside the replace_prefixes map.
BAZEL_RENAME = re.compile(r'"(?P<out>[^"]+)":\s*"(?P<public>[^"]+)"')
# `cp bazel-bin/datalib/backend/dag/datalib_dag_bin "${stage}/datalib-dag"`
RELEASE_COPY = re.compile(
    r'cp\s+bazel-bin/datalib/backend/(?P<out>\S+)\s+"\$\{stage\}/(?P<public>[^"]+)"'
)


def bazel_names(root: Path) -> dict[str, str]:
    text = (root / BUILD).read_text()
    start = text.index("replace_prefixes = {")
    end = text.index("}", start)
    return {
        m.group("public"): m.group("out")
        for m in BAZEL_RENAME.finditer(text[start:end])
    }


def release_names(root: Path) -> dict[str, str]:
    text = (root / RELEASE).read_text()
    return {m.group("public"): m.group("out") for m in RELEASE_COPY.finditer(text)}


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    bazel = bazel_names(root)
    release = release_names(root)

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
