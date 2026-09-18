#!/usr/bin/env python3
"""The license allow list exists twice; make the notices one cover the gate.

`datalib/backend/deny.toml` is the gate: `cargo deny` fails a PR on a
crate whose license is not in its `allow`. `datalib/backend/about.toml`
is what `cargo about` accepts when it writes the release's
`licenses/rust-crates.md`, and `--fail` makes it refuse a crate outside
its `accepted`. A license added to the gate and not to the notices
config passes every PR and breaks the next release, so `accepted` must
be a superset of `allow`. The tauri shell's deny.toml is held to the
same list, since the notices script covers only the backend workspace
and the shell links a subset of the same crates.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

DENY = [Path("datalib/backend/deny.toml"), Path("datalib/tauri/deny.toml")]
ABOUT = Path("datalib/backend/about.toml")


def main() -> int:
    accepted = set(tomllib.loads(ABOUT.read_text())["accepted"])
    failed = False
    for deny in DENY:
        allowed = set(tomllib.loads(deny.read_text())["licenses"]["allow"])
        missing = sorted(allowed - accepted)
        if missing:
            failed = True
            print(
                f"{ABOUT} accepts none of {missing}, which {deny} allows",
                file=sys.stderr,
            )
    if failed:
        return 1
    print(f"{ABOUT}: accepted covers every deny.toml allow list")
    return 0


if __name__ == "__main__":
    sys.exit(main())
