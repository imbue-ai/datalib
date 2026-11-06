#!/usr/bin/env python3
"""Unpack what release.yml's compile job made, for the job holding the signing key.

The compile job runs every crate's build script, so nothing in its archive can
be trusted. Unpacked as it came, one entry could replace a script the signing
job runs next. This takes only the paths the bundle needs, as plain files and
directories, and places them only after checking the whole archive.

    unpack_compiled_app.py <archive> <dest>

Runs under the macOS system Python, 3.9 at the time of writing.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tarfile
import tempfile
from collections.abc import Iterable
from pathlib import Path

ALLOWED_TREES = ("binaries", "licenses", "runtime")
ALLOWED_FILE = "target/release/datalib-tauri"
EVERY_MODE_BIT_ALLOWED = 0o755


def member_problem(name: str, is_file: bool, is_dir: bool) -> str | None:
    if not (is_file or is_dir):
        return f"{name}: not a plain file or directory"
    parts = name.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return f"{name}: not a plain relative path"
    if name == ALLOWED_FILE:
        return None if is_file else f"{name}: must be a plain file"
    if parts[0] not in ALLOWED_TREES:
        return f"{name}: not something the bundle takes"
    return None


def archive_problems(members: Iterable[tuple[str, bool, bool]]) -> list[str]:
    """Every reason to refuse an archive, given each member's name and kind."""
    problems: list[str] = []
    seen: set[str] = set()
    for name, is_file, is_dir in members:
        problem = member_problem(name, is_file, is_dir)
        if problem:
            problems.append(problem)
        if name in seen:
            problems.append(f"{name}: appears twice")
        seen.add(name)
    tops = {name.split("/")[0] for name in seen}
    for tree in ALLOWED_TREES:
        if tree not in tops:
            problems.append(f"{tree}: missing")
    if ALLOWED_FILE not in seen:
        problems.append(f"{ALLOWED_FILE}: missing")
    return problems


def extract(archive: Path, into: Path) -> None:
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            target = into / member.name
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            source = tar.extractfile(member)
            if source is None:
                raise OSError(f"{member.name}: no contents")
            target.parent.mkdir(parents=True, exist_ok=True)
            # O_NOFOLLOW and O_EXCL: this creates a new file or fails.
            fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW)
            with os.fdopen(fd, "wb") as out:
                shutil.copyfileobj(source, out, 1024 * 1024)
                os.fchmod(out.fileno(), member.mode & EVERY_MODE_BIT_ALLOWED)


def unpack(archive: Path, dest: Path) -> list[str]:
    """Place the archive's trees in `dest`; on any problem, place nothing and say why."""
    with tarfile.open(archive, "r:gz") as tar:
        problems = archive_problems(
            (member.name, member.isfile(), member.isdir()) for member in tar
        )
    tops = [*ALLOWED_TREES, ALLOWED_FILE.split("/")[0]]
    problems += [
        f"{top}: already in {dest}" for top in tops if os.path.lexists(dest / top)
    ]
    if problems:
        return problems
    with tempfile.TemporaryDirectory(dir=archive.parent) as scratch:
        extract(archive, Path(scratch))
        for top in tops:
            shutil.move(str(Path(scratch) / top), str(dest / top))
    return []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("dest", type=Path)
    args = parser.parse_args()
    problems = unpack(args.archive, args.dest)
    if problems:
        print(f"ERROR: refusing {args.archive}:", file=sys.stderr)
        for problem in problems[:50]:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"Unpacked {args.archive} into {args.dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
